//! The player interface from SPEC.md (`play(path, start_ms)`, `pause()`,
//! `resume()`, `seek(ms)`, `stop()`, `status()`) implemented over MCWS.

use std::{future::Future, sync::Arc, time::Duration};

use log::{debug, info, warn};
use tokio::time::{sleep, Instant};

use crate::{
  same_media, AliveInfo, McwsClient, McwsError, PauseAction, PlayState, PlaybackInfo, UiInfo,
};

/// The MCWS calls the player needs. Implemented by [`McwsClient`]; tests
/// substitute a scripted fake.
pub trait Mcws: Send + Sync {
  fn alive(&self) -> impl Future<Output = Result<AliveInfo, McwsError>> + Send;
  fn minimize_window(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn select_audio_stream(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn select_subtitles(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn info(&self) -> impl Future<Output = Result<PlaybackInfo, McwsError>> + Send;
  fn ui_info(&self) -> impl Future<Output = Result<UiInfo, McwsError>> + Send;
  fn play_by_filename(&self, path: &str) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn clear_playlist(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn seek(&self, position_ms: u64) -> impl Future<Output = Result<u64, McwsError>> + Send;
  fn pause(&self, action: PauseAction) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn stop(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
}

impl Mcws for McwsClient {
  fn alive(&self) -> impl Future<Output = Result<AliveInfo, McwsError>> + Send {
    McwsClient::alive(self)
  }
  fn minimize_window(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::minimize_window(self)
  }
  fn select_audio_stream(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::select_audio_stream(self, track)
  }
  fn select_subtitles(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::select_subtitles(self, track)
  }
  fn info(&self) -> impl Future<Output = Result<PlaybackInfo, McwsError>> + Send {
    McwsClient::info(self)
  }
  fn ui_info(&self) -> impl Future<Output = Result<UiInfo, McwsError>> + Send {
    McwsClient::ui_info(self)
  }
  fn play_by_filename(&self, path: &str) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::play_by_filename(self, path)
  }
  fn clear_playlist(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::clear_playlist(self)
  }
  fn seek(&self, position_ms: u64) -> impl Future<Output = Result<u64, McwsError>> + Send {
    McwsClient::seek(self, position_ms)
  }
  fn pause(&self, action: PauseAction) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::pause(self, action)
  }
  fn stop(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    McwsClient::stop(self)
  }
}

impl<T: Mcws> Mcws for Arc<T> {
  fn alive(&self) -> impl Future<Output = Result<AliveInfo, McwsError>> + Send {
    (**self).alive()
  }
  fn minimize_window(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).minimize_window()
  }
  fn select_audio_stream(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).select_audio_stream(track)
  }
  fn select_subtitles(&self, track: i32) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).select_subtitles(track)
  }
  fn info(&self) -> impl Future<Output = Result<PlaybackInfo, McwsError>> + Send {
    (**self).info()
  }
  fn ui_info(&self) -> impl Future<Output = Result<UiInfo, McwsError>> + Send {
    (**self).ui_info()
  }
  fn play_by_filename(&self, path: &str) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).play_by_filename(path)
  }
  fn clear_playlist(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).clear_playlist()
  }
  fn seek(&self, position_ms: u64) -> impl Future<Output = Result<u64, McwsError>> + Send {
    (**self).seek(position_ms)
  }
  fn pause(&self, action: PauseAction) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).pause(action)
  }
  fn stop(&self) -> impl Future<Output = Result<(), McwsError>> + Send {
    (**self).stop()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerOptions {
  /// From `PlayByFilename` until the file is playing (SPEC: 20 s).
  pub start_timeout: Duration,
  /// For Playing Now to empty after `ClearPlaylist` (observed: ~400 ms).
  pub clear_timeout: Duration,
  /// Poll interval during the start sequence only.
  pub start_poll_interval: Duration,
  /// How far the landed position may be from the requested start (SPEC: ±2 s).
  pub position_tolerance: Duration,
  /// After a start seek, how long to wait for the position to advance from
  /// the target before playing on unconfirmed (observed: ~1.2 s).
  pub confirm_timeout: Duration,
}

impl Default for PlayerOptions {
  fn default() -> Self {
    Self {
      start_timeout: Duration::from_secs(20),
      clear_timeout: Duration::from_secs(5),
      start_poll_interval: Duration::from_millis(250),
      position_tolerance: Duration::from_secs(2),
      confirm_timeout: Duration::from_secs(5),
    }
  }
}

/// A file that MC is playing (or holding paused) after [`JRiverPlayer::play`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedPlayback {
  pub file_key: i64,
  pub duration_ms: u64,
  /// Position at the moment playback was confirmed.
  pub position_ms: u64,
  /// The position landed within the tolerance of the requested start.
  pub position_confirmed: bool,
  /// MC switched to Display view; `None` if `UserInterface/Info` failed.
  pub display_view: Option<bool>,
  /// The filename as MC reports it (a Blu-ray shows as `index.bluray;N`).
  pub filename: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
  #[error(transparent)]
  Mcws(#[from] McwsError),
  #[error("Playing Now did not empty within {waited:?} after ClearPlaylist (state {state:?}, {tracks} tracks)")]
  ClearTimeout {
    waited: Duration,
    state: PlayState,
    tracks: u32,
  },
  #[error("the file was not playing within {waited:?} (last state {last_state:?}, file {last_filename:?})")]
  Timeout {
    waited: Duration,
    last_state: PlayState,
    last_filename: Option<String>,
  },
  #[error("MC stopped while opening the file (it may be unreadable or unsupported)")]
  StoppedWhileOpening,
  #[error("another file took over while starting: {filename:?}")]
  Replaced { filename: Option<String> },
}

/// JRiver as the bridge's player. One instance drives one zone.
#[derive(Debug)]
pub struct JRiverPlayer<M> {
  mcws: M,
  options: PlayerOptions,
}

impl<M: Mcws> JRiverPlayer<M> {
  pub fn new(mcws: M, options: PlayerOptions) -> Self {
    Self { mcws, options }
  }

  pub fn mcws(&self) -> &M {
    &self.mcws
  }

  /// Replace whatever MC has queued with `path` and start it at `start_ms`.
  ///
  /// Sequence (SPEC "Playback flow", measured on MC 36.0.51):
  /// 1. `ClearPlaylist` and wait for Playing Now to empty.
  /// 2. `PlayByFilename`, then wait until MC plays *our* file (`State` 2
  ///    with a real file key and a duration). A seek sent earlier, while MC
  ///    is still opening the file, is acknowledged and then lost.
  /// 3. Seek to `start_ms`, always, even 0: about 1 s after it starts
  ///    playing, MC jumps to its own bookmark for the file unless a seek has
  ///    already been made.
  /// 4. Confirm only once the position is within tolerance *and advancing*:
  ///    right after a seek MC echoes the target, frozen, for about a second.
  ///    If MC drifts away from the target (bookmark jump, lost seek), seek
  ///    once more.
  pub async fn play(&self, path: &str, start_ms: u64) -> Result<StartedPlayback, StartError> {
    self.clear_and_wait().await?;

    let started = Instant::now();
    info!("JRiver: playing {path} from {start_ms} ms");
    self.mcws.play_by_filename(path).await?;

    // From here on MC may be opening or playing our file. If the start fails
    // for any reason other than another file taking over, stop MC so it
    // can't go on playing a film the caller believes failed to start.
    match self.drive_start(path, start_ms, started).await {
      Err(err) if !matches!(err, StartError::Replaced { .. }) => {
        warn!("JRiver start failed ({err}); stopping MC");
        if let Err(stop_err) = self.mcws.stop().await {
          warn!("JRiver: stop after failed start failed: {stop_err}");
        }
        Err(err)
      }
      other => other,
    }
  }

  /// Steps 2–4 of [`Self::play`], after `PlayByFilename` was sent.
  async fn drive_start(
    &self,
    path: &str,
    start_ms: u64,
    started: Instant,
  ) -> Result<StartedPlayback, StartError> {
    let deadline = started + self.options.start_timeout;

    // Step 2: wait for our file to play.
    let mut seen_active = false;
    loop {
      let status = self.mcws.info().await?;
      let ours = is_ours(path, &status);
      debug!(
        "JRiver start: state {:?}, key {}, pos {} ms, dur {} ms, ours {ours}",
        status.state, status.file_key, status.position_ms, status.duration_ms
      );
      if ours {
        match status.state {
          PlayState::Playing | PlayState::Paused if status.duration_ms > 0 => break,
          PlayState::Opening | PlayState::Playing | PlayState::Paused => seen_active = true,
          PlayState::Stopped if seen_active => return Err(StartError::StoppedWhileOpening),
          PlayState::Stopped | PlayState::Unknown(_) => {}
        }
      }
      if Instant::now() >= deadline {
        return Err(StartError::Timeout {
          waited: self.options.start_timeout,
          last_state: status.state,
          last_filename: status.filename,
        });
      }
      sleep(self.options.start_poll_interval).await;
    }

    // Steps 3 and 4: seek, then confirm the landing.
    let tolerance = duration_ms(self.options.position_tolerance);
    let mut seeks = 0;
    self.start_seek(start_ms, &mut seeks).await?;
    let mut confirm_deadline = Instant::now() + self.options.confirm_timeout;
    let mut previous_near: Option<u64> = None;
    loop {
      sleep(self.options.start_poll_interval).await;
      let status = self.mcws.info().await?;
      let pos = status.position_ms;
      let near = pos.abs_diff(start_ms) <= tolerance;
      debug!(
        "JRiver start confirm: state {:?}, pos {pos} ms (target {start_ms} ms)",
        status.state
      );
      if status.state == PlayState::Stopped {
        return Err(StartError::StoppedWhileOpening);
      }
      if !is_ours(path, &status) {
        return Err(StartError::Replaced {
          filename: status.filename,
        });
      }
      match status.state {
        PlayState::Paused if near => return Ok(self.finish(status, true, started).await),
        PlayState::Playing if near && previous_near.is_some_and(|prev| pos > prev) => {
          return Ok(self.finish(status, true, started).await);
        }
        PlayState::Playing | PlayState::Paused if !near => {
          if seeks < MAX_START_SEEKS {
            warn!("JRiver start: position {pos} ms is off the target {start_ms} ms; seeking again");
            self.start_seek(start_ms, &mut seeks).await?;
            previous_near = None;
            confirm_deadline = Instant::now() + self.options.confirm_timeout;
            continue;
          }
          warn!("JRiver start: position {pos} ms still off the target {start_ms} ms after {seeks} seeks");
          return Ok(self.finish(status, false, started).await);
        }
        _ => {}
      }
      previous_near = near.then_some(pos);
      if Instant::now() >= confirm_deadline {
        warn!(
          "JRiver start: position {pos} ms not seen advancing from {start_ms} ms within {:?}",
          self.options.confirm_timeout
        );
        return Ok(self.finish(status, false, started).await);
      }
    }
  }

  async fn start_seek(&self, start_ms: u64, seeks: &mut u32) -> Result<(), McwsError> {
    let echoed = self.mcws.seek(start_ms).await?;
    *seeks += 1;
    debug!("JRiver start: seek {seeks} to {start_ms} ms, MC echoes {echoed} ms");
    Ok(())
  }

  pub async fn pause(&self) -> Result<(), McwsError> {
    self.mcws.pause(PauseAction::Pause).await
  }

  pub async fn resume(&self) -> Result<(), McwsError> {
    self.mcws.pause(PauseAction::Unpause).await
  }

  pub async fn toggle_pause(&self) -> Result<(), McwsError> {
    self.mcws.pause(PauseAction::Toggle).await
  }

  /// Absolute seek; returns the position MC reports after it.
  pub async fn seek(&self, position_ms: u64) -> Result<u64, McwsError> {
    self.mcws.seek(position_ms).await
  }

  pub async fn stop(&self) -> Result<(), McwsError> {
    self.mcws.stop().await
  }

  pub async fn status(&self) -> Result<PlaybackInfo, McwsError> {
    self.mcws.info().await
  }

  /// `Alive`: MC is up and its Media Network answers.
  pub async fn alive(&self) -> Result<AliveInfo, McwsError> {
    self.mcws.alive().await
  }

  /// MC's UI mode (`UserInterface/Info`, read-only); 2 is Display view.
  pub async fn ui_mode(&self) -> Result<i32, McwsError> {
    self.mcws.ui_info().await.map(|ui| ui.mode)
  }

  /// Minimize MC's window (`MCC_MINIMIZE_WINDOW`). It toggles: the caller
  /// must know MC isn't minimized already.
  pub async fn minimize(&self) -> Result<(), McwsError> {
    self.mcws.minimize_window().await
  }

  /// Pick the playing file's audio track (`MCC_SET_AUDIO_STREAM`). MC
  /// counts the file's audio tracks from 0.
  pub async fn select_audio_stream(&self, track: i32) -> Result<(), McwsError> {
    self.mcws.select_audio_stream(track).await
  }

  /// Pick the playing file's subtitles (`MCC_SET_SUBTITLES`). MC counts the
  /// file's subtitle tracks from 1; 0 turns subtitles off.
  pub async fn select_subtitles(&self, track: i32) -> Result<(), McwsError> {
    self.mcws.select_subtitles(track).await
  }

  async fn clear_and_wait(&self) -> Result<(), StartError> {
    self.mcws.clear_playlist().await?;
    let deadline = Instant::now() + self.options.clear_timeout;
    loop {
      let status = self.mcws.info().await?;
      if status.playing_now_tracks == 0 && status.state == PlayState::Stopped {
        return Ok(());
      }
      if Instant::now() >= deadline {
        return Err(StartError::ClearTimeout {
          waited: self.options.clear_timeout,
          state: status.state,
          tracks: status.playing_now_tracks,
        });
      }
      sleep(self.options.start_poll_interval).await;
    }
  }

  async fn finish(
    &self,
    status: PlaybackInfo,
    position_confirmed: bool,
    started: Instant,
  ) -> StartedPlayback {
    let display_view = match self.mcws.ui_info().await {
      Ok(ui) => {
        if !ui.is_display_view() {
          warn!(
            "JRiver: MC is not in Display view (UI mode {}); the video may not be full screen",
            ui.mode
          );
        }
        Some(ui.is_display_view())
      }
      Err(err) => {
        warn!("JRiver: could not read the UI mode: {err}");
        None
      }
    };
    info!(
      "JRiver: playing after {} ms at {} ms of {} ms (key {})",
      started.elapsed().as_millis(),
      status.position_ms,
      status.duration_ms,
      status.file_key
    );
    StartedPlayback {
      file_key: status.file_key,
      duration_ms: status.duration_ms,
      position_ms: status.position_ms,
      position_confirmed,
      display_view,
      filename: status.filename,
    }
  }
}

/// The initial start seek plus one correction.
const MAX_START_SEEKS: u32 = 2;

fn is_ours(path: &str, status: &PlaybackInfo) -> bool {
  status.has_file()
    && status
      .filename
      .as_deref()
      .is_some_and(|reported| same_media(path, reported))
}

fn duration_ms(duration: Duration) -> u64 {
  u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use std::{collections::VecDeque, sync::Mutex};

  use super::*;
  use crate::Function;

  const FILE: &str = r"\\nas\movies\Avatar\Avatar.mkv";

  #[derive(Debug, Clone, PartialEq, Eq)]
  enum Call {
    Info,
    Ui,
    Play(String),
    Clear,
    Seek(u64),
    Pause(PauseAction),
    Stop,
  }

  /// Scripted MC: `info()` returns the queued snapshots in order and repeats
  /// the last one; every call is recorded.
  struct FakeMc {
    infos: Mutex<VecDeque<Result<PlaybackInfo, ()>>>,
    last: Mutex<Option<PlaybackInfo>>,
    ui: Result<UiInfo, ()>,
    play_result: Mutex<Option<McwsError>>,
    calls: Mutex<Vec<Call>>,
  }

  impl FakeMc {
    fn new(infos: Vec<PlaybackInfo>) -> Self {
      Self {
        infos: Mutex::new(infos.into_iter().map(Ok).collect()),
        last: Mutex::new(None),
        ui: Ok(UiInfo { mode: 2 }),
        play_result: Mutex::new(None),
        calls: Mutex::new(Vec::new()),
      }
    }

    fn calls(&self) -> Vec<Call> {
      self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: Call) {
      self.calls.lock().unwrap().push(call);
    }
  }

  fn unreachable(function: Function) -> McwsError {
    McwsError::Timeout {
      function,
      timeout: Duration::from_secs(5),
    }
  }

  impl Mcws for FakeMc {
    async fn alive(&self) -> Result<AliveInfo, McwsError> {
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
      self.record(Call::Info);
      let next = self.infos.lock().unwrap().pop_front();
      match next {
        Some(Ok(info)) => {
          *self.last.lock().unwrap() = Some(info.clone());
          Ok(info)
        }
        Some(Err(())) => Err(unreachable(Function::PlaybackInfo)),
        None => Ok(
          self
            .last
            .lock()
            .unwrap()
            .clone()
            .expect("script has a snapshot"),
        ),
      }
    }
    async fn ui_info(&self) -> Result<UiInfo, McwsError> {
      self.record(Call::Ui);
      self
        .ui
        .map_err(|()| unreachable(Function::UserInterfaceInfo))
    }
    async fn play_by_filename(&self, path: &str) -> Result<(), McwsError> {
      self.record(Call::Play(path.to_owned()));
      match self.play_result.lock().unwrap().take() {
        Some(err) => Err(err),
        None => Ok(()),
      }
    }
    async fn clear_playlist(&self) -> Result<(), McwsError> {
      self.record(Call::Clear);
      Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<u64, McwsError> {
      self.record(Call::Seek(position_ms));
      Ok(position_ms)
    }
    async fn pause(&self, action: PauseAction) -> Result<(), McwsError> {
      self.record(Call::Pause(action));
      Ok(())
    }
    async fn stop(&self) -> Result<(), McwsError> {
      self.record(Call::Stop);
      Ok(())
    }
  }

  fn snapshot(
    state: PlayState,
    key: i64,
    pos: u64,
    dur: u64,
    tracks: u32,
    file: Option<&str>,
  ) -> PlaybackInfo {
    PlaybackInfo {
      state,
      file_key: key,
      position_ms: pos,
      duration_ms: dur,
      playing_now_tracks: tracks,
      filename: file.map(str::to_owned),
      name: None,
    }
  }

  fn idle() -> PlaybackInfo {
    snapshot(PlayState::Stopped, -1, 0, 0, 0, None)
  }
  /// What MC reports right after PlayByFilename (observed): State=2, no file.
  fn stale() -> PlaybackInfo {
    snapshot(PlayState::Playing, -1, 0, 7_925_000, 1, None)
  }
  fn opening(file: &str) -> PlaybackInfo {
    snapshot(PlayState::Opening, 7, 0, 11_840_000, 1, Some(file))
  }
  fn playing(file: &str, pos: u64) -> PlaybackInfo {
    snapshot(PlayState::Playing, 7, pos, 11_840_703, 1, Some(file))
  }
  fn paused(file: &str, pos: u64) -> PlaybackInfo {
    snapshot(PlayState::Paused, 7, pos, 11_840_703, 1, Some(file))
  }

  fn player(fake: FakeMc) -> JRiverPlayer<FakeMc> {
    JRiverPlayer::new(fake, PlayerOptions::default())
  }

  fn seeks(p: &JRiverPlayer<FakeMc>) -> Vec<u64> {
    p.mcws()
      .calls()
      .iter()
      .filter_map(|c| match c {
        Call::Seek(ms) => Some(*ms),
        _ => None,
      })
      .collect()
  }

  #[tokio::test(start_paused = true)]
  async fn seeks_once_playing_and_confirms_on_advancing_position() {
    // Measured on MC 36.0.51: opening, playing at 0, then after the seek a
    // frozen echo of the target for ~1 s before the position advances.
    let p = player(FakeMc::new(vec![
      idle(),
      stale(),
      opening(FILE),
      opening(FILE),
      playing(FILE, 0),
      playing(FILE, 1_200_000),
      playing(FILE, 1_200_000),
      playing(FILE, 1_200_506),
    ]));
    let started = p.play(FILE, 1_200_000).await.unwrap();
    assert!(started.position_confirmed);
    assert_eq!(started.position_ms, 1_200_506);
    assert_eq!(started.display_view, Some(true));
    assert_eq!(seeks(&p), vec![1_200_000]);
    let calls = p.mcws().calls();
    assert_eq!(calls[0], Call::Clear);
    assert_eq!(calls[2], Call::Play(FILE.to_owned()));
    // The seek comes after the fifth snapshot (the first "playing"), never
    // while MC is still opening the file.
    let seek_at = calls
      .iter()
      .position(|c| matches!(c, Call::Seek(_)))
      .unwrap();
    let infos_before = calls[..seek_at]
      .iter()
      .filter(|c| **c == Call::Info)
      .count();
    assert_eq!(infos_before, 5, "{calls:?}");
    assert_eq!(calls.last(), Some(&Call::Ui));
  }

  #[tokio::test(start_paused = true)]
  async fn frozen_echo_alone_is_never_confirmed() {
    // MC acknowledged the seek but the position never moves: give up after
    // the confirm timeout and play on unconfirmed, without a second seek.
    let p = player(FakeMc::new(vec![
      idle(),
      playing(FILE, 0),
      playing(FILE, 600_000),
    ]));
    let before = Instant::now();
    let started = p.play(FILE, 600_000).await.unwrap();
    assert!(!started.position_confirmed);
    assert_eq!(seeks(&p), vec![600_000]);
    let waited = before.elapsed();
    assert!(
      waited >= Duration::from_secs(5) && waited < Duration::from_secs(7),
      "{waited:?}"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn stale_playing_snapshot_is_not_mistaken_for_our_file() {
    // Stale State=2/key -1 for a while: no seek, no early success.
    let p = player(FakeMc::new(vec![
      idle(),
      stale(),
      stale(),
      stale(),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 250),
    ]));
    let started = p.play(FILE, 0).await.unwrap();
    assert_eq!(started.file_key, 7);
    let calls = p.mcws().calls();
    let seek_at = calls
      .iter()
      .position(|c| matches!(c, Call::Seek(_)))
      .unwrap();
    assert_eq!(
      calls[..seek_at]
        .iter()
        .filter(|c| **c == Call::Info)
        .count(),
      5
    );
  }

  #[tokio::test(start_paused = true)]
  async fn start_at_zero_still_seeks_to_beat_mc_bookmark() {
    let p = player(FakeMc::new(vec![
      idle(),
      opening(FILE),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 224),
    ]));
    let started = p.play(FILE, 0).await.unwrap();
    assert!(started.position_confirmed);
    assert_eq!(seeks(&p), vec![0]);
  }

  #[tokio::test(start_paused = true)]
  async fn corrects_once_when_mc_jumps_to_its_own_bookmark() {
    // Observed: without a seek MC jumps to its bookmark ~1 s after playing.
    let p = player(FakeMc::new(vec![
      idle(),
      opening(FILE),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 3_626_776),
      playing(FILE, 0),
      playing(FILE, 300),
    ]));
    let started = p.play(FILE, 0).await.unwrap();
    assert!(started.position_confirmed);
    assert_eq!(seeks(&p), vec![0, 0]);
  }

  #[tokio::test(start_paused = true)]
  async fn lost_seek_is_corrected() {
    // Observed: a seek MC acknowledged, then playback from the start.
    let p = player(FakeMc::new(vec![
      idle(),
      playing(FILE, 0),
      playing(FILE, 1_200_000),
      playing(FILE, 2_709),
      playing(FILE, 1_200_000),
      playing(FILE, 1_200_152),
    ]));
    let started = p.play(FILE, 1_200_000).await.unwrap();
    assert!(started.position_confirmed);
    assert_eq!(seeks(&p), vec![1_200_000, 1_200_000]);
  }

  #[tokio::test(start_paused = true)]
  async fn gives_up_correcting_after_one_retry_but_keeps_playing() {
    let p = player(FakeMc::new(vec![
      idle(),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 500),
    ]));
    let started = p.play(FILE, 3_600_000).await.unwrap();
    assert!(!started.position_confirmed);
    assert_eq!(
      seeks(&p),
      vec![3_600_000, 3_600_000],
      "one start seek plus one correction"
    );
    assert!(!p.mcws().calls().contains(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn paused_at_the_target_counts_as_landed() {
    let p = player(FakeMc::new(vec![
      idle(),
      playing(FILE, 0),
      paused(FILE, 600_000),
    ]));
    let started = p.play(FILE, 600_000).await.unwrap();
    assert!(started.position_confirmed);
  }

  #[tokio::test(start_paused = true)]
  async fn times_out_after_20_seconds_and_stops_mc() {
    let p = player(FakeMc::new(vec![idle(), stale()]));
    let before = Instant::now();
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(matches!(err, StartError::Timeout { .. }), "{err:?}");
    let waited = before.elapsed();
    assert!(
      waited >= Duration::from_secs(20) && waited < Duration::from_secs(21),
      "{waited:?}"
    );
    assert_eq!(p.mcws().calls().last(), Some(&Call::Stop));
    assert!(seeks(&p).is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn another_file_playing_is_not_ours() {
    let p = player(FakeMc::new(vec![
      idle(),
      playing(r"\\nas\movies\Other.mkv", 5_000),
    ]));
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(
      matches!(&err, StartError::Timeout { last_filename: Some(f), .. } if f.ends_with("Other.mkv")),
      "{err:?}"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn another_file_taking_over_during_confirmation_is_an_error() {
    let p = player(FakeMc::new(vec![
      idle(),
      playing(FILE, 0),
      playing(r"\\nas\movies\Other.mkv", 5_000),
    ]));
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(matches!(err, StartError::Replaced { .. }), "{err:?}");
    // Someone else's file is playing now; leave it alone.
    assert!(!p.mcws().calls().contains(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn clear_timeout_never_calls_play() {
    let busy = snapshot(PlayState::Playing, 3, 10, 100, 1, Some(FILE));
    let p = player(FakeMc::new(vec![busy]));
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(
      matches!(err, StartError::ClearTimeout { tracks: 1, .. }),
      "{err:?}"
    );
    assert!(!p.mcws().calls().iter().any(|c| matches!(c, Call::Play(_))));
  }

  #[tokio::test(start_paused = true)]
  async fn waits_for_clear_to_settle() {
    // ClearPlaylist is asynchronous in MC: the first snapshots still show
    // the old queue.
    let old = snapshot(
      PlayState::Paused,
      2,
      600_000,
      7_139_465,
      2,
      Some(r"\\nas\old.mkv"),
    );
    let p = player(FakeMc::new(vec![
      old.clone(),
      old,
      idle(),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 100),
    ]));
    p.play(FILE, 0).await.unwrap();
    let calls = p.mcws().calls();
    let play_at = calls
      .iter()
      .position(|c| matches!(c, Call::Play(_)))
      .unwrap();
    assert_eq!(play_at, 4, "{calls:?}");
  }

  #[tokio::test(start_paused = true)]
  async fn stopping_while_opening_fails_fast() {
    let mut stopped = opening(FILE);
    stopped.state = PlayState::Stopped;
    let p = player(FakeMc::new(vec![idle(), opening(FILE), stopped]));
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(matches!(err, StartError::StoppedWhileOpening), "{err:?}");
  }

  #[tokio::test(start_paused = true)]
  async fn stopping_during_confirmation_fails() {
    let mut stopped = playing(FILE, 0);
    stopped.state = PlayState::Stopped;
    let p = player(FakeMc::new(vec![idle(), playing(FILE, 0), stopped]));
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(matches!(err, StartError::StoppedWhileOpening), "{err:?}");
    assert_eq!(p.mcws().calls().last(), Some(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn bluray_index_bdmv_matches_index_bluray_title() {
    let disc = r"\\nas\movies\Star Trek\Into Darkness";
    let requested = format!(r"{disc}\BDMV\index.bdmv");
    let reported = format!(r"{disc}\BDMV\index.bluray;1");
    let p = player(FakeMc::new(vec![
      idle(),
      opening(&reported),
      playing(&reported, 0),
      playing(&reported, 500_000),
      playing(&reported, 500_100),
    ]));
    let started = p.play(&requested, 500_000).await.unwrap();
    assert!(started.position_confirmed);
    assert_eq!(started.filename.as_deref(), Some(reported.as_str()));
  }

  #[tokio::test(start_paused = true)]
  async fn play_by_filename_error_is_returned_without_polling() {
    let fake = FakeMc::new(vec![idle()]);
    *fake.play_result.lock().unwrap() = Some(McwsError::Status {
      function: Function::PlayByFilename,
      status: 500,
      detail: None,
    });
    let p = player(fake);
    let err = p.play(r"\\nas\movies\Disc", 0).await.unwrap_err();
    assert!(
      matches!(err, StartError::Mcws(McwsError::Status { status: 500, .. })),
      "{err:?}"
    );
    assert_eq!(
      p.mcws().calls().last(),
      Some(&Call::Play(r"\\nas\movies\Disc".to_owned()))
    );
  }

  #[tokio::test(start_paused = true)]
  async fn mcws_failure_during_start_is_returned() {
    let fake = FakeMc::new(vec![]);
    fake.infos.lock().unwrap().extend([Ok(idle()), Err(())]);
    let p = player(fake);
    let err = p.play(FILE, 0).await.unwrap_err();
    assert!(
      matches!(&err, StartError::Mcws(e) if e.is_unreachable()),
      "{err:?}"
    );
    // MC may still finish opening the film: it must be stopped, so nothing
    // plays that the caller thinks failed.
    assert_eq!(p.mcws().calls().last(), Some(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn mcws_failure_during_confirmation_stops_mc() {
    let fake = FakeMc::new(vec![]);
    fake
      .infos
      .lock()
      .unwrap()
      .extend([Ok(idle()), Ok(playing(FILE, 0)), Err(())]);
    let p = player(fake);
    assert!(matches!(p.play(FILE, 0).await, Err(StartError::Mcws(_))));
    assert_eq!(p.mcws().calls().last(), Some(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn error_before_play_by_filename_does_not_stop() {
    // ClearPlaylist failed: nothing of ours was started, nothing to stop.
    let fake = FakeMc::new(vec![]);
    fake.infos.lock().unwrap().extend([Err(())]);
    let p = player(fake);
    assert!(p.play(FILE, 0).await.is_err());
    assert!(!p.mcws().calls().contains(&Call::Stop));
  }

  fn confirmable() -> Vec<PlaybackInfo> {
    vec![
      idle(),
      playing(FILE, 0),
      playing(FILE, 0),
      playing(FILE, 200),
    ]
  }

  #[tokio::test(start_paused = true)]
  async fn ui_mode_failure_does_not_fail_the_start() {
    let mut fake = FakeMc::new(confirmable());
    fake.ui = Err(());
    let p = player(fake);
    assert_eq!(p.play(FILE, 0).await.unwrap().display_view, None);
  }

  #[tokio::test(start_paused = true)]
  async fn not_in_display_view_is_reported() {
    let mut fake = FakeMc::new(confirmable());
    fake.ui = Ok(UiInfo { mode: 0 });
    let p = player(fake);
    assert_eq!(p.play(FILE, 0).await.unwrap().display_view, Some(false));
  }

  #[tokio::test]
  async fn controls_map_to_mcws_calls() {
    let p = player(FakeMc::new(vec![idle()]));
    p.pause().await.unwrap();
    p.resume().await.unwrap();
    p.toggle_pause().await.unwrap();
    assert_eq!(p.seek(42).await.unwrap(), 42);
    p.stop().await.unwrap();
    p.status().await.unwrap();
    assert_eq!(
      p.mcws().calls(),
      vec![
        Call::Pause(PauseAction::Pause),
        Call::Pause(PauseAction::Unpause),
        Call::Pause(PauseAction::Toggle),
        Call::Seek(42),
        Call::Stop,
        Call::Info,
      ]
    );
  }
}
