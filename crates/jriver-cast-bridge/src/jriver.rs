//! JRiver as the bridge's player: [`JRiverPlayer`] plus launching MC when it
//! isn't running (SPEC.md "JRiver not running": launch MC from the
//! configured exe path, wait for `Alive`, then play).

use std::{
  future::Future,
  path::{Path, PathBuf},
  sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, PoisonError,
  },
  time::Duration,
};

use jriver_mcws::{JRiverPlayer, Mcws, McwsError, PlaybackInfo, StartError, StartedPlayback};
use log::{debug, info, warn};
use tokio::{
  sync::watch,
  time::{sleep, Instant},
};

use crate::{
  controller::Player,
  path_map::{check_exists, ExistsError},
  process,
  status::JRiverNotice,
  tracks::TrackKind,
};

/// How often to ask `Alive` while MC starts.
const ALIVE_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
  /// `Media Center 36.exe`. `None`: never launch MC.
  pub exe_path: Option<PathBuf>,
  /// From launching MC until its Media Network answers.
  pub timeout: Duration,
}

impl Default for LaunchOptions {
  fn default() -> Self {
    Self {
      exe_path: None,
      timeout: Duration::from_secs(60),
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
  #[error("JRiver isn't running and no mcws.exe_path is configured to start it ({0})")]
  NotRunning(#[source] McwsError),
  #[error("couldn't start JRiver from {path}: {source}")]
  Spawn {
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },
  #[error(
    "started JRiver, but its Media Network didn't answer within {0:?} (is Media Network on?)"
  )]
  Timeout(Duration),
  #[error(transparent)]
  Mcws(McwsError),
  #[error("JRiver is hung, but no mcws.exe_path is configured to restart it")]
  NoExePath,
  #[error("couldn't close the hung JRiver: {0}")]
  Kill(#[source] std::io::Error),
}

pub struct JRiver<M> {
  player: JRiverPlayer<M>,
  launch: LaunchOptions,
  /// When the bridge last started MC. A cast arriving while MC is still
  /// starting must wait for it, not start a second copy.
  launched_at: Mutex<Option<Instant>>,
  spawn: fn(&Path) -> std::io::Result<()>,
  not_responding_check: NotRespondingCheck,
  kill: Kill,
  /// Set while the bridge starts or restarts MC, for the on-screen notice.
  notice: watch::Sender<Option<JRiverNotice>>,
  /// The "Not responding" check failed last time (logged once).
  check_failing: AtomicBool,
}

impl<M: Mcws> JRiver<M> {
  pub fn new(player: JRiverPlayer<M>, launch: LaunchOptions) -> Self {
    Self {
      player,
      launch,
      launched_at: Mutex::new(None),
      spawn: spawn_detached,
      not_responding_check: real_not_responding,
      kill: real_kill,
      notice: watch::Sender::new(None),
      check_failing: AtomicBool::new(false),
    }
  }

  pub fn player(&self) -> &JRiverPlayer<M> {
    &self.player
  }

  /// Whether the bridge is starting or restarting MC right now.
  pub fn notices(&self) -> watch::Receiver<Option<JRiverNotice>> {
    self.notice.subscribe()
  }

  /// Show `notice` until the returned guard is dropped. An outer notice
  /// (a restart) is kept while an inner step (the start) runs.
  fn show(&self, notice: JRiverNotice) -> Shown<'_> {
    self.notice.send_if_modified(|current| {
      let set = current.is_none();
      if set {
        *current = Some(notice);
      }
      set
    });
    Shown(&self.notice)
  }

  /// `Ok(false)` if MC already answers, `Ok(true)` if it had to be started.
  ///
  /// MC is started only when its port refuses the connection. A timeout means
  /// MC is there but busy, and a second copy would not help.
  pub async fn ensure_running(&self) -> Result<bool, LaunchError> {
    let err = match self.player.alive().await {
      Ok(_) => return Ok(false),
      Err(err @ McwsError::Unreachable { .. }) => err,
      Err(err) => return Err(LaunchError::Mcws(err)),
    };
    let Some(exe) = &self.launch.exe_path else {
      return Err(LaunchError::NotRunning(err));
    };
    let _shown = self.show(JRiverNotice::Starting);
    let starting = {
      let mut launched_at = self
        .launched_at
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
      let starting = launched_at.is_some_and(|at| at.elapsed() < self.launch.timeout);
      if !starting {
        *launched_at = Some(Instant::now());
      }
      starting
    };
    if starting {
      info!("JRiver is still starting; waiting for it");
    } else {
      info!("JRiver isn't answering; starting {}", exe.display());
      (self.spawn)(exe).map_err(|source| LaunchError::Spawn {
        path: exe.clone(),
        source,
      })?;
    }
    let deadline = Instant::now() + self.launch.timeout;
    loop {
      sleep(ALIVE_POLL).await;
      // MC's web server answers `Alive` before (and apart from) the rest
      // of MC, so it's up only once `Playback/Info` answers too. Until then
      // it's still starting, which the hang check leaves alone.
      let result = match self.player.alive().await {
        Ok(alive) => self.player.status().await.map(|_| alive),
        Err(err) => Err(err),
      };
      match result {
        Ok(alive) => {
          info!("JRiver {} answering after start", alive.program_version);
          *self
            .launched_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
          return Ok(true);
        }
        Err(err) if Instant::now() >= deadline => {
          warn!("JRiver: still not answering after start: {err}");
          return Err(LaunchError::Timeout(self.launch.timeout));
        }
        Err(_) => {}
      }
    }
  }
}

impl<M: Mcws> JRiver<M> {
  /// MC's image name (`Media Center 36.exe`), from the exe path.
  fn image(&self) -> Option<&str> {
    self.launch.exe_path.as_deref()?.file_name()?.to_str()
  }

