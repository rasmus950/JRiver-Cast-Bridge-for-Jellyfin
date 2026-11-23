//! The bridge's own record of the film it plays (SPEC.md "The bridge
//! killed"). The server forgets a cast target's film as soon as its socket
//! closes, so after a crash or a kill this record is what tells the next
//! bridge which film MC may still be playing and where it was.
//!
//! The file is `%APPDATA%\jriver-cast-bridge\playback-<account>.json`. It's
//! written when a film starts and about once a second while it plays, and
//! deleted when the playback ends. It holds no secrets. Writes run on a
//! background thread, in order, so a slow disk never holds up the bridge.

use std::{
  future::Future,
  io,
  path::{Path, PathBuf},
  pin::Pin,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use log::{debug, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

pub type BoxFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The film the bridge was playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackRecord {
  pub item_id: String,
  pub media_source_id: Option<String>,
  pub play_session_id: String,
  pub name: String,
  /// The path MC was given.
  pub path: String,
  /// The last position polled while it played.
  pub position_ms: u64,
  pub paused: bool,
  /// When it was written (seconds since 1970, UTC).
  #[serde(default)]
  pub written_at: u64,
  /// The audio and subtitle stream in use (Jellyfin's indexes), so a
  /// takeover keeps reporting them. Absent in records before 0.7.0.
  #[serde(default)]
  pub audio_stream_index: Option<i32>,
  #[serde(default)]
  pub subtitle_stream_index: Option<i32>,
}

impl PlaybackRecord {
  /// How long ago it was written (zero if the clock went back).
  pub fn age(&self) -> Duration {
    Duration::from_secs(now_secs().saturating_sub(self.written_at))
  }
}

pub fn now_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |since| since.as_secs())
}

/// What a bridge found on start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
  /// Recorded under this Jellyfin user.
  Mine(PlaybackRecord),
  /// Recorded under another user of the same account (a new login with a
  /// different user name): it can't be reported any more.
  OtherUser(PlaybackRecord),
}

impl Found {
  pub fn record(&self) -> &PlaybackRecord {
    match self {
      Self::Mine(record) | Self::OtherUser(record) => record,
    }
  }
}

/// Where the record is kept. Failures are logged here, not returned: a
/// record that can't be written only costs the takeover after a kill.
pub trait RecordStore: Send + Sync + 'static {
  /// Read the record. It may block: call it off the async runtime.
  fn load(&self) -> Option<Found>;
  /// Replace the record (queued; doesn't block).
  fn save(&self, record: &PlaybackRecord);
  /// Delete the record (queued, in order with saves; doesn't block).
  fn clear(&self);
  /// Wait until the queued saves and deletes are done.
  fn flush(&self) -> BoxFuture<'_>;
}

/// Kept in memory only: nothing survives the process (tests, or when the
/// app folder isn't known).
#[derive(Debug, Default)]
pub struct MemoryRecord(Mutex<Option<PlaybackRecord>>);

impl MemoryRecord {
  /// The record as it stands, for tests.
  pub fn get(&self) -> Option<PlaybackRecord> {
    self.0.lock().unwrap().clone()
  }
}

impl RecordStore for MemoryRecord {
  fn load(&self) -> Option<Found> {
    self.get().map(Found::Mine)
  }
  fn save(&self, record: &PlaybackRecord) {
    *self.0.lock().unwrap() = Some(record.clone());
  }
  fn clear(&self) {
    *self.0.lock().unwrap() = None;
  }
  fn flush(&self) -> BoxFuture<'_> {
    Box::pin(async {})
  }
}

#[derive(Debug, Serialize, Deserialize)]
struct OnDisk {
  /// `profile@server URL`.
  account: String,
  user_id: String,
  #[serde(flatten)]
  record: PlaybackRecord,
}

/// A queued write: each one replaces the file's whole state, so only the
/// latest matters.
#[derive(Debug, Clone)]
enum Op {
  Save(Vec<u8>),
  Clear,
}

#[derive(Debug)]
struct Disk {
  path: PathBuf,
  /// A failed write is a warning once, then debug, until one succeeds.
  failing: AtomicBool,
}

impl Disk {
  fn apply(&self, op: &Op) {
    let result = match op {
      Op::Save(bytes) => self.write(bytes),
      Op::Clear => self.remove(),
    };
    match result {
      Ok(()) => {
        if self.failing.swap(false, Ordering::Relaxed) {
          debug!("the playback record is written again");
        }
      }
      Err(err) => {
        if self.failing.swap(true, Ordering::Relaxed) {
          debug!("writing the playback record failed again: {err}");
        } else {
          warn!(
            "can't update the playback record {} ({err}); a killed bridge may not take this film over",
            self.path.display()
          );
        }
      }
    }
  }

  fn write(&self, bytes: &[u8]) -> io::Result<()> {
    // Write beside it and rename, so a kill mid-write leaves the old record.
    let tmp = self.path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &self.path)
  }

  fn remove(&self) -> io::Result<()> {
    match std::fs::remove_file(&self.path) {
      Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
      _ => Ok(()),
    }
  }
}

/// The record file of one account and Jellyfin user.
#[derive(Debug)]
pub struct RecordFile {
  disk: Arc<Disk>,
  account: String,
  user_id: String,
  /// Latest queued write, with its sequence number.
  queue: watch::Sender<(u64, Option<Op>)>,
  /// Sequence number of the last write done.
  done: watch::Receiver<u64>,
}

/// `playback-<account>.json`, the account made safe for a file name, so
/// each config (e.g. a test profile's) has its own record.
pub fn file_name(account: &str) -> String {
  let safe: String = account
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
    .collect();
  format!("playback-{safe}.json")
}