  /// MC's processes in this session that Windows reports "Not responding".
  /// Empty without an exe path; `None` when `tasklist` failed.
  async fn query_hung(&self) -> Option<Vec<u32>> {
    let Some(image) = self.image() else {
      return Some(Vec::new());
    };
    match (self.not_responding_check)(image).await {
      Ok(pids) => {
        if self.check_failing.swap(false, Ordering::Relaxed) {
          info!("Windows answers about {image} again");
        }
        Some(pids)
      }
      Err(err) => {
        // Asked every second: warn once per run of failures.
        if self.check_failing.swap(true, Ordering::Relaxed) {
          debug!("still can't ask Windows whether {image} responds: {err}");
        } else {
          warn!("can't ask Windows whether {image} responds: {err}");
        }
        None
      }
    }
  }

  /// Does Windows report MC as "Not responding"? `None` when it can't tell
  /// (`tasklist` failed): not the same as "responds".
  pub async fn not_responding(&self) -> Option<bool> {
    self.query_hung().await.map(|pids| !pids.is_empty())
  }

  /// Close the hung MC by force (only its "Not responding" processes in
  /// this session), start it again and wait for `Alive` (SPEC.md "JRiver
  /// hung").
  pub async fn restart(&self) -> Result<(), LaunchError> {
    let _shown = self.show(JRiverNotice::Restarting);
    let image = self.image().ok_or(LaunchError::NoExePath)?.to_owned();
    // If Windows can't tell which process is hung, nothing is closed and
    // the restart fails: "starting" an MC that is still hung (its `Alive`
    // answers) would only look like a restart.
    let pids = self.query_hung().await.ok_or_else(|| {
      LaunchError::Kill(std::io::Error::other(format!(
        "can't ask Windows which {image} is hung"
      )))
    })?;
    if pids.is_empty() {
      info!("JRiver: {image} no longer shows as hung; not closing it");
    } else {
      (self.kill)(pids.clone()).await.map_err(LaunchError::Kill)?;
      info!("JRiver: closed hung {image} (PID {pids:?}); starting it again");
    }
    *self
      .launched_at
      .lock()
      .unwrap_or_else(PoisonError::into_inner) = None;
    self.ensure_running().await.map(drop)
  }
}

/// Clears the notice when the start or restart is over, however it ends.
struct Shown<'a>(&'a watch::Sender<Option<JRiverNotice>>);

impl Drop for Shown<'_> {
  fn drop(&mut self) {
    self.0.send_replace(None);
  }
}

type NotRespondingCheck = fn(&str) -> BoxFuture<'_, std::io::Result<Vec<u32>>>;
type Kill = fn(Vec<u32>) -> BoxFuture<'static, std::io::Result<()>>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn real_not_responding(image: &str) -> BoxFuture<'_, std::io::Result<Vec<u32>>> {
  Box::pin(process::not_responding(image))
}

fn real_kill(pids: Vec<u32>) -> BoxFuture<'static, std::io::Result<()>> {
  Box::pin(async move { process::kill(&pids).await })
}

/// Start MC as an independent process; the bridge doesn't wait for it or
/// own it.
fn spawn_detached(exe: &Path) -> std::io::Result<()> {
  let mut command = std::process::Command::new(exe);
  if let Some(dir) = exe.parent() {
    command.current_dir(dir);
  }
  command
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
  command.spawn().map(drop)
}

impl<M: Mcws + 'static> Player for JRiver<M> {
  fn check_path(
    &self,
    path: &str,
    timeout: Duration,
  ) -> impl Future<Output = Result<(), ExistsError>> + Send {
    check_exists(path, timeout)
  }
  fn ensure_running(&self) -> impl Future<Output = Result<bool, LaunchError>> + Send {
    JRiver::ensure_running(self)
  }
  fn play(
    &self,
    path: &str,
    start_ms: u64,
  ) -> impl Future<Output = Result<StartedPlayback, StartError>> + Send {
    self.player.play(path, start_ms)
  }
  fn pause(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    self.player.pause()
  }
  fn resume(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    self.player.resume()
  }
  fn toggle_pause(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    self.player.toggle_pause()
  }
  fn seek(&self, position_ms: u64) -> impl Future<Output = Result<u64, McwsError>> + Send {
    self.player.seek(position_ms)
  }
  fn stop(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    self.player.stop()
  }
  fn status(&self) -> impl Future<Output = Result<PlaybackInfo, McwsError>> + Send {
    self.player.status()
  }
  fn ui_mode(&self) -> impl Future<Output = Result<i32, McwsError>> + Send {
    self.player.ui_mode()
  }
  fn minimize(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    self.player.minimize()
  }
  async fn select_track(&self, kind: TrackKind, track: i32) -> Result<(), McwsError> {
    match kind {
      TrackKind::Audio => self.player.select_audio_stream(track).await,
      TrackKind::Subtitle => self.player.select_subtitles(track).await,
    }
  }
  async fn check_health(&self) -> Result<(), McwsError> {
    self.player.status().await.map(drop)
  }
  fn not_responding(&self) -> impl Future<Output = Option<bool>> + Send {
    JRiver::not_responding(self)
  }
  fn restart(&self) -> impl Future<Output = Result<(), LaunchError>> + Send {
    JRiver::restart(self)
  }
  fn launching(&self) -> bool {
    self
      .launched_at
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .is_some_and(|at| at.elapsed() < self.launch.timeout)
  }
}

#[cfg(test)]
mod tests {
  use std::{cell::Cell, sync::Mutex};

  use jriver_mcws::{AliveInfo, Function, PauseAction, PlayerOptions, UiInfo};

  use super::*;

  /// How `Alive` fails before it answers.
  #[derive(Clone, Copy)]
  enum Down {
    /// Connection refused (MC isn't running).
    Refused,
    /// No answer in time (MC busy).
    Slow,
  }

  /// `Alive` fails `count` times as `kind`, then answers.
  struct FakeMc {
    count: Mutex<u32>,
    kind: Down,
    /// `Playback/Info` times out while `Alive` answers (MC hung, or still
    /// loading after its web server came up).
    info_hangs: Mutex<bool>,
  }

  fn timeout() -> McwsError {
    McwsError::Timeout {
      function: Function::Alive,
      timeout: Duration::from_secs(5),
    }
  }

  /// A transport error without any network access: an unparsable URL.
  fn refused() -> McwsError {
    let source = reqwest::Client::new()
      .get("not a url")
      .build()
      .expect_err("an unparsable URL fails to build");
    McwsError::Unreachable {
      function: Function::Alive,
      source,
    }
  }

  impl Mcws for FakeMc {
    async fn alive(&self) -> Result<AliveInfo, McwsError> {
      let mut count = self.count.lock().unwrap();
      if *count > 0 {
        *count -= 1;
        return Err(match self.kind {
          Down::Refused => refused(),
          Down::Slow => timeout(),
        });
      }
      Ok(AliveInfo::default())
    }
    async fn minimize_window(&self) -> Result<(), McwsError> {
      Ok(())
    }
    async fn select_audio_stream(&self, _track: i32) -> Result<(), McwsError> {
      Ok(())
    }
    async fn select_subtitles(&self, _track: i32) -> Result<(), McwsError> {
      Ok(())
    }
    async fn info(&self) -> Result<PlaybackInfo, McwsError> {
      // Down like Alive, or hung apart from the web server.
      if *self.count.lock().unwrap() > 0 || *self.info_hangs.lock().unwrap() {
        return Err(timeout());
      }
      Ok(PlaybackInfo {
        state: jriver_mcws::PlayState::Stopped,
        file_key: -1,
        position_ms: 0,
        duration_ms: 0,
        playing_now_tracks: 0,
        filename: None,
        name: None,
      })
    }
    async fn ui_info(&self) -> Result<UiInfo, McwsError> {
      Err(timeout())
    }
    async fn play_by_filename(&self, _: &str) -> Result<(), McwsError> {
      Ok(())
    }
    async fn clear_playlist(&self) -> Result<(), McwsError> {
      Ok(())
    }
    async fn seek(&self, ms: u64) -> Result<u64, McwsError> {
      Ok(ms)
    }
    async fn pause(&self, _: PauseAction) -> Result<(), McwsError> {
      Ok(())
    }
    async fn stop(&self) -> Result<(), McwsError> {
      Ok(())
    }
  }