impl RecordFile {
  /// The record in `dir`. Starts the writer thread's task, so it must be
  /// called inside the Tokio runtime.
  pub fn new(dir: &Path, account: &str, user_id: &str) -> Self {
    let disk = Arc::new(Disk {
      path: dir.join(file_name(account)),
      failing: AtomicBool::new(false),
    });
    let (queue, mut queued) = watch::channel((0, None::<Op>));
    let (done_tx, done) = watch::channel(0u64);
    let writer = disk.clone();
    tokio::spawn(async move {
      // Ends once the RecordFile is gone and the last write is done.
      while queued.changed().await.is_ok() {
        let (seq, op) = queued.borrow_and_update().clone();
        if let Some(op) = op {
          let writer = writer.clone();
          if let Err(err) = tokio::task::spawn_blocking(move || writer.apply(&op)).await {
            warn!("the playback record writer failed: {err}");
          }
        }
        done_tx.send_replace(seq);
      }
    });
    Self {
      disk,
      account: account.to_owned(),
      user_id: user_id.to_owned(),
      queue,
      done,
    }
  }

  fn queue(&self, op: Op) {
    // Number and publish in one step, so writes queued from two threads
    // can't land out of order.
    self.queue.send_modify(|(seq, queued)| {
      *seq += 1;
      *queued = Some(op);
    });
  }

  fn read(&self) -> io::Result<Option<OnDisk>> {
    match std::fs::read(&self.disk.path) {
      Ok(bytes) => serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
      Err(err) => Err(err),
    }
  }
}

impl RecordStore for RecordFile {
  fn load(&self) -> Option<Found> {
    let on_disk = match self.read() {
      Ok(on_disk) => on_disk?,
      Err(err) => {
        warn!(
          "can't read the playback record {} ({err}); deleting it",
          self.disk.path.display()
        );
        self.clear();
        return None;
      }
    };
    if on_disk.account != self.account {
      warn!(
        "the playback record {} belongs to {}, not {}; ignoring it",
        self.disk.path.display(),
        on_disk.account,
        self.account
      );
      return None;
    }
    Some(if on_disk.user_id == self.user_id {
      Found::Mine(on_disk.record)
    } else {
      Found::OtherUser(on_disk.record)
    })
  }

  fn save(&self, record: &PlaybackRecord) {
    let on_disk = OnDisk {
      account: self.account.clone(),
      user_id: self.user_id.clone(),
      record: record.clone(),
    };
    match serde_json::to_vec_pretty(&on_disk) {
      Ok(bytes) => self.queue(Op::Save(bytes)),
      Err(err) => warn!("can't encode the playback record: {err}"),
    }
  }

  fn clear(&self) {
    self.queue(Op::Clear);
  }

  fn flush(&self) -> BoxFuture<'_> {
    let target = self.queue.borrow().0;
    let mut done = self.done.clone();
    Box::pin(async move {
      // Fails only if the writer is gone, and then nothing is left to wait for.
      let _ = done.wait_for(|seq| *seq >= target).await;
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn record(position_ms: u64) -> PlaybackRecord {
    PlaybackRecord {
      item_id: "item".to_owned(),
      media_source_id: Some("source".to_owned()),
      play_session_id: "session".to_owned(),
      name: "Avatar".to_owned(),
      path: r"\\server\movies\avatar.mkv".to_owned(),
      position_ms,
      paused: false,
      // Fixed, so two calls give equal records whatever the clock does.
      written_at: 1_795_000_000,
      audio_stream_index: Some(3),
      subtitle_stream_index: None,
    }
  }

  fn dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-record-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }

  const ACCOUNT: &str = "default@http://jf:8096";

  #[tokio::test]
  async fn the_record_survives_the_process_and_writes_land_in_order() {
    let dir = dir();
    let path = dir.join(file_name(ACCOUNT));
    assert!(path.ends_with("playback-default_http___jf_8096.json"));
    let file = RecordFile::new(&dir, ACCOUNT, "user1");
    assert_eq!(file.load(), None);
    file.save(&record(1_000));
    file.save(&record(2_000));
    file.flush().await;
    // A new bridge process, same account and user.
    let next = RecordFile::new(&dir, ACCOUNT, "user1");
    assert_eq!(next.load(), Some(Found::Mine(record(2_000))));
    assert_eq!(
      next.load(),
      Some(Found::Mine(record(2_000))),
      "reading keeps it"
    );
    // A delete queued after a save wins, however fast they come.
    file.save(&record(3_000));
    file.clear();
    file.flush().await;
    assert!(!path.exists());
    assert_eq!(next.load(), None);
    file.clear();
    file.flush().await;
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn another_user_is_told_apart_and_another_account_has_its_own_file() {
    let dir = dir();
    let file = RecordFile::new(&dir, ACCOUNT, "user1");
    file.save(&record(1_000));
    file.flush().await;
    let other_account = RecordFile::new(&dir, "verifier@http://jf:8096", "user1");
    assert_eq!(other_account.load(), None);
    let other_user = RecordFile::new(&dir, ACCOUNT, "user2");
    assert_eq!(other_user.load(), Some(Found::OtherUser(record(1_000))));
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn a_damaged_record_is_deleted() {
    let dir = dir();
    let path = dir.join(file_name(ACCOUNT));
    std::fs::write(&path, b"{ not json").unwrap();
    let file = RecordFile::new(&dir, ACCOUNT, "user1");
    assert_eq!(file.load(), None);
    file.flush().await;
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn age_counts_from_the_write() {
    let mut old = record(0);
    old.written_at -= 600;
    assert!(old.age() >= Duration::from_secs(600));
    old.written_at = now_secs() + 100;
    assert_eq!(old.age(), Duration::ZERO);
  }
}