  thread_local! {
    static SPAWNS: Cell<u32> = const { Cell::new(0) };
  }

  fn fake_spawn(_: &Path) -> std::io::Result<()> {
    SPAWNS.with(|n| n.set(n.get() + 1));
    Ok(())
  }

  fn jriver(count: u32, kind: Down, exe_path: Option<PathBuf>) -> JRiver<FakeMc> {
    jriver_with(count, kind, exe_path, false)
  }

  /// `info_hangs`: `Playback/Info` keeps timing out after `Alive` answers.
  fn jriver_with(
    count: u32,
    kind: Down,
    exe_path: Option<PathBuf>,
    info_hangs: bool,
  ) -> JRiver<FakeMc> {
    let mut jriver = JRiver::new(
      JRiverPlayer::new(
        FakeMc {
          count: Mutex::new(count),
          kind,
          info_hangs: Mutex::new(info_hangs),
        },
        PlayerOptions::default(),
      ),
      LaunchOptions {
        exe_path,
        timeout: Duration::from_secs(5),
      },
    );
    jriver.spawn = fake_spawn;
    jriver
  }

  fn exe() -> Option<PathBuf> {
    Some(PathBuf::from(r"C:\MC\Media Center 36.exe"))
  }

  #[tokio::test(start_paused = true)]
  async fn running_mc_is_left_alone() {
    assert!(!jriver(0, Down::Refused, exe())
      .ensure_running()
      .await
      .unwrap());
    assert_eq!(SPAWNS.with(Cell::get), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_refused_connection_starts_mc_and_waits_for_it() {
    let j = jriver(3, Down::Refused, exe());
    let mut notices = j.notices();
    let (started, seen) = tokio::join!(j.ensure_running(), async {
      notices.changed().await.unwrap();
      *notices.borrow_and_update()
    });
    assert!(started.unwrap());
    assert_eq!(SPAWNS.with(Cell::get), 1);
    assert_eq!(seen, Some(JRiverNotice::Starting), "shown while MC starts");
    assert_eq!(*notices.borrow(), None, "and gone once it answers");
  }

  #[tokio::test(start_paused = true)]
  async fn the_health_check_sees_a_hung_mc_whose_web_server_still_answers() {
    // As seen on a real hang: Alive answers, Playback/Info times out.
    let j = jriver_with(0, Down::Refused, exe(), true);
    assert!(j.player().alive().await.is_ok());
    assert!(matches!(
      Player::check_health(&j).await,
      Err(McwsError::Timeout { .. })
    ));
  }

  #[tokio::test(start_paused = true)]
  async fn a_started_mc_counts_as_up_only_once_playback_info_answers() {
    // The web server answers, the rest of MC doesn't within the timeout:
    // still starting, then a failed launch, never "up".
    let j = jriver_with(2, Down::Refused, exe(), true);
    assert!(matches!(
      j.ensure_running().await,
      Err(LaunchError::Timeout(_))
    ));
    assert_eq!(SPAWNS.with(Cell::get), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn a_running_mc_shows_no_notice_and_a_failed_start_clears_it() {
    let j = jriver(0, Down::Refused, exe());
    let notices = j.notices();
    assert!(!j.ensure_running().await.unwrap());
    assert!(!notices.has_changed().unwrap());
    let j = jriver(u32::MAX, Down::Refused, exe());
    assert!(j.ensure_running().await.is_err());
    assert_eq!(*j.notices().borrow(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_busy_mc_is_not_started_again() {
    let result = jriver(1, Down::Slow, exe()).ensure_running().await;
    assert!(matches!(result, Err(LaunchError::Mcws(_))));
    assert_eq!(SPAWNS.with(Cell::get), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_second_cast_while_mc_starts_waits_instead_of_starting_it_again() {
    // MC never comes up within the timeout: the first call gives up, and a
    // second call inside the start window must not spawn again.
    let j = jriver(u32::MAX, Down::Refused, exe());
    assert!(matches!(
      j.ensure_running().await,
      Err(LaunchError::Timeout(_))
    ));
    *j.launched_at.lock().unwrap() = Some(Instant::now());
    assert!(matches!(
      j.ensure_running().await,
      Err(LaunchError::Timeout(_))
    ));
    assert_eq!(SPAWNS.with(Cell::get), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn without_an_exe_path_a_missing_mc_is_an_error() {
    assert!(matches!(
      jriver(1, Down::Refused, None).ensure_running().await,
      Err(LaunchError::NotRunning(_))
    ));
  }

  thread_local! {
    static KILLED: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
  }

  fn fake_kill(pids: Vec<u32>) -> BoxFuture<'static, std::io::Result<()>> {
    KILLED.with(|k| k.borrow_mut().extend(pids));
    Box::pin(async { Ok(()) })
  }

  fn fake_hung(_: &str) -> BoxFuture<'_, std::io::Result<Vec<u32>>> {
    Box::pin(async { Ok(vec![4242]) })
  }

  fn killed() -> Vec<u32> {
    KILLED.with(|k| k.borrow().clone())
  }

  #[tokio::test(start_paused = true)]
  async fn restart_closes_only_the_hung_process_then_starts_mc_and_waits() {
    // After the kill MC refuses connections until the new one is up.
    let mut j = jriver(2, Down::Refused, exe());
    j.kill = fake_kill;
    j.not_responding_check = fake_hung;
    // A launch just before must not stop the restart from starting MC.
    *j.launched_at.lock().unwrap() = Some(Instant::now());
    let mut notices = j.notices();
    let (restarted, seen) = tokio::join!(j.restart(), async {
      notices.changed().await.unwrap();
      *notices.borrow_and_update()
    });
    restarted.unwrap();
    assert_eq!(killed(), vec![4242]);
    assert_eq!(SPAWNS.with(Cell::get), 1);
    // "Restarting" stays up through the start that follows the kill.
    assert_eq!(seen, Some(JRiverNotice::Restarting));
    assert_eq!(*notices.borrow(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_restart_after_mc_recovered_closes_nothing() {
    let mut j = jriver(0, Down::Refused, exe());
    j.kill = fake_kill;
    j.not_responding_check = |_| Box::pin(async { Ok(Vec::new()) });
    j.restart().await.unwrap();
    assert!(killed().is_empty());
    assert_eq!(SPAWNS.with(Cell::get), 0, "MC answers, so it isn't started");
  }

  #[tokio::test(start_paused = true)]
  async fn without_an_exe_path_mc_is_never_closed_or_asked_about() {
    let mut j = jriver(0, Down::Refused, None);
    j.kill = fake_kill;
    j.not_responding_check = fake_hung;
    assert!(matches!(j.restart().await, Err(LaunchError::NoExePath)));
    assert_eq!(j.not_responding().await, Some(false));
    assert!(killed().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn the_hang_check_asks_windows_about_the_configured_image() {
    let mut j = jriver(0, Down::Refused, exe());
    j.not_responding_check = |image| {
      assert_eq!(image, "Media Center 36.exe");
      Box::pin(async { Ok(vec![1]) })
    };
    assert_eq!(j.not_responding().await, Some(true));
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_hang_check_is_unknown_not_responding() {
    let mut j = jriver(0, Down::Refused, exe());
    j.not_responding_check = |_| Box::pin(async { Err(std::io::Error::other("tasklist failed")) });
    assert_eq!(j.not_responding().await, None);
    // A restart then closes nothing and fails: it can't tell what's hung.
    j.kill = fake_kill;
    assert!(matches!(j.restart().await, Err(LaunchError::Kill(_))));
    assert!(killed().is_empty());
    assert_eq!(SPAWNS.with(Cell::get), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_bad_exe_path_is_reported() {
    let missing = std::env::temp_dir().join("no-such-media-center.exe");
    let mut j = jriver(1, Down::Refused, Some(missing));
    j.spawn = spawn_detached;
    assert!(matches!(
      j.ensure_running().await,
      Err(LaunchError::Spawn { .. })
    ));
  }
}
