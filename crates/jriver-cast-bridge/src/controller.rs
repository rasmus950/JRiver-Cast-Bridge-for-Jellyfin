//! The session controller (SPEC.md "Playback flow").
//!
//! It owns one playback at a time. Every way out of a playback (Stop from
//! Jellyfin, JRiver stopping, the file ending, a new Play, MCWS no longer
//! answering, a failed start, shutdown) goes through [`Controller::end`],
//! which reports it to Jellyfin exactly once and returns to idle.
//!
//! The start sequence (item lookup, path mapping, existence check, starting
//! MC if needed, the JRiver start sequence) runs as its own task so a Stop or a new Play can
//! cancel it; its result comes back as [`Event::StartFinished`].

use std::{future::Future, sync::Arc, time::Duration};

use crate::socket::{GeneralCommand, JellyfinCommand, PlayRequest, PlaystateRequest};
use jriver_mcws::{
  same_media, Function, McwsError, PlayState, PlaybackInfo, StartError, StartedPlayback,
  UI_MODE_DISPLAY,
};
use log::{debug, error, info, warn};
use tokio::{
  sync::{mpsc, watch},
  task::JoinHandle,
  time::{Instant, MissedTickBehavior},
};

use crate::{
  jellyfin::{
    Item, JellyfinClient, JellyfinError, MediaStream, NowPlaying, PlayMethod, ProgressReport,
    Report, StopReport,
  },
  jriver::LaunchError,
  path_map::{ExistsError, MapError, PathMapper},
  record::{now_secs, Found, MemoryRecord, PlaybackRecord, RecordStore},
  reporter::Reporter,
  resolve::{select_target, PlayTarget, ResolveError},
  tracks::{self, StartPick, TrackChoice, TrackKind},
  units::{Millis, Ticks},
};

/// JRiver as the controller sees it. [`crate::jriver::JRiver`] in the bridge.
pub trait Player: Send + Sync + 'static {
  /// Can the player open `path`? (The share may be down or deny access.)
  fn check_path(
    &self,
    path: &str,
    timeout: Duration,
  ) -> impl Future<Output = Result<(), ExistsError>> + Send;
  /// Start MC if it isn't running. `Ok(true)` if it had to be started.
  fn ensure_running(&self) -> impl Future<Output = Result<bool, LaunchError>> + Send;
  fn play(
    &self,
    path: &str,
    start_ms: u64,
  ) -> impl Future<Output = Result<StartedPlayback, StartError>> + Send;
  fn pause(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn resume(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn toggle_pause(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn seek(&self, position_ms: u64) -> impl Future<Output = Result<u64, McwsError>> + Send;
  fn stop(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  fn status(&self) -> impl Future<Output = Result<PlaybackInfo, McwsError>> + Send;
  /// MC's UI mode (read-only); [`UI_MODE_DISPLAY`] is full-screen Display view.
  fn ui_mode(&self) -> impl Future<Output = Result<i32, McwsError>> + Send;
  /// Minimize MC. It toggles, so only call it when MC isn't minimized.
  fn minimize(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  /// Pick the playing file's audio or subtitle track by JRiver's number
  /// (see [`crate::tracks`]).
  fn select_track(
    &self,
    kind: TrackKind,
    track: i32,
  ) -> impl Future<Output = Result<(), McwsError>> + Send;
  /// The health check while no film plays: a read-only call that MC's
  /// main thread answers (`Playback/Info`). Not `Alive`: MC's web server
  /// keeps answering that while the rest of MC is hung.
  fn check_health(&self) -> impl Future<Output = Result<(), McwsError>> + Send;
  /// Windows reports MC as "Not responding".
  fn not_responding(&self) -> impl Future<Output = Option<bool>> + Send;
  /// Close a hung MC by force, start it again and wait until it answers.
  fn restart(&self) -> impl Future<Output = Result<(), LaunchError>> + Send;
  /// The bridge started MC and it hasn't answered yet: a slow start-up is
  /// not a hang.
  fn launching(&self) -> bool;
}

/// The Jellyfin calls the controller makes itself (reports go through the
/// [`Reporter`]). [`JellyfinClient`] in the bridge.
pub trait Library: Send + Sync + 'static {
  fn item(&self, item_id: &str) -> impl Future<Output = Result<Item, JellyfinError>> + Send;
  fn register(&self) -> impl Future<Output = Result<(), JellyfinError>> + Send;
  /// The film the server shows as playing on this cast target.
  fn now_playing(&self) -> impl Future<Output = Result<Option<NowPlaying>, JellyfinError>> + Send;
}

impl Library for JellyfinClient {
  fn item(&self, item_id: &str) -> impl Future<Output = Result<Item, JellyfinError>> + Send {
    JellyfinClient::item(self, item_id)
  }
  fn register(&self) -> impl Future<Output = Result<(), JellyfinError>> + Send {
    JellyfinClient::register(self)
  }
  fn now_playing(&self) -> impl Future<Output = Result<Option<NowPlaying>, JellyfinError>> + Send {
    JellyfinClient::now_playing(self)
  }
}

#[derive(Debug)]
pub enum Event {
  Command(JellyfinCommand),
  /// The socket is up (first connect or after a reconnect): register as a
  /// cast target again, and resend progress if something is playing.
  Connected,
  ConnectionLost,
  /// The start task finished. Stale results (from a cancelled start) are
  /// ignored by generation.
  StartFinished {
    generation: u64,
    result: Result<Started, StartFailure>,
  },
  /// The check after the first connect (SPEC.md "The bridge killed")
  /// finished. Ignored if a Play arrived meanwhile.
  RecoveryFinished {
    generation: u64,
    recovery: Recovery,
  },
  /// Quit: stop a playing film, report it, and end the run.
  Shutdown,
  /// Log out: as Quit, then the service revokes and deletes the login.
  Logout,
  /// A new login was saved: the run ends and the service starts over with
  /// it. A playing film plays on, recorded for the next run to take over.
  LoginChanged,
  /// The settings were saved: as a new login, but the service reloads the
  /// config first.
  Reload,
}

/// Why [`Controller::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
  Shutdown,
  Logout,
  LoginChanged,
  Reload,
}

/// What the bridge's record (or the server) and MC showed after a restart.
#[derive(Debug)]
pub enum Recovery {
  /// No film was left over.
  Nothing,
  /// MC still plays the film left over: track it again.
  TakeOver {
    item_id: String,
    media_source_id: Option<String>,
    /// The recorded play session, carried on; `None` starts a new one.
    play_session_id: Option<String>,
    name: String,
    path: String,
    status: PlaybackInfo,
    /// The tracks in use, as recorded.
    tracks: TrackChoice,
    /// The file's tracks, looked up again (empty if that failed).
    streams: Vec<MediaStream>,
  },
  /// MC isn't playing the film left over: report it stopped.
  Stale(StopReport),
  /// The film was cast under another Jellyfin user before a new login. It
  /// can't be reported any more, so MC stops it if it still plays.
  OtherUser { item_id: String, playing: bool },
}

#[derive(Debug)]
pub struct Started {
  /// The item's name, for the tray status.
  pub name: String,
  pub target: PlayTarget,
  pub path: String,
  pub playback: StartedPlayback,
  /// The audio and subtitle stream in use after the start, as far as known.
  pub tracks: TrackChoice,
}

#[derive(Debug, thiserror::Error)]
pub enum StartFailure {
  #[error("{0}")]
  Lookup(#[source] JellyfinError),
  #[error(transparent)]
  Resolve(#[from] ResolveError),
  #[error(transparent)]
  Map(#[from] MapError),
  #[error(transparent)]
  Exists(#[from] ExistsError),
  #[error(transparent)]
  Launch(#[from] LaunchError),
  #[error("JRiver: {0}")]
  Player(#[from] StartError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerOptions {
  /// MCWS status poll while playing (SPEC: 1 Hz).
  pub poll_interval: Duration,
  /// Progress report while playing or paused (SPEC: every 10 s).
  pub progress_interval: Duration,
  /// How long the share may take to answer the existence check.
  pub exists_timeout: Duration,
  /// Failed polls in a row before the playback counts as stopped.
  pub max_missed_polls: u32,
  /// Minimize MC once a playback that was full screen ends (SPEC: so the
  /// Jellyfin app on the HTPC gets focus back).
  pub minimize_after_playback: bool,
  /// Play the audio and subtitle tracks Jellyfin asks for (SPEC "Select
  /// tracks"). Off: JRiver decides, and no track command is sent.
  pub tracks_from_jellyfin: bool,
  /// How long to wait for MC to leave Display view before minimizing.
  pub minimize_wait: Duration,
  /// MC counts as hung once Windows has said "Not responding" this long
  /// (checked every `windows_check_interval`) while MCWS times out (SPEC:
  /// 3 s; config `watchdog.hang_after_s`).
  pub hang_confirm: Duration,
  /// The same while a cast is starting: opening a file can freeze MC while
  /// the NAS disks spin up (SPEC: 20 s; `watchdog.hang_while_starting_s`).
  pub hang_confirm_starting: Duration,
  /// How often to ask Windows whether MC responds (SPEC: every second).
  pub windows_check_interval: Duration,
  /// Polls and health checks wait at most this long for MCWS; longer counts
  /// as a timeout. MC answers `Playback/Info` in tens of ms when healthy.
  pub probe_timeout: Duration,
  /// MCWS silent this long while Windows says MC responds ends a playing
  /// film without a restart (SPEC: 30 s).
  pub hang_after: Duration,
  /// Health check while no film plays (SPEC: every 5 s).
  pub idle_check_interval: Duration,
  /// A hang within this long of an automatic resume isn't resumed again.
  pub resume_window: Duration,
  /// Restarts of a hung MC allowed per hour.
  pub max_restarts_per_hour: usize,
}

impl Default for ControllerOptions {
  fn default() -> Self {
    Self {
      poll_interval: Duration::from_secs(1),
      progress_interval: Duration::from_secs(10),
      exists_timeout: Duration::from_secs(10),
      max_missed_polls: 5,
      minimize_after_playback: true,
      tracks_from_jellyfin: true,
      minimize_wait: Duration::from_secs(5),
      hang_confirm: Duration::from_secs(3),
      hang_confirm_starting: Duration::from_secs(20),
      windows_check_interval: Duration::from_secs(1),
      probe_timeout: Duration::from_secs(2),
      hang_after: Duration::from_secs(30),
      idle_check_interval: Duration::from_secs(5),
      resume_window: Duration::from_secs(600),
      max_restarts_per_hour: 3,
    }
  }
}

/// A film to play again after MC was restarted.
#[derive(Debug, Clone)]
struct ResumeTarget {
  item_id: String,
  media_source_id: Option<String>,
  position_ms: u64,
  tracks: TrackChoice,
  /// It was itself an automatic resume.
  automatic: bool,
}

/// Hang detection and restart bookkeeping (SPEC.md "JRiver hung").
#[derive(Debug)]
struct Watchdog {
  /// First MCWS timeout of the current silence.
  silent_since: Option<Instant>,
  /// Since when Windows has said MC isn't responding (checked every
  /// second).
  not_responding_since: Option<Instant>,
  /// MC came back after not running and hasn't answered `Playback/Info`
  /// yet: it may still be loading.
  mc_appeared: Option<Instant>,
  /// The last health check found MC not running (refused).
  mc_down: bool,
  /// A cast's start just failed on an MCWS timeout: MC may still be busy
  /// opening the file, so the longer hang time applies for a while.
  start_timed_out: Option<Instant>,
  /// MCWS silent long enough, but Windows says MC responds: logged once.
  silence_logged: bool,
  /// Over the restart limit: logged once per hung period.
  limit_logged: bool,
  /// The health check failed for another reason (e.g. MCWS login): warned
  /// once per run of failures.
  check_error_logged: bool,
  /// Restarts within the last hour.
  restarts: std::collections::VecDeque<Instant>,
  last_resume: Option<Instant>,
  /// A start that failed shortly before a hang is resumed like a playing film.
  failed_start: Option<(ResumeTarget, Instant)>,
  last_check: Instant,
}

const HOUR: Duration = Duration::from_secs(3600);
/// A failed start this recent counts as "starting" when MC turns out hung (or
/// the start-time hang time plus 15 s, if longer).
const FAILED_START_WINDOW: Duration = Duration::from_secs(60);
/// MC seen coming back after not running counts as starting this long, or
/// until it answers `Playback/Info` (as long as the bridge's own launch wait).
const MC_STARTUP_GRACE: Duration = Duration::from_secs(60);

/// A playback as Jellyfin knows it.
#[derive(Debug, Clone)]
struct Session {
  item_id: String,
  media_source_id: Option<String>,
  play_session_id: String,
  start_ms: u64,
  /// The audio and subtitle stream Jellyfin asked for at the start.
  tracks: TrackChoice,
  /// Started by the bridge itself to resume after a hang (not a cast).
  automatic: bool,
}

#[derive(Debug)]
struct Active {
  session: Session,
  name: String,
  /// The path MC was given (for the playback record).
  path: String,
  /// The file's tracks (empty for a film taken over after a restart).
  streams: Vec<MediaStream>,
  /// The audio and subtitle stream in use, as far as the bridge knows: MC
  /// can't be asked.
  tracks: TrackChoice,
  file_key: i64,
  duration_ms: u64,
  /// Last position polled while playing or paused. After a stop MC reports
  /// 0, so the stop report always uses this one (SPEC "Played to the end").
  position_ms: u64,
  paused: bool,
  /// MC was in full-screen Display view at the last check. Only then is it
  /// certainly not minimized, which the minimize toggle needs.
  display_view: bool,
  /// The last UI-mode read failed (logged once per run of failures).
  ui_mode_failed: bool,
  last_report: Instant,
  missed_polls: u32,
  /// Position and pause state last written to the playback record.
  recorded: Option<(u64, bool, Instant)>,
}

/// The record is rewritten once the position moved this much (about every
/// poll while playing, not while paused).
const RECORD_STEP_MS: u64 = 500;
/// The record is rewritten at least this often, so its write time means
/// "last seen" even while the film is paused.
const RECORD_HEARTBEAT: Duration = Duration::from_secs(60);
/// A record older than this is ignored: whatever MC plays now was started
/// some other way.
const RECORD_MAX_AGE: Duration = Duration::from_secs(12 * 3600);
/// A film MC no longer plays is reported stopped only if its record is
/// this recent. Later, the user may have watched on elsewhere, and an old
/// position would overwrite the newer resume point; the server keeps the
/// last progress report's position anyway.
const RECORD_STOP_MAX_AGE: Duration = Duration::from_secs(600);
/// A run ending on a reload or a new login waits this long for its restart
/// check (record, MCWS and server calls, each with its own timeout).
const RESTART_CHECK_WAIT: Duration = Duration::from_secs(30);

enum State {
  Idle,
  Starting {
    session: Session,
    generation: u64,
    task: JoinHandle<()>,
  },
  Active(Active),
}

/// What the run loop does with its poll timer after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollHint {
  Keep,
  /// Poll right away (a pause or resume should show within about 1 s).
  Now,
  /// Playback just started: first poll one interval from now.
  Restart,
}

/// What a poll found, decided while `self.state` is borrowed.
enum PollOutcome {
  Nothing,
  Report(ProgressReport),
  /// `minimize`: the playback ended the way a viewer ends a film (stopped
  /// in JRiver or played to the end), not by MC switching files or vanishing.
  End {
    why: String,
    minimize: bool,
  },
}

/// What the bridge is doing, for the tray icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStatus {
  Idle,
  Starting,
  Playing {
    name: String,
    paused: bool,
  },
  /// MC hung and is being closed and started again.
  RestartingJRiver,
}

/// A remaining-time margin within which a stop counts as "played to the
/// end" in the log. Jellyfin decides played status itself.
const END_MARGIN_MS: u64 = 10_000;

/// Registration retries after a connect: capped backoff, about 25 minutes in
/// all. A later reconnect starts over.
const REGISTER_BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30, 60];
const MAX_REGISTER_ATTEMPTS: usize = 30;

pub struct Controller<P, L> {
  player: Arc<P>,
  library: Arc<L>,
  mapper: Arc<PathMapper>,
  reporter: Reporter,
  options: ControllerOptions,
  events: mpsc::Sender<Event>,
  state: State,
  generation: u64,
  register_task: Option<JoinHandle<()>>,
  minimize_task: Option<JoinHandle<()>>,
  status: watch::Sender<PlaybackStatus>,
  /// The check for a film left over from a killed bridge runs once, on the
  /// first connect.
  recovery_started: bool,
  /// The check has started and its result isn't handled yet.
  recovery_pending: bool,
  recovery_task: Option<JoinHandle<()>>,
  watchdog: Watchdog,
  /// The film being played, kept where a restarted bridge finds it.
  record: Arc<dyn RecordStore>,
  /// See [`Self::with_reload_check`].
  reload_keeps_target: Box<dyn Fn() -> bool + Send + Sync>,
  /// Set once a registration as a cast target succeeded (for starting
  /// Jellyfin Media Player after it).
  registered: watch::Sender<bool>,
  /// See [`Self::with_socket_up`].
  socket_up: bool,
  /// See [`Self::with_carry`].
  carry: Arc<std::sync::Mutex<Option<Carry>>>,
  /// Without a record, ask the server for a film left over (not within a
  /// Jellyfin session a reload kept: the server's film is this bridge's own).
  server_recovery: bool,
  /// The first generation of this run: task results from before it belong
  /// to the last run in the session and are dropped.
  first_generation: u64,
}

/// What a controller hands the next one when a reload keeps the Jellyfin
/// session (SPEC.md "Settings"): the playing film, in memory, so the
/// remote's commands keep working throughout.
#[derive(Debug)]
pub struct Carry {
  active: Option<Active>,
  generation: u64,
  recovery_started: bool,
  recovery_pending: bool,
  /// Hang detection goes on as it was (a silence, the restart limit, MC's
  /// start-up grace).
  watchdog: Watchdog,
}

/// Where a reload's [`Carry`] waits for the next controller.
pub type CarrySlot = Arc<std::sync::Mutex<Option<Carry>>>;

impl<P: Player, L: Library> Controller<P, L> {
  /// `events` is the sender side of the channel [`Self::run`] reads; the
  /// controller keeps a clone for its start tasks.
  pub fn new(
    player: Arc<P>,
    library: Arc<L>,
    mapper: Arc<PathMapper>,
    reporter: Reporter,
    options: ControllerOptions,
    events: mpsc::Sender<Event>,
  ) -> Self {
    Self {
      player,
      library,
      mapper,
      reporter,
      options,
      events,
      state: State::Idle,
      generation: 0,
      register_task: None,
      minimize_task: None,
      status: watch::channel(PlaybackStatus::Idle).0,
      recovery_started: false,
      recovery_pending: false,
      recovery_task: None,
      watchdog: Watchdog {
        silent_since: None,
        not_responding_since: None,
        // MC may be loading as the bridge starts (both start at logon):
        // grace until its first answer.
        mc_appeared: Some(Instant::now()),
        mc_down: false,
        start_timed_out: None,
        silence_logged: false,
        limit_logged: false,
        check_error_logged: false,
        restarts: std::collections::VecDeque::new(),
        last_resume: None,
        failed_start: None,
        last_check: Instant::now(),
      },
      record: Arc::new(MemoryRecord::default()),
      reload_keeps_target: Box::new(|| true),
      registered: watch::Sender::new(false),
      socket_up: false,
      carry: CarrySlot::default(),
      server_recovery: true,
      first_generation: 0,
    }
  }

  /// The Jellyfin socket is already connected (a reload kept it), so no
  /// [`Event::Connected`] will come: [`Self::run`] acts on one at once
  /// (registers again, and reports a film carried over).
  pub fn with_socket_up(mut self, up: bool) -> Self {
    self.socket_up = up;
    self
  }

  /// Controllers within one Jellyfin session share `slot`. A run that ends
  /// on [`Event::Reload`] leaves its playing film there (and its counters);
  /// the next run starts with it, so nothing is left untracked in between.
  pub fn with_carry(mut self, slot: CarrySlot) -> Self {
    self.carry = slot;
    self
  }

  /// Start from what the last run in this session left (see
  /// [`Self::with_carry`]).
  fn take_carry(&mut self) {
    let carried = self
      .carry
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .take();
    let Some(carry) = carried else {
      return;
    };
    // Results of the last run's tasks that were already sent are dropped
    // (see `on_recovery` and `on_start_finished`).
    self.generation = carry.generation + 1;
    self.first_generation = self.generation;
    let mut watchdog = carry.watchdog;
    watchdog.last_check = Instant::now();
    self.watchdog = watchdog;
    if carry.recovery_started || carry.active.is_some() {
      // The session's film on the server is this bridge's own. (A carried
      // film means the socket was up, so the check started.)
      self.server_recovery = false;
    }
    if carry.recovery_pending {
      // The last run's check for a film from before a restart didn't
      // finish within `RESTART_CHECK_WAIT`: check the record again, now
      // (the socket needn't be up). Until it's handled the record keeps
      // that film (a carried film's writes wait, as they did in the last
      // run), and the check reports it stopped if a carried film replaced
      // it.
      self.recovery_started = true;
      self.start_recovery();
    } else if carry.recovery_started || carry.active.is_some() {
      // The check is done. Checking again could race the remote: a Stop
      // meanwhile would be reported twice.
      self.recovery_started = true;
    }
    if let Some(active) = carry.active {
      info!(
        "carrying item {} on at {} with the new settings",
        active.session.item_id,
        Millis(active.position_ms)
      );
      self.publish_playing(&active);
      self.state = State::Active(active);
      self.save_record(true);
    }
  }

  /// Keep the playback record here (a file, in the bridge) instead of in
  /// memory only.
  pub fn with_record(mut self, record: Arc<dyn RecordStore>) -> Self {
    self.record = record;
    self
  }

  /// On a reload: does the new config keep the Jellyfin account and MC zone
  /// of a playing film? If not, the film is stopped and reported, as on
  /// Quit, instead of handed over. (Default: it does.)
  pub fn with_reload_check(mut self, check: Box<dyn Fn() -> bool + Send + Sync>) -> Self {
    self.reload_keeps_target = check;
    self
  }

  /// Set `registered` to true once a registration as a cast target
  /// succeeds.
  pub fn with_registered(mut self, registered: watch::Sender<bool>) -> Self {
    self.registered = registered;
    self
  }

  /// Publish what the bridge is doing (for the tray icon).
  pub fn with_status(mut self, status: watch::Sender<PlaybackStatus>) -> Self {
    status.send_replace(PlaybackStatus::Idle);
    self.status = status;
    self
  }

  /// Handle events, poll JRiver while playing and check it's alive
  /// otherwise, until [`Event::Shutdown`], [`Event::Logout`] or
  /// [`Event::LoginChanged`].
  pub async fn run(mut self, events: &mut mpsc::Receiver<Event>) -> RunEnd {
    let mut poll = tokio::time::interval(self.options.poll_interval);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut windows = tokio::time::interval(self.options.windows_check_interval);
    windows.set_missed_tick_behavior(MissedTickBehavior::Delay);
    self.watchdog.last_check = Instant::now();
    self.take_carry();
    if self.socket_up {
      self.handle(Event::Connected).await;
    }
    let end = loop {
      let active = matches!(self.state, State::Active(_));
      let check_every = if matches!(self.state, State::Starting { .. }) {
        self.options.poll_interval
      } else {
        self.options.idle_check_interval
      };
      let next_check = self.watchdog.last_check + check_every;
      tokio::select! {
        event = events.recv() => {
          let event = match event {
            None | Some(Event::Shutdown) => break RunEnd::Shutdown,
            Some(Event::Logout) => break RunEnd::Logout,
            Some(Event::LoginChanged) => break RunEnd::LoginChanged,
            Some(Event::Reload) => break RunEnd::Reload,
            Some(event) => event,
          };
          match self.handle(event).await {
            PollHint::Keep => {}
            PollHint::Now => poll.reset_immediately(),
            PollHint::Restart => poll.reset(),
          }
        }
        _ = poll.tick(), if active => self.poll().await,
        () = tokio::time::sleep_until(next_check), if !active => self.health_check().await,
        _ = windows.tick() => self.watch_windows().await,
      }
    };
    let end = self.finish_restart_check(end, events).await;
    for task in [self.register_task.take(), self.recovery_task.take()]
      .into_iter()
      .flatten()
    {
      task.abort();
    }
    let hand_over = match end {
      RunEnd::LoginChanged => true,
      RunEnd::Reload if (self.reload_keeps_target)() => true,
      RunEnd::Reload => {
        if !matches!(self.state, State::Idle) {
          info!(
            "the new settings change the Jellyfin server or MC's zone; stopping the film as on Quit"
          );
        }
        false
      }
      RunEnd::Shutdown | RunEnd::Logout => false,
    };
    let carried = if hand_over {
      self.hand_over(end).await
    } else {
      self.shut_down().await;
      None
    };
    self.record.flush().await;
    if end == RunEnd::Reload {
      *self
        .carry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Carry {
        active: carried,
        generation: self.generation,
        recovery_started: self.recovery_started,
        recovery_pending: self.recovery_pending,
        watchdog: self.watchdog,
      });
    }
    end
  }

  /// A run ending on a reload hands its film over. While the restart check
  /// runs, the record still holds the film from before the restart and a
  /// film cast meanwhile isn't recorded, so the check is finished first
  /// (within `RESTART_CHECK_WAIT`), events handled as usual, and a cast that
  /// is starting then may finish too. A Quit or Log out meanwhile ends the
  /// run at once instead; a new login is passed on to the next run.
  ///
  /// Not on a new login: its old token is already revoked, so the check's
  /// report would be lost. The record is left for the next login's check, as
  /// before (a film cast during the check isn't recorded then: SPEC.md "The bridge killed").
  async fn finish_restart_check(
    &mut self,
    end: RunEnd,
    events: &mut mpsc::Receiver<Event>,
  ) -> RunEnd {
    if end != RunEnd::Reload || !self.recovery_pending {
      return end;
    }
    info!("finishing the check for a film from before the restart first");
    let deadline = Instant::now() + RESTART_CHECK_WAIT;
    let mut login_changed = false;
    let end = loop {
      if !self.recovery_pending && !matches!(self.state, State::Starting { .. }) {
        break end;
      }
      let event = match tokio::time::timeout_at(deadline, events.recv()).await {
        Err(_) => {
          if self.recovery_pending {
            warn!(
              "the check for a film from before the restart didn't finish in {RESTART_CHECK_WAIT:?}; the next run checks again"
            );
          }
          break end;
        }
        Ok(None | Some(Event::Shutdown)) => break RunEnd::Shutdown,
        Ok(Some(Event::Logout)) => break RunEnd::Logout,
        Ok(Some(Event::LoginChanged)) => {
          login_changed = true;
          continue;
        }
        // Again: this run already ends on it.
        Ok(Some(Event::Reload)) => continue,
        Ok(Some(event)) => event,
      };
      self.handle(event).await;
    };
    if login_changed {
      // The channel is shared with the next run.
      if self.events.try_send(Event::LoginChanged).is_err() {
        warn!("can't pass the new login on to the next run");
      }
    }
    end
  }

  /// A playback that just ended may still be waiting to minimize MC: let it
  /// finish rather than leave MC in front.
  async fn finish_minimize(&mut self) {
    if let Some(task) = self.minimize_task.take() {
      let wait = self.options.minimize_wait + Duration::from_secs(1);
      if tokio::time::timeout(wait, task).await.is_err() {
        warn!("JRiver: the pending minimize didn't finish before exit");
      }
    }
  }

  /// A new login: the old token is already revoked, so nothing can be
  /// reported with it. A playing film plays on; its record lets the next
  /// run take it over (same user) or stop it (another user). A start in
  /// progress is cancelled.
  ///
  /// Returns the playing film, for a next run in the same Jellyfin session
  /// (see [`Self::with_carry`]).
  async fn hand_over(&mut self, end: RunEnd) -> Option<Active> {
    let why = if end == RunEnd::Reload {
      "the settings were saved"
    } else {
      "a new login was saved"
    };
    self.finish_minimize().await;
    match self.state {
      State::Idle => None,
      State::Starting { .. } => {
        self.end(why).await;
        self.stop_jriver().await;
        None
      }
      State::Active(_) => {
        self.refresh_position().await;
        self.save_record(true);
        // On a reload the film is carried on (or taken over after a full
        // restart, which shows its own status): the tray goes on showing it.
        if end != RunEnd::Reload {
          self.status.send_replace(PlaybackStatus::Idle);
        }
        let State::Active(active) = std::mem::replace(&mut self.state, State::Idle) else {
          unreachable!("checked above");
        };
        info!(
          "{why}; item {} plays on at {} for the next run to take over",
          active.session.item_id,
          Millis(active.position_ms)
        );
        Some(active)
      }
    }
  }

  /// Quit (SPEC: stop JRiver, then report Stopped at the last known
  /// position), and minimize MC as after any playback.
  async fn shut_down(&mut self) {
    self.finish_minimize().await;
    if matches!(self.state, State::Idle) {
      return;
    }
    self.refresh_position().await;
    let full_screen = if matches!(self.state, State::Starting { .. }) {
      // Cancel the start first, so it can't set MC playing after the stop.
      let full_screen = self.end("the bridge is shutting down").await;
      self.stop_jriver().await;
      full_screen
    } else {
      self.stop_jriver().await;
      self.end("the bridge is shutting down").await
    };
    if full_screen && self.options.minimize_after_playback {
      minimize_once_out_of_display_view(&*self.player, self.options.minimize_wait).await;
    }
  }

  async fn stop_jriver(&self) {
    if let Err(err) = self.player.stop().await {
      warn!("JRiver: stop failed: {err}");
    }
  }

  /// After a playback that was full screen ends: minimize MC in the
  /// background once it has left Display view. A new Play cancels it.
  fn minimize_after_playback(&mut self, full_screen: bool) {
    if !(full_screen && self.options.minimize_after_playback) {
      return;
    }
    self.cancel_minimize();
    let player = self.player.clone();
    let wait = self.options.minimize_wait;
    self.minimize_task = Some(tokio::spawn(async move {
      minimize_once_out_of_display_view(&*player, wait).await;
    }));
  }

  fn cancel_minimize(&mut self) {
    if let Some(task) = self.minimize_task.take() {
      task.abort();
    }
  }

  async fn handle(&mut self, event: Event) -> PollHint {
    match event {
      Event::Command(JellyfinCommand::Play(request)) => {
        self.on_play(request).await;
        PollHint::Keep
      }
      Event::Command(JellyfinCommand::Playstate(request)) => self.on_playstate(request).await,
      Event::Command(JellyfinCommand::GeneralCommand(command)) => {
        self.on_general_command(command).await;
        PollHint::Keep
      }
      Event::Connected => {
        self.register();
        if !self.recovery_started {
          self.recovery_started = true;
          self.start_recovery();
        }
        if let State::Active(active) = &mut self.state {
          active.last_report = Instant::now();
          self
            .reporter
            .send(Report::Progress(progress_report(active)));
        }
        PollHint::Keep
      }
      Event::ConnectionLost => {
        warn!("Jellyfin: connection lost; playback continues and reports wait for the server");
        PollHint::Keep
      }
      Event::StartFinished { generation, result } => self.on_start_finished(generation, result),
      Event::RecoveryFinished {
        generation,
        recovery,
      } => self.on_recovery(generation, recovery).await,
      // Handled by the run loop.
      Event::Shutdown | Event::Logout | Event::LoginChanged | Event::Reload => PollHint::Keep,
    }
  }

  /// After a restart of the bridge, find out whether a film was left over:
  /// from the bridge's own record, or else from the server (SPEC.md "The
  /// bridge killed"). Runs in its own task.
  ///
  /// The record stays on disk until the result is handled, so a Quit or a
  /// new login meanwhile leaves it for the next run.
  fn start_recovery(&mut self) {
    let record = self.record.clone();
    let generation = self.generation;
    let player = self.player.clone();
    let library = self.library.clone();
    let mapper = self.mapper.clone();
    let events = self.events.clone();
    let server = self.server_recovery;
    self.recovery_pending = true;
    self.recovery_task = Some(tokio::spawn(async move {
      let found = match tokio::task::spawn_blocking(move || record.load()).await {
        Ok(found) => found,
        Err(err) => {
          warn!("reading the playback record failed: {err}");
          None
        }
      };
      // In its own task, so even a panic ends the check (and lets record
      // writes resume) instead of leaving it pending for the whole login.
      let check =
        tokio::spawn(async move { recover(&*player, &*library, &mapper, found, server).await });
      let recovery = match check.await {
        Ok(recovery) => recovery,
        Err(err) => {
          error!("the check for a film left over from before the restart failed: {err}");
          Recovery::Nothing
        }
      };
      let _ = events
        .send(Event::RecoveryFinished {
          generation,
          recovery,
        })
        .await;
    }));
  }

  /// Act on the restart check, then replace the record with the playback
  /// that now owns it, or delete it. (Until now the record was left as the
  /// check read it: see [`Self::save_record`].)
  async fn on_recovery(&mut self, generation: u64, recovery: Recovery) -> PollHint {
    if generation < self.first_generation {
      // The last run's check, sent just before it ended: this run checks
      // again itself.
      debug!("ignoring the restart check of the last run");
      return PollHint::Keep;
    }
    self.recovery_pending = false;
    let hint = if generation != self.generation || !matches!(self.state, State::Idle) {
      self.on_recovery_after_play(recovery);
      PollHint::Keep
    } else {
      self.act_on_recovery(recovery).await
    };
    if matches!(self.state, State::Active(_)) {
      self.save_record(true);
    } else {
      self.record.clear();
    }
    hint
  }

  /// A Play came in during the restart check, so the left-over film is
  /// gone from MC (or about to be): only its stop is still owed.
  fn on_recovery_after_play(&mut self, recovery: Recovery) {
    let current = match &self.state {
      State::Starting { session, .. } => Some(session.play_session_id.as_str()),
      State::Active(active) => Some(active.session.play_session_id.as_str()),
      State::Idle => None,
    };
    let report = match recovery {
      Recovery::Stale(report) => report,
      Recovery::TakeOver {
        item_id,
        media_source_id,
        play_session_id: Some(play_session_id),
        status,
        ..
      } => StopReport {
        item_id,
        media_source_id,
        play_session_id,
        position_ticks: Millis(status.position_ms).to_ticks(),
        failed: false,
      },
      _ => {
        debug!("a Play arrived during the restart check; nothing left over to report");
        return;
      }
    };
    // The record may already be the new film's (it's read in the background).
    if current == Some(report.play_session_id.as_str()) {
      return;
    }
    info!(
      "a Play arrived during the restart check; reporting item {} from before the restart stopped at {}",
      report.item_id,
      report.position_ticks.to_millis()
    );
    self.reporter.send(Report::Stopped(report));
  }

  async fn act_on_recovery(&mut self, recovery: Recovery) -> PollHint {
    match recovery {
      Recovery::Nothing => PollHint::Keep,
      Recovery::Stale(report) => {
        self.reporter.send(Report::Stopped(report));
        PollHint::Keep
      }
      Recovery::OtherUser { item_id, playing } => {
        if playing {
          warn!(
            "JRiver still plays item {item_id}, cast under another Jellyfin user before the new login; stopping it"
          );
          self.stop_jriver().await;
        } else {
          debug!("item {item_id} of another Jellyfin user no longer plays");
        }
        PollHint::Keep
      }
      Recovery::TakeOver {
        item_id,
        media_source_id,
        play_session_id,
        name,
        path,
        status,
        tracks,
        streams,
      } => {
        let ui_mode = self.player.ui_mode().await.ok();
        let paused = status.state == PlayState::Paused;
        info!(
          "JRiver is still playing item {item_id} from before the bridge restarted; taking it over at {}{}",
          Millis(status.position_ms),
          if paused { " (paused)" } else { "" }
        );
        let active = Active {
          session: Session {
            item_id,
            media_source_id,
            play_session_id: play_session_id.unwrap_or_else(new_play_session_id),
            start_ms: status.position_ms,
            tracks,
            automatic: false,
          },
          name,
          path,
          streams,
          tracks,
          file_key: status.file_key,
          duration_ms: status.duration_ms,
          position_ms: status.position_ms,
          paused,
          display_view: ui_mode == Some(UI_MODE_DISPLAY),
          ui_mode_failed: false,
          last_report: Instant::now(),
          missed_polls: 0,
          recorded: None,
        };
        self.reporter.send(Report::Start(progress_report(&active)));
        self.publish_playing(&active);
        self.state = State::Active(active);
        self.save_record(true);
        PollHint::Restart
      }
    }
  }

  /// The check while no film plays (and while one starts), and when Windows
  /// says MC isn't responding: `Playback/Info`, given at most
  /// `probe_timeout`.
  async fn health_check(&mut self) {
    let result = capped(self.options.probe_timeout, self.player.check_health()).await;
    self.watchdog.last_check = Instant::now();
    if !matches!(
      result,
      Err(McwsError::Unreachable { .. } | McwsError::Timeout { .. })
    ) {
      self.health_error(result.as_ref().err());
    }
    self.note_reachable(!matches!(result, Err(McwsError::Unreachable { .. })));
    match result {
      Ok(()) => self.mcws_ok(),
      Err(McwsError::Timeout { .. }) => self.on_mcws_timeout().await,
      // Not running (refused) or answering with an error: not a hang.
      Err(err) => {
        debug!("JRiver health check: {err}");
        self.mcws_answered();
      }
    }
  }

  /// Every second: does Windows say MC isn't responding? That, held for
  /// `hang_confirm` while MCWS times out, means MC is hung (SPEC.md "JRiver
  /// hung"). Windows itself says so only after 5 s without an answer.
  ///
  /// Only this check restarts MC, right after asking Windows, so a restart
  /// never rests on an old answer.
  async fn watch_windows(&mut self) {
    if self.mc_starting() {
      self.watchdog.not_responding_since = None;
      return;
    }
    match self.player.not_responding().await {
      // Windows can't tell (tasklist failed): that breaks "3 s in a row",
      // but isn't "responds" either (no log, the limit's log stays). The
      // 30 s rule then still ends a playing film.
      None => {
        self.watchdog.not_responding_since = None;
        return;
      }
      Some(false) => {
        let was_hung = self.watchdog.not_responding_since.take().is_some();
        if was_hung || std::mem::take(&mut self.watchdog.limit_logged) {
          info!("JRiver: Windows says MC responds again");
        }
        return;
      }
      Some(true) => {}
    }
    if self.watchdog.not_responding_since.is_none() {
      self.watchdog.not_responding_since = Some(Instant::now());
      if !self.over_restart_limit() {
        warn!(
          "Windows says JRiver isn't responding; restarting it if that lasts {:?} and MCWS doesn't answer",
          self.hang_confirm_now()
        );
      }
    }
    // Over the restart limit, once the hang is logged, MC is left alone.
    let left_alone = self.over_restart_limit() && self.watchdog.limit_logged;
    if self.watchdog.silent_since.is_none() && !left_alone {
      // No MCWS timeout seen yet: ask MCWS now. That takes up to 2 s, so
      // the next check decides, with a fresh answer from Windows.
      self.health_check().await;
      return;
    }
    self.restart_if_hung().await;
  }

  /// MC refused the connection (not running), or answered or timed out
  /// (running). Running again after not running: it may still be loading.
  fn note_reachable(&mut self, reachable: bool) {
    if !reachable {
      self.watchdog.mc_down = true;
    } else if std::mem::take(&mut self.watchdog.mc_down) {
      self.watchdog.mc_appeared = Some(Instant::now());
    }
  }

  /// How long "Not responding" must last now: `hang_confirm_starting` while
  /// a cast starts, or just after its start timed out (MC freezing while it
  /// opens a file from sleeping disks makes the start fail after 5 s);
  /// `hang_confirm` otherwise.
  fn hang_confirm_now(&self) -> Duration {
    let starting = matches!(self.state, State::Starting { .. })
      || self
        .watchdog
        .start_timed_out
        .is_some_and(|at| at.elapsed() < self.options.hang_confirm_starting);
    if starting {
      self.options.hang_confirm_starting
    } else {
      self.options.hang_confirm
    }
  }

  /// Restart MC once Windows has said "Not responding" for
  /// [`Self::hang_confirm_now`] and MCWS hasn't answered since it last
  /// timed out.
  async fn restart_if_hung(&mut self) {
    let confirm = self.hang_confirm_now();
    let confirmed = self
      .watchdog
      .not_responding_since
      .is_some_and(|since| since.elapsed() >= confirm);
    if confirmed && self.watchdog.silent_since.is_some() && !self.mc_starting() {
      self.restart_hung_mc().await;
    }
  }

  /// MC is starting up: launched by the bridge and not answering yet, or
  /// seen coming back after not running (by the user, or at logon) and not
  /// yet answered `Playback/Info`. A slow start-up is not a hang.
  fn mc_starting(&self) -> bool {
    self.player.launching()
      || self
        .watchdog
        .mc_appeared
        .is_some_and(|at| at.elapsed() < MC_STARTUP_GRACE)
  }

  /// Restarted `max_restarts_per_hour` times in the last hour already.
  fn over_restart_limit(&mut self) -> bool {
    let now = Instant::now();
    self
      .watchdog
      .restarts
      .retain(|at| now.duration_since(*at) < HOUR);
    self.watchdog.restarts.len() >= self.options.max_restarts_per_hour
  }

  /// A health check that failed with an answer (not a timeout or refusal):
  /// while that lasts, a hang can't be seen, so it's a warning (once).
  /// `None` when the check answered again.
  fn health_error(&mut self, err: Option<&McwsError>) {
    match err {
      Some(err) if !self.watchdog.check_error_logged => {
        self.watchdog.check_error_logged = true;
        warn!("JRiver health check failed ({err}); a hang can't be detected while it fails");
      }
      Some(_) => {}
      None if self.watchdog.check_error_logged => {
        self.watchdog.check_error_logged = false;
        info!("JRiver health check answering again");
      }
      None => {}
    }
  }

  /// MCWS answered (or refused: not running).
  fn mcws_answered(&mut self) {
    if self.watchdog.silent_since.take().is_some() && self.watchdog.silence_logged {
      info!("JRiver: MCWS answering again");
    }
    self.watchdog.silence_logged = false;
  }

  /// MCWS answered a call successfully: MC is up, past any start-up.
  fn mcws_ok(&mut self) {
    self.watchdog.mc_appeared = None;
    self.watchdog.mc_down = false;
    self.watchdog.start_timed_out = None;
    self.mcws_answered();
  }

  /// An MCWS call timed out (a poll, a health check, a command from
  /// Jellyfin). With Windows saying "Not responding" for long enough, MC is
  /// hung: [`Self::watch_windows`] decides that, every second.
  async fn on_mcws_timeout(&mut self) {
    let since = *self.watchdog.silent_since.get_or_insert_with(Instant::now);
    if self.mc_starting() {
      debug!("JRiver: MCWS silent while MC is still starting; waiting");
      return;
    }
    if self.watchdog.not_responding_since.is_some() {
      return;
    }
    if since.elapsed() >= self.options.hang_after {
      self.on_mcws_silent().await;
    }
  }

  /// MCWS has been silent for `hang_after` while Windows says MC responds
  /// (or can't tell): no restart, but a playing film is ended as before.
  async fn on_mcws_silent(&mut self) {
    let silent = self.options.hang_after;
    if matches!(self.state, State::Active(_)) {
      self.watchdog.silent_since = None;
      self
        .end(&format!(
          "JRiver stopped answering for {silent:?} (Windows doesn't report it hung, so it isn't restarted)"
        ))
        .await;
    } else if !self.watchdog.silence_logged {
      self.watchdog.silence_logged = true;
      warn!("JRiver: MCWS hasn't answered for {silent:?}, but Windows doesn't report MC hung; not restarting it");
    }
  }

  /// SPEC.md "JRiver hung": close MC by force, start it again, and play the
  /// film that was playing or starting again, once.
  async fn restart_hung_mc(&mut self) {
    let not_responding_for = self
      .watchdog
      .not_responding_since
      .map_or(Duration::ZERO, |since| since.elapsed());
    let why = format!(
      "JRiver hung (Windows has said it isn't responding for {}s, and MCWS doesn't answer)",
      not_responding_for.as_secs()
    );
    if self.over_restart_limit() {
      // Once per hung period; the hang stays known (no new warnings) until
      // MC responds again or the hour is over.
      if !self.watchdog.limit_logged {
        self.watchdog.limit_logged = true;
        if self.options.max_restarts_per_hour == 0 {
          error!("{why}; restarting is turned off (watchdog.max_restarts_per_hour = 0)");
        } else {
          error!(
            "{why}; it was already restarted {} times in the last hour, so it's left alone",
            self.watchdog.restarts.len()
          );
        }
      }
      // A playing or starting film can't go on with MC hung. (Idle: nothing
      // to end, and no status to send every second.)
      if !matches!(self.state, State::Idle) {
        self.end(&why).await;
      }
      return;
    }
    let resume = self.resume_target();
    self.watchdog.limit_logged = false;
    self.watchdog.silent_since = None;
    self.watchdog.silence_logged = false;
    self.watchdog.not_responding_since = None;
    self.watchdog.start_timed_out = None;
    self.watchdog.restarts.push_back(Instant::now());
    self.cancel_minimize();
    self.end(&why).await;
    warn!("{why}; restarting it");
    // The restart runs inside the event loop: Stop, Play and Quit wait for
    // it (at most about 1.5 min). A Quit that gives up meanwhile can leave MC
    // closed; the next cast starts it again.
    self.status.send_replace(PlaybackStatus::RestartingJRiver);
    let restarted = self.player.restart().await;
    self.status.send_replace(PlaybackStatus::Idle);
    self.watchdog.last_check = Instant::now();
    if let Err(err) = restarted {
      error!("JRiver: restart failed: {err}");
      return;
    }
    info!("JRiver: restarted after the hang");
    let Some(target) = resume else {
      return;
    };
    // Only an automatic resume is limited: a cast the user just sent is
    // always played after the restart.
    if target.automatic
      && self
        .watchdog
        .last_resume
        .is_some_and(|at| at.elapsed() < self.options.resume_window)
    {
      warn!(
        "JRiver hung again within {:?} of resuming item {}; not resuming it again",
        self.options.resume_window, target.item_id
      );
      return;
    }
    self.watchdog.last_resume = Some(Instant::now());
    info!(
      "resuming item {} at {} after the restart",
      target.item_id,
      Millis(target.position_ms)
    );
    self
      .on_play(PlayRequest {
        item_ids: vec![target.item_id],
        start_position_ticks: Some(Millis(target.position_ms).to_ticks().0),
        play_command: "PlayNow".to_owned(),
        media_source_id: target.media_source_id,
        // The tracks in use before the hang (a new MC picks its defaults).
        audio_stream_index: target.tracks.audio,
        subtitle_stream_index: target.tracks.subtitles,
        start_index: None,
      })
      .await;
    if let State::Starting { session, .. } = &mut self.state {
      session.automatic = true;
    }
  }

  /// The film to play again after a restart: the one playing or starting,
  /// or one whose start failed just before.
  fn resume_target(&mut self) -> Option<ResumeTarget> {
    match &self.state {
      State::Active(active) => Some(ResumeTarget {
        item_id: active.session.item_id.clone(),
        media_source_id: active.session.media_source_id.clone(),
        position_ms: active.position_ms,
        tracks: active.tracks,
        automatic: active.session.automatic,
      }),
      State::Starting { session, .. } => Some(ResumeTarget {
        item_id: session.item_id.clone(),
        media_source_id: session.media_source_id.clone(),
        position_ms: session.start_ms,
        tracks: session.tracks,
        automatic: session.automatic,
      }),
      State::Idle => {
        // Long enough for the start-time hang time, which follows a start
        // that timed out, to run its course (it's a setting).
        let window =
          FAILED_START_WINDOW.max(self.options.hang_confirm_starting + Duration::from_secs(15));
        self
          .watchdog
          .failed_start
          .take()
          .filter(|(_, at)| at.elapsed() < window)
          .map(|(target, _)| target)
      }
    }
  }

  /// Register as a cast target in the background, retrying failures that
  /// may pass. A newer connection replaces an unfinished attempt.
  fn register(&mut self) {
    if let Some(previous) = self.register_task.take() {
      previous.abort();
    }
    let library = self.library.clone();
    let registered = self.registered.clone();
    self.register_task = Some(tokio::spawn(async move {
      for attempt in 0..MAX_REGISTER_ATTEMPTS {
        match library.register().await {
          Ok(()) => {
            info!("Jellyfin: registered as a cast target");
            registered.send_replace(true);
            return;
          }
          Err(err) if err.is_retryable() && attempt + 1 < MAX_REGISTER_ATTEMPTS => {
            let delay = Duration::from_secs(
              REGISTER_BACKOFF_SECS[attempt.min(REGISTER_BACKOFF_SECS.len() - 1)],
            );
            warn!("Jellyfin: {err}; retrying in {delay:?}");
            tokio::time::sleep(delay).await;
          }
          Err(err) => {
            error!("Jellyfin: {err}; the bridge won't show in the cast menu until it reconnects");
            return;
          }
        }
      }
    }));
  }

  async fn on_play(&mut self, request: PlayRequest) {
    if !request.play_command.eq_ignore_ascii_case("PlayNow") {
      warn!(
        "Jellyfin: ignoring Play command {:?}; only PlayNow is supported (queueing comes later)",
        request.play_command
      );
      return;
    }
    let index = request.start_index.unwrap_or(0);
    let Some(item_id) = usize::try_from(index)
      .ok()
      .and_then(|i| request.item_ids.get(i))
      .cloned()
    else {
      warn!(
        "Jellyfin: Play has no item at index {index} ({} item(s)); ignored",
        request.item_ids.len()
      );
      return;
    };
    if request.item_ids.len() > 1 {
      info!(
        "Jellyfin: Play sent a queue of {} items; playing item {index} only (queueing comes later)",
        request.item_ids.len()
      );
    }
    // The current playback ends here, and JRiver stops with it: the new start
    // can fail before it reaches JRiver (lookup, mapping, a missing file),
    // and the old film must not play on with nothing tracking it.
    // A film is coming: don't minimize MC under it.
    self.cancel_minimize();
    if matches!(self.state, State::Idle) && self.recovery_pending {
      // MC may still play the film left over from before a restart, which
      // nothing tracks yet: stop it, for the same reason.
      self.stop_jriver().await;
    }
    if !matches!(self.state, State::Idle) {
      self.refresh_position().await;
      self.end("a new Play arrived").await;
      self.stop_jriver().await;
    }

    let start_ms = Ticks(request.start_position_ticks.unwrap_or(0))
      .to_millis()
      .0;
    let media_source_id = request.media_source_id.filter(|id| !id.is_empty());
    let session = Session {
      item_id: item_id.clone(),
      media_source_id,
      play_session_id: new_play_session_id(),
      start_ms,
      tracks: TrackChoice {
        audio: request.audio_stream_index,
        subtitles: request.subtitle_stream_index,
      },
      automatic: false,
    };
    self.watchdog.failed_start = None;
    self.watchdog.start_timed_out = None;
    info!(
      "Jellyfin: Play {item_id} from {} (play session {}; audio stream {}, subtitle stream {})",
      Millis(start_ms),
      session.play_session_id,
      stream_label(session.tracks.audio),
      stream_label(session.tracks.subtitles),
    );

    self.generation += 1;
    let generation = self.generation;
    let player = self.player.clone();
    let library = self.library.clone();
    let mapper = self.mapper.clone();
    let events = self.events.clone();
    let exists_timeout = self.options.exists_timeout;
    let tracks_from_jellyfin = self.options.tracks_from_jellyfin;
    let wanted = session.clone();
    let task = tokio::spawn(async move {
      let result = start(
        &*player,
        &*library,
        &mapper,
        &wanted,
        exists_timeout,
        tracks_from_jellyfin,
      )
      .await;
      // Only fails when the controller is gone.
      let _ = events
        .send(Event::StartFinished { generation, result })
        .await;
    });
    self.state = State::Starting {
      session,
      generation,
      task,
    };
    self.status.send_replace(PlaybackStatus::Starting);
  }

  fn on_start_finished(
    &mut self,
    generation: u64,
    result: Result<Started, StartFailure>,
  ) -> PollHint {
    if !matches!(&self.state, State::Starting { generation: current, .. } if *current == generation)
    {
      debug!("ignoring the result of a cancelled start");
      return PollHint::Keep;
    }
    let State::Starting { mut session, .. } = std::mem::replace(&mut self.state, State::Idle)
    else {
      unreachable!("checked above");
    };
    match result {
      Ok(started) => {
        // MC answered the whole start.
        self.mcws_ok();
        let playback = started.playback;
        if session.media_source_id.is_none() {
          session.media_source_id = Some(started.target.media_source_id);
        }
        if !playback.position_confirmed {
          warn!(
            "JRiver: the start position {} wasn't confirmed; playing on from {}",
            Millis(session.start_ms),
            Millis(playback.position_ms)
          );
        }
        info!(
          "Playing {} at {} of {}",
          started.path,
          Millis(playback.position_ms),
          Millis(playback.duration_ms)
        );
        let active = Active {
          session,
          name: started.name,
          path: started.path,
          streams: started.target.streams,
          tracks: started.tracks,
          file_key: playback.file_key,
          duration_ms: playback.duration_ms,
          position_ms: playback.position_ms,
          paused: false,
          display_view: playback.display_view == Some(true),
          ui_mode_failed: false,
          last_report: Instant::now(),
          missed_polls: 0,
          recorded: None,
        };
        self.reporter.send(Report::Start(progress_report(&active)));
        self.publish_playing(&active);
        self.state = State::Active(active);
        self.save_record(true);
        PollHint::Restart
      }
      Err(failure) => {
        error!("Can't play item {}: {failure}", session.item_id);
        // MC not answering in time may be a hang: check right away rather
        // than at the next scheduled check.
        if matches!(
          failure,
          StartFailure::Player(StartError::Mcws(McwsError::Timeout { .. }))
            | StartFailure::Launch(LaunchError::Mcws(McwsError::Timeout { .. }))
        ) {
          self.watchdog.last_check = Instant::now()
            .checked_sub(self.options.idle_check_interval)
            .unwrap_or_else(Instant::now);
          self.watchdog.start_timed_out = Some(Instant::now());
        }
        // If MC turns out to be hung, this film is played again after the
        // restart, as one that was starting.
        if matches!(failure, StartFailure::Player(_) | StartFailure::Launch(_)) {
          self.watchdog.failed_start = Some((
            ResumeTarget {
              item_id: session.item_id.clone(),
              media_source_id: session.media_source_id.clone(),
              position_ms: session.start_ms,
              tracks: session.tracks,
              automatic: session.automatic,
            },
            Instant::now(),
          ));
        }
        self.reporter.send(Report::Stopped(stop_report(
          &session,
          session.start_ms,
          true,
        )));
        self.status.send_replace(PlaybackStatus::Idle);
        PollHint::Keep
      }
    }
  }

  /// The remote's `SetAudioStreamIndex` and `SetSubtitleStreamIndex`
  /// (SPEC.md "Select tracks"); other general commands are ignored.
  async fn on_general_command(&mut self, command: GeneralCommand) {
    let name = command.name.as_str();
    let kind = if name.eq_ignore_ascii_case("SetAudioStreamIndex") {
      TrackKind::Audio
    } else if name.eq_ignore_ascii_case("SetSubtitleStreamIndex") {
      TrackKind::Subtitle
    } else {
      info!("Jellyfin: ignoring GeneralCommand {name}");
      return;
    };
    if !self.options.tracks_from_jellyfin {
      info!("Jellyfin: {name} ignored: JRiver picks the tracks (setting)");
      return;
    }
    let Some(index) = command_index(command.arguments.as_ref()) else {
      warn!("Jellyfin: {name} without an index; ignored");
      return;
    };
    let active = match &self.state {
      State::Active(active) => active,
      State::Starting { .. } => {
        info!("Jellyfin: {name} ignored: the file is still starting");
        return;
      }
      State::Idle => {
        info!("Jellyfin: {name} ignored: nothing is playing");
        return;
      }
    };
    let Some(track) = tracks::jriver_track(&active.streams, kind, index) else {
      warn!(
        "Jellyfin: {} stream {index} has no JRiver track (unknown, beside an external subtitle file, a disc folder, or a film whose tracks couldn't be looked up); ignored",
        kind.label()
      );
      return;
    };
    match self.player.select_track(kind, track).await {
      Ok(()) => {
        info!(
          "JRiver: {} track {track} (Jellyfin stream {})",
          kind.label(),
          stream_label(Some(index))
        );
        self.mcws_ok();
        if let State::Active(active) = &mut self.state {
          active.tracks.set(kind, Some(index));
          active.last_report = Instant::now();
          self
            .reporter
            .send(Report::Progress(progress_report(active)));
        }
        // A takeover keeps the new tracks.
        self.save_record(true);
      }
      Err(err) => {
        warn!(
          "JRiver: picking {} track {track} failed: {err}",
          kind.label()
        );
        if matches!(err, McwsError::Timeout { .. }) {
          self.on_mcws_timeout().await;
        }
      }
    }
  }

  async fn on_playstate(&mut self, request: PlaystateRequest) -> PollHint {
    let command = request.command.as_str();
    if command.eq_ignore_ascii_case("Stop") {
      self.on_stop().await;
      return PollHint::Keep;
    }
    let known = ["Pause", "Unpause", "PlayPause", "Seek"]
      .iter()
      .any(|c| c.eq_ignore_ascii_case(command));
    if !known {
      info!("Jellyfin: Playstate {command} is not supported; ignored");
      return PollHint::Keep;
    }
    if !matches!(self.state, State::Active(_)) {
      let why = match self.state {
        State::Starting { .. } => "the file is still starting",
        _ => "nothing is playing",
      };
      info!("Jellyfin: {command} ignored: {why}");
      return PollHint::Keep;
    }

    if command.eq_ignore_ascii_case("Seek") {
      let Some(ticks) = request.seek_position_ticks else {
        warn!("Jellyfin: Seek without a position; ignored");
        return PollHint::Keep;
      };
      let target = Ticks(ticks).to_millis();
      match self.player.seek(target.0).await {
        Ok(_) => {
          info!("JRiver: seek to {target}");
          self.mcws_ok();
          if let State::Active(active) = &mut self.state {
            active.position_ms = target.0;
            active.last_report = Instant::now();
            self
              .reporter
              .send(Report::Progress(progress_report(active)));
          }
          self.save_record(true);
        }
        Err(err) => {
          warn!("JRiver: seek to {target} failed: {err}");
          if matches!(err, McwsError::Timeout { .. }) {
            self.on_mcws_timeout().await;
          }
        }
      }
      return PollHint::Keep;
    }

    let result = if command.eq_ignore_ascii_case("Pause") {
      self.player.pause().await
    } else if command.eq_ignore_ascii_case("Unpause") {
      self.player.resume().await
    } else {
      self.player.toggle_pause().await
    };
    match result {
      // The poll that follows sees the new state and reports it.
      Ok(()) => {
        self.mcws_ok();
        PollHint::Now
      }
      Err(err) => {
        warn!("JRiver: {command} failed: {err}");
        if matches!(err, McwsError::Timeout { .. }) {
          self.on_mcws_timeout().await;
        }
        PollHint::Keep
      }
    }
  }

  async fn on_stop(&mut self) {
    match self.state {
      State::Idle => info!("Jellyfin: Stop with nothing playing; ignored"),
      State::Starting { .. } => {
        self.end("Stop from Jellyfin while starting").await;
        self.stop_jriver().await;
      }
      State::Active(_) => {
        self.refresh_position().await;
        self.stop_jriver().await;
        let full_screen = self.end("Stop from Jellyfin").await;
        self.minimize_after_playback(full_screen);
      }
    }
  }

  /// Before ending a playback on a command, take the freshest position and
  /// full-screen state (Escape a moment before a Stop must count).
  async fn refresh_position(&mut self) {
    let State::Active(active) = &self.state else {
      return;
    };
    let file_key = active.file_key;
    let Ok(status) = self.player.status().await else {
      return;
    };
    if status.file_key != file_key
      || !matches!(status.state, PlayState::Playing | PlayState::Paused)
    {
      return;
    }
    let ui_mode = self.read_ui_mode().await;
    if let State::Active(active) = &mut self.state {
      active.position_ms = status.position_ms;
      if let Some(mode) = ui_mode {
        active.display_view = mode == UI_MODE_DISPLAY;
      }
    }
  }

  /// MC's UI mode, or `None` if the read failed. The first failure of a
  /// playback is a warning: the minimize decision then rests on an older
  /// reading.
  async fn read_ui_mode(&mut self) -> Option<i32> {
    let result = self.player.ui_mode().await;
    let State::Active(active) = &mut self.state else {
      return result.ok();
    };
    match result {
      Ok(mode) => {
        active.ui_mode_failed = false;
        Some(mode)
      }
      Err(err) => {
        if active.ui_mode_failed {
          debug!("JRiver: reading the UI mode failed again: {err}");
        } else {
          warn!("JRiver: reading the UI mode failed ({err}); keeping the last full-screen reading");
          active.ui_mode_failed = true;
        }
        None
      }
    }
  }

  /// One MCWS status poll while playing (SPEC: 1 Hz).
  async fn poll(&mut self) {
    if !matches!(self.state, State::Active(_)) {
      return;
    }
    let status = capped(self.options.probe_timeout, self.player.status()).await;
    // Whether MC is full screen decides if it may be minimized afterwards.
    // Read it only while MC still shows our film: after a stop it is gone.
    let ui_mode = match &status {
      Ok(s) if matches!(s.state, PlayState::Playing | PlayState::Paused) => {
        self.read_ui_mode().await
      }
      _ => None,
    };
    self.note_reachable(!matches!(status, Err(McwsError::Unreachable { .. })));
    match &status {
      Ok(_) => self.mcws_ok(),
      Err(McwsError::Timeout { .. }) => {}
      Err(_) => self.mcws_answered(),
    }
    let progress_interval = self.options.progress_interval;
    let max_missed = self.options.max_missed_polls;
    let State::Active(active) = &mut self.state else {
      return;
    };
    let was_paused = active.paused;
    let outcome = match status {
      // MC not answering in time may be a hang: the watchdog decides with
      // Windows' "Not responding" (SPEC.md "JRiver hung").
      Err(err @ McwsError::Timeout { .. }) => {
        if self.watchdog.silent_since.is_none() {
          warn!("JRiver: status poll timed out: {err}");
        } else {
          debug!("JRiver: status poll timed out again: {err}");
        }
        self.on_mcws_timeout().await;
        return;
      }
      Err(err) => {
        active.missed_polls += 1;
        if active.missed_polls == 1 {
          warn!("JRiver: status poll failed: {err}");
        } else {
          debug!("JRiver: status poll {} failed: {err}", active.missed_polls);
        }
        if active.missed_polls >= max_missed {
          PollOutcome::End {
            why: format!(
              "JRiver stopped answering ({} polls in a row failed)",
              active.missed_polls
            ),
            minimize: false,
          }
        } else {
          PollOutcome::Nothing
        }
      }
      Ok(status) => {
        if active.missed_polls > 0 {
          info!("JRiver: answering again");
        }
        active.missed_polls = 0;
        if status.file_key == active.file_key {
          if let Some(mode) = ui_mode {
            active.display_view = mode == UI_MODE_DISPLAY;
          }
        }
        poll_outcome(active, &status, progress_interval)
      }
    };
    if let State::Active(active) = &self.state {
      if active.paused != was_paused {
        self.publish_playing(active);
      }
    }
    match outcome {
      PollOutcome::Nothing => self.save_record(false),
      PollOutcome::Report(report) => {
        self.reporter.send(Report::Progress(report));
        self.save_record(false);
      }
      PollOutcome::End { why, minimize } => {
        let full_screen = self.end(&why).await;
        self.minimize_after_playback(minimize && full_screen);
      }
    }
  }

  /// Write the playing film to the record: always if `force`, otherwise once
  /// its position or pause state changed, or a minute after the last write
  /// (so its time stays "last seen" while paused).
  ///
  /// While the restart check runs, the record still holds the film from
  /// before the restart, which the check reads in the background: nothing
  /// overwrites or deletes it until the check is handled.
  fn save_record(&mut self, force: bool) {
    if self.recovery_pending {
      return;
    }
    let State::Active(active) = &mut self.state else {
      return;
    };
    let now = (active.position_ms, active.paused);
    let unchanged = matches!(
      active.recorded,
      Some((position, paused, at))
        if paused == now.1
          && position.abs_diff(now.0) < RECORD_STEP_MS
          && at.elapsed() < RECORD_HEARTBEAT
    );
    if unchanged && !force {
      return;
    }
    active.recorded = Some((now.0, now.1, Instant::now()));
    self.record.save(&PlaybackRecord {
      item_id: active.session.item_id.clone(),
      media_source_id: active.session.media_source_id.clone(),
      play_session_id: active.session.play_session_id.clone(),
      name: active.name.clone(),
      path: active.path.clone(),
      position_ms: active.position_ms,
      paused: active.paused,
      written_at: now_secs(),
      audio_stream_index: active.tracks.audio,
      subtitle_stream_index: active.tracks.subtitles,
    });
  }

  fn publish_playing(&self, active: &Active) {
    self.status.send_replace(PlaybackStatus::Playing {
      name: active.name.clone(),
      paused: active.paused,
    });
  }

  /// The single way out of a playback: report it once, return to idle.
  /// Returns whether MC was full screen (Display view) at the last check,
  /// which is when it may be minimized afterwards.
  async fn end(&mut self, why: &str) -> bool {
    self.status.send_replace(PlaybackStatus::Idle);
    if !matches!(self.state, State::Idle) && !self.recovery_pending {
      self.record.clear();
    }
    match std::mem::replace(&mut self.state, State::Idle) {
      State::Idle => false,
      State::Starting { session, task, .. } => {
        // Wait for the task to stop, so it can't go on driving MC.
        task.abort();
        let _ = task.await;
        info!("Start of item {} cancelled: {why}", session.item_id);
        self.reporter.send(Report::Stopped(stop_report(
          &session,
          session.start_ms,
          true,
        )));
        false
      }
      State::Active(active) => {
        info!(
          "Stopped item {} at {} of {}: {why}",
          active.session.item_id,
          Millis(active.position_ms),
          Millis(active.duration_ms)
        );
        self.reporter.send(Report::Stopped(stop_report(
          &active.session,
          active.position_ms,
          false,
        )));
        active.display_view
      }
    }
  }

  #[cfg(test)]
  fn state_name(&self) -> &'static str {
    match self.state {
      State::Idle => "idle",
      State::Starting { .. } => "starting",
      State::Active(_) => "active",
    }
  }
}

/// `call`, given at most `cap`; longer counts as an MCWS timeout.
async fn capped<T>(
  cap: Duration,
  call: impl Future<Output = Result<T, McwsError>>,
) -> Result<T, McwsError> {
  tokio::time::timeout(cap, call)
    .await
    .unwrap_or(Err(McwsError::Timeout {
      function: Function::PlaybackInfo,
      timeout: cap,
    }))
}

fn new_play_session_id() -> String {
  uuid::Uuid::new_v4().simple().to_string()
}

/// After a bridge restart: was a film left over, and does MC still play it?
/// The bridge's own record comes first; the server only knows the film
/// while the old bridge's socket is still open. Errors just mean nothing is
/// recovered. `server`: whether to ask the server when there's no record.
async fn recover<P: Player, L: Library>(
  player: &P,
  library: &L,
  mapper: &PathMapper,
  found: Option<Found>,
  server: bool,
) -> Recovery {
  if let Some(found) = &found {
    let record = found.record();
    if record.age() > RECORD_MAX_AGE {
      info!(
        "the playback record of item {} is {} h old; ignoring it",
        record.item_id,
        record.age().as_secs() / 3600
      );
      return Recovery::Nothing;
    }
  }
  match found {
    Some(Found::Mine(record)) => match playing(player, &record.path).await {
      Some(status) => Recovery::TakeOver {
        streams: streams_of(library, &record.item_id, record.media_source_id.as_deref()).await,
        tracks: TrackChoice {
          audio: record.audio_stream_index,
          subtitles: record.subtitle_stream_index,
        },
        item_id: record.item_id,
        media_source_id: record.media_source_id,
        play_session_id: Some(record.play_session_id),
        name: record.name,
        path: record.path,
        status,
      },
      None if record.age() > RECORD_STOP_MAX_AGE => {
        info!(
          "item {} was playing at {} when the bridge stopped {} min ago; JRiver no longer plays it, and the server keeps its last progress",
          record.item_id,
          Millis(record.position_ms),
          record.age().as_secs() / 60
        );
        Recovery::Nothing
      }
      None => {
        info!(
          "item {} was playing at {} when the bridge stopped, and JRiver no longer plays it; reporting it stopped there",
          record.item_id,
          Millis(record.position_ms)
        );
        Recovery::Stale(StopReport {
          item_id: record.item_id,
          media_source_id: record.media_source_id,
          play_session_id: record.play_session_id,
          position_ticks: Millis(record.position_ms).to_ticks(),
          failed: false,
        })
      }
    },
    Some(Found::OtherUser(record)) => Recovery::OtherUser {
      playing: playing(player, &record.path).await.is_some(),
      item_id: record.item_id,
    },
    None if server => recover_from_server(player, library, mapper).await,
    None => Recovery::Nothing,
  }
}

/// The tracks of a film taken over, so the remote can switch them: looked
/// up again, as the record doesn't keep them. Empty if that fails.
async fn streams_of<L: Library>(
  library: &L,
  item_id: &str,
  media_source_id: Option<&str>,
) -> Vec<MediaStream> {
  let target = match library.item(item_id).await {
    Ok(item) => select_target(&item, media_source_id).map_err(|err| err.to_string()),
    Err(err) => Err(err.to_string()),
  };
  match target {
    Ok(target) => target.streams,
    Err(err) => {
      warn!("Jellyfin: can't look up the tracks of item {item_id} ({err}); the remote can't switch them");
      Vec::new()
    }
  }
}

/// MC's status if it plays (or pauses) `path`.
async fn playing<P: Player>(player: &P, path: &str) -> Option<PlaybackInfo> {
  let status = player.status().await.ok()?;
  let plays = matches!(status.state, PlayState::Playing | PlayState::Paused)
    && status.has_file()
    && status
      .filename
      .as_deref()
      .is_some_and(|reported| same_media(path, reported));
  plays.then_some(status)
}

/// Without a record: does the server still show a film here, and is MC
/// still playing it?
async fn recover_from_server<P: Player, L: Library>(
  player: &P,
  library: &L,
  mapper: &PathMapper,
) -> Recovery {
  let now_playing = match library.now_playing().await {
    Ok(Some(now_playing)) => now_playing,
    Ok(None) => return Recovery::Nothing,
    Err(err) => {
      warn!("Jellyfin: can't check for a film left over from before the restart: {err}");
      return Recovery::Nothing;
    }
  };
  let status = match player.status().await {
    Ok(status)
      if matches!(status.state, PlayState::Playing | PlayState::Paused) && status.has_file() =>
    {
      status
    }
    _ => return server_stale(now_playing),
  };
  let Some(reported) = status.filename.clone() else {
    return server_stale(now_playing);
  };
  let item = match library.item(&now_playing.item_id).await {
    Ok(item) => item,
    Err(err) => {
      warn!(
        "Jellyfin: can't look up item {} left over from before the restart: {err}",
        now_playing.item_id
      );
      return server_stale(now_playing);
    }
  };
  let mapped = select_target(&item, now_playing.media_source_id.as_deref())
    .map_err(|err| err.to_string())
    .and_then(|target| {
      mapper
        .map(&target.server_path, target.shape)
        .map(|path| (target, path))
        .map_err(|err| err.to_string())
    });
  match mapped {
    Ok((target, path)) if same_media(&path, &reported) => Recovery::TakeOver {
      item_id: now_playing.item_id,
      media_source_id: Some(target.media_source_id),
      play_session_id: None,
      name: item.name.unwrap_or(item.id),
      path,
      status,
      // Not known: MC's own choice, or the old bridge's.
      tracks: TrackChoice::default(),
      streams: target.streams,
    },
    Ok((_, path)) => {
      info!("JRiver plays {reported}, not {path}; leaving it alone");
      server_stale(now_playing)
    }
    Err(err) => {
      warn!(
        "can't map item {} left over from before the restart: {err}",
        now_playing.item_id
      );
      server_stale(now_playing)
    }
  }
}

/// The server's film that MC no longer plays: reported stopped at the
/// server's last known position.
fn server_stale(now_playing: NowPlaying) -> Recovery {
  info!(
    "Jellyfin still showed item {} playing here from before the bridge restarted, but JRiver isn't playing it; reporting it stopped at {}",
    now_playing.item_id,
    Ticks(now_playing.position_ticks).to_millis()
  );
  Recovery::Stale(StopReport {
    item_id: now_playing.item_id,
    media_source_id: now_playing.media_source_id,
    play_session_id: new_play_session_id(),
    position_ticks: Ticks(now_playing.position_ticks),
    failed: false,
  })
}

/// Look up, map, check and start, then pick the tracks. Runs in its own task.
async fn start<P: Player, L: Library>(
  player: &P,
  library: &L,
  mapper: &PathMapper,
  wanted: &Session,
  exists_timeout: Duration,
  tracks_from_jellyfin: bool,
) -> Result<Started, StartFailure> {
  let item = library
    .item(&wanted.item_id)
    .await
    .map_err(StartFailure::Lookup)?;
  let target = select_target(&item, wanted.media_source_id.as_deref())?;
  let path = mapper.map(&target.server_path, target.shape)?;
  info!("Mapped {} ({}) -> {path}", target.server_path, target.shape);
  player.check_path(&path, exists_timeout).await?;
  player.ensure_running().await?;
  let playback = player.play(&path, wanted.start_ms).await?;
  let tracks = if tracks_from_jellyfin {
    select_start_tracks(player, &target.streams, wanted.tracks).await
  } else {
    // JRiver decides; which tracks it plays isn't known.
    TrackChoice::default()
  };
  Ok(Started {
    name: item.name.unwrap_or_else(|| item.id.clone()),
    target,
    path,
    playback,
    tracks,
  })
}

/// Apply Jellyfin's audio and subtitle choice once the file plays, where it
/// differs from the file's default (SPEC.md "Select tracks"). Returns the
/// streams then in use, as far as the bridge knows. A failure leaves JRiver's
/// own choice and doesn't fail the start.
async fn select_start_tracks<P: Player>(
  player: &P,
  streams: &[MediaStream],
  requested: TrackChoice,
) -> TrackChoice {
  let mut in_use = TrackChoice::default();
  for kind in [TrackKind::Audio, TrackKind::Subtitle] {
    let asked = requested.get(kind);
    let default = tracks::default_index(streams, kind);
    let now = match tracks::start_pick(streams, kind, asked) {
      StartPick::Keep => asked.or(default),
      StartPick::Select(track) => match player.select_track(kind, track).await {
        Ok(()) => {
          info!(
            "JRiver: {} track {track} (Jellyfin stream {})",
            kind.label(),
            stream_label(asked)
          );
          asked
        }
        Err(err) => {
          warn!(
            "JRiver: picking {} track {track} failed ({err}); JRiver's own choice stays",
            kind.label()
          );
          default
        }
      },
      StartPick::Unmapped(index) => {
        warn!(
          "Jellyfin: {} stream {index} has no JRiver track (unknown, beside an external subtitle file, a disc folder, or a film whose tracks couldn't be looked up); JRiver's own choice stays",
          kind.label()
        );
        default
      }
    };
    in_use.set(kind, now);
  }
  in_use
}

/// A Jellyfin stream index for the log (-1 is no subtitles).
fn stream_label(index: Option<i32>) -> String {
  match index {
    None => "not given".to_owned(),
    Some(tracks::NO_SUBTITLES) => "none".to_owned(),
    Some(index) => index.to_string(),
  }
}

/// The `Index` argument of a track command; Jellyfin sends it as a string.
fn command_index(arguments: Option<&serde_json::Value>) -> Option<i32> {
  match arguments?.get("Index")? {
    serde_json::Value::String(text) => text.trim().parse().ok(),
    serde_json::Value::Number(number) => number.as_i64().and_then(|n| i32::try_from(n).ok()),
    _ => None,
  }
}

/// Wait (up to `wait`) for MC to leave Display view after a stop, then
/// minimize it once. MC was full screen when the playback ended, so it isn't
/// minimized and the toggle minimizes.
async fn minimize_once_out_of_display_view<P: Player>(player: &P, wait: Duration) {
  let deadline = Instant::now() + wait;
  loop {
    match player.ui_mode().await {
      Ok(mode) if mode != UI_MODE_DISPLAY => break,
      Ok(_) if Instant::now() >= deadline => {
        warn!("JRiver: still in Display view {wait:?} after the stop; not minimizing");
        return;
      }
      Ok(_) => {}
      Err(err) => {
        warn!("JRiver: can't read the UI mode ({err}); not minimizing");
        return;
      }
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
  }
  match player.minimize().await {
    Ok(()) => info!("JRiver: minimized after the playback"),
    Err(err) => warn!("JRiver: minimize failed: {err}"),
  }
}

fn poll_outcome(
  active: &mut Active,
  status: &PlaybackInfo,
  progress_interval: Duration,
) -> PollOutcome {
  match status.state {
    // Right after some MC transitions the file key reads -1; wait it out.
    PlayState::Playing | PlayState::Paused if !status.has_file() => PollOutcome::Nothing,
    PlayState::Playing | PlayState::Paused if status.file_key == active.file_key => {
      active.position_ms = status.position_ms;
      if status.duration_ms > 0 {
        active.duration_ms = status.duration_ms;
      }
      let paused = status.state == PlayState::Paused;
      if paused != active.paused {
        active.paused = paused;
        info!(
          "JRiver: {} at {}",
          if paused { "paused" } else { "playing" },
          Millis(active.position_ms)
        );
      } else if active.last_report.elapsed() < progress_interval {
        return PollOutcome::Nothing;
      }
      active.last_report = Instant::now();
      PollOutcome::Report(progress_report(active))
    }
    PlayState::Playing | PlayState::Paused => PollOutcome::End {
      why: format!(
        "JRiver switched to another file ({})",
        status.filename.as_deref().unwrap_or("unknown")
      ),
      minimize: false,
    },
    PlayState::Stopped => {
      let near_end = active.duration_ms > 0
        && active.position_ms.saturating_add(END_MARGIN_MS) >= active.duration_ms;
      PollOutcome::End {
        why: if near_end {
          "played to the end"
        } else {
          "stopped in JRiver"
        }
        .to_owned(),
        minimize: true,
      }
    }
    PlayState::Opening | PlayState::Unknown(_) => PollOutcome::Nothing,
  }
}

fn progress_report(active: &Active) -> ProgressReport {
  ProgressReport {
    item_id: active.session.item_id.clone(),
    media_source_id: active.session.media_source_id.clone(),
    play_session_id: active.session.play_session_id.clone(),
    position_ticks: Millis(active.position_ms).to_ticks(),
    is_paused: active.paused,
    is_muted: false,
    can_seek: true,
    play_method: PlayMethod::DirectPlay,
    audio_stream_index: active.tracks.audio,
    subtitle_stream_index: active.tracks.subtitles,
  }
}

fn stop_report(session: &Session, position_ms: u64, failed: bool) -> StopReport {
  StopReport {
    item_id: session.item_id.clone(),
    media_source_id: session.media_source_id.clone(),
    play_session_id: session.play_session_id.clone(),
    position_ticks: Millis(position_ms).to_ticks(),
    failed,
  }
}

#[cfg(test)]
mod tests {
  use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
  };

  use jriver_mcws::Function;
  use tokio::sync::mpsc::error::TryRecvError;

  use super::*;
  use crate::{jellyfin::MediaSourceInfo, path_map::PathRule};

  const MOVIE: &str = "fc6b41643b6a10ebf8c0ef947a2d06c8";
  const EPISODE: &str = "76792f779d30cb6bf30127671ef566f6";
  const DISC: &str = "a05ea8c6650fb5db2c14d3511440ff5a";
  const ISO: &str = "0000000000000000000000000000150a";
  /// A film with tracks: video 0; audio 1 (default), 2 and 3; subtitles 4
  /// and 5.
  const TRACKS: &str = "000000000000000000000000000a0d10";

  #[derive(Debug, Clone, PartialEq, Eq)]
  enum Call {
    Check(String),
    Play(String, u64),
    Pause,
    Resume,
    Toggle,
    Seek(u64),
    Stop,
    Minimize,
    Restart,
    Track(TrackKind, i32),
  }

  struct FakePlayer {
    calls: Mutex<Vec<Call>>,
    /// Status answers in order; the last one repeats.
    statuses: Mutex<VecDeque<Result<PlaybackInfo, ()>>>,
    play_delay: Mutex<Duration>,
    fail_start: Mutex<bool>,
    fail_seek: Mutex<bool>,
    fail_tracks: Mutex<bool>,
    missing: Mutex<bool>,
    next_key: Mutex<i64>,
    /// MC's UI mode: 2 (Display view) while a film plays, 0 after a stop.
    ui: Mutex<i32>,
    /// MC stays in Display view after a stop (to hold a minimize back).
    stays_full_screen: Mutex<bool>,
    not_running: Mutex<bool>,
    /// Failed status answers (`Err(())`) are timeouts; otherwise refusals.
    errors_are_timeouts: Mutex<bool>,
    /// MC is hung: every MCWS call times out until a restart.
    hung: Mutex<bool>,
    /// What Windows says about MC.
    not_responding: Mutex<bool>,
    /// After a restart, MC is hung again (to test the resume limit).
    hangs_again: Mutex<bool>,
    /// The bridge has just started MC and waits for it.
    launching: Mutex<bool>,
    /// `tasklist` fails: Windows can't tell whether MC responds.
    windows_unknown: Mutex<bool>,
    /// `Playback/Info` health checks made.
    health_checks: Mutex<u32>,
  }

  impl FakePlayer {
    fn new() -> Arc<Self> {
      Arc::new(Self {
        calls: Mutex::new(Vec::new()),
        statuses: Mutex::new(VecDeque::new()),
        play_delay: Mutex::new(Duration::ZERO),
        fail_start: Mutex::new(false),
        fail_seek: Mutex::new(false),
        fail_tracks: Mutex::new(false),
        missing: Mutex::new(false),
        next_key: Mutex::new(100),
        ui: Mutex::new(0),
        stays_full_screen: Mutex::new(false),
        not_running: Mutex::new(false),
        errors_are_timeouts: Mutex::new(false),
        hung: Mutex::new(false),
        not_responding: Mutex::new(false),
        hangs_again: Mutex::new(false),
        launching: Mutex::new(false),
        windows_unknown: Mutex::new(false),
        health_checks: Mutex::new(0),
      })
    }

    fn hang(&self) {
      *self.hung.lock().unwrap() = true;
      *self.not_responding.lock().unwrap() = true;
    }

    /// A hung MC answers nothing for 5 s, then the call times out.
    async fn timeout_if_hung(&self) -> Result<(), McwsError> {
      if *self.hung.lock().unwrap() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Err(timeout_err());
      }
      Ok(())
    }

    fn calls(&self) -> Vec<Call> {
      self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: Call) {
      self.calls.lock().unwrap().push(call);
    }

    fn set_status(&self, statuses: Vec<Result<PlaybackInfo, ()>>) {
      *self.statuses.lock().unwrap() = statuses.into();
    }
  }

  fn info(state: PlayState, key: i64, pos: u64) -> PlaybackInfo {
    PlaybackInfo {
      state,
      file_key: key,
      position_ms: pos,
      duration_ms: 7_200_000,
      playing_now_tracks: 1,
      filename: Some(format!("file-{key}")),
      name: None,
    }
  }

  fn timeout_err() -> McwsError {
    McwsError::Timeout {
      function: Function::PlaybackInfo,
      timeout: Duration::from_secs(5),
    }
  }

  /// Connection refused (MC not running), without any network access.
  fn unreachable_err() -> McwsError {
    let source = reqwest::Client::new()
      .get("not a url")
      .build()
      .expect_err("an unparsable URL fails to build");
    McwsError::Unreachable {
      function: Function::PlaybackInfo,
      source,
    }
  }

  impl Player for FakePlayer {
    async fn check_path(&self, path: &str, _timeout: Duration) -> Result<(), ExistsError> {
      self.record(Call::Check(path.to_owned()));
      if *self.missing.lock().unwrap() {
        return Err(ExistsError::Missing {
          path: path.to_owned(),
        });
      }
      Ok(())
    }

    async fn ensure_running(&self) -> Result<bool, LaunchError> {
      if *self.not_running.lock().unwrap() {
        return Err(LaunchError::NotRunning(unreachable_err()));
      }
      Ok(false)
    }

    async fn ui_mode(&self) -> Result<i32, McwsError> {
      Ok(*self.ui.lock().unwrap())
    }

    async fn minimize(&self) -> Result<(), McwsError> {
      self.record(Call::Minimize);
      Ok(())
    }

    async fn select_track(&self, kind: TrackKind, track: i32) -> Result<(), McwsError> {
      self.record(Call::Track(kind, track));
      self.timeout_if_hung().await?;
      if *self.fail_tracks.lock().unwrap() {
        return Err(unreachable_err());
      }
      Ok(())
    }

    async fn play(&self, path: &str, start_ms: u64) -> Result<StartedPlayback, StartError> {
      self.record(Call::Play(path.to_owned(), start_ms));
      if *self.hung.lock().unwrap() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Err(StartError::Mcws(timeout_err()));
      }
      let delay = *self.play_delay.lock().unwrap();
      tokio::time::sleep(delay).await;
      if *self.fail_start.lock().unwrap() {
        return Err(StartError::StoppedWhileOpening);
      }
      *self.ui.lock().unwrap() = UI_MODE_DISPLAY;
      let key = {
        let mut next = self.next_key.lock().unwrap();
        *next += 1;
        *next
      };
      Ok(StartedPlayback {
        file_key: key,
        duration_ms: 7_200_000,
        position_ms: start_ms + 300,
        position_confirmed: true,
        display_view: Some(true),
        filename: Some(path.to_owned()),
      })
    }

    async fn pause(&self) -> Result<(), McwsError> {
      self.record(Call::Pause);
      self.timeout_if_hung().await?;
      Ok(())
    }

    async fn resume(&self) -> Result<(), McwsError> {
      self.record(Call::Resume);
      self.timeout_if_hung().await?;
      Ok(())
    }

    async fn toggle_pause(&self) -> Result<(), McwsError> {
      self.record(Call::Toggle);
      self.timeout_if_hung().await?;
      Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<u64, McwsError> {
      self.record(Call::Seek(position_ms));
      self.timeout_if_hung().await?;
      if *self.fail_seek.lock().unwrap() {
        return Err(unreachable_err());
      }
      Ok(position_ms)
    }

    async fn stop(&self) -> Result<(), McwsError> {
      self.record(Call::Stop);
      if !*self.stays_full_screen.lock().unwrap() {
        *self.ui.lock().unwrap() = 0;
      }
      Ok(())
    }

    async fn status(&self) -> Result<PlaybackInfo, McwsError> {
      if *self.hung.lock().unwrap() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Err(timeout_err());
      }
      let mut statuses = self.statuses.lock().unwrap();
      let next = if statuses.len() > 1 {
        statuses.pop_front()
      } else {
        statuses.front().cloned()
      };
      match next {
        Some(Ok(info)) => Ok(info),
        Some(Err(())) | None if *self.errors_are_timeouts.lock().unwrap() => Err(timeout_err()),
        Some(Err(())) | None => Err(unreachable_err()),
      }
    }

    async fn check_health(&self) -> Result<(), McwsError> {
      *self.health_checks.lock().unwrap() += 1;
      if *self.hung.lock().unwrap() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        return Err(timeout_err());
      }
      if *self.not_running.lock().unwrap() {
        return Err(unreachable_err());
      }
      Ok(())
    }

    async fn not_responding(&self) -> Option<bool> {
      if *self.windows_unknown.lock().unwrap() {
        return None;
      }
      Some(*self.not_responding.lock().unwrap())
    }

    async fn restart(&self) -> Result<(), LaunchError> {
      self.record(Call::Restart);
      let again = *self.hangs_again.lock().unwrap();
      *self.hung.lock().unwrap() = again;
      *self.not_responding.lock().unwrap() = again;
      Ok(())
    }

    fn launching(&self) -> bool {
      *self.launching.lock().unwrap()
    }
  }

  struct FakeLibrary {
    items: HashMap<String, Item>,
    registrations: Mutex<u32>,
    /// Registration attempts that fail with HTTP 503 before one succeeds.
    register_failures: Mutex<u32>,
    /// What the server shows as playing on this cast target.
    now_playing: Mutex<Option<NowPlaying>>,
  }

  fn file_item(id: &str, path: &str, video_type: &str) -> Item {
    Item {
      id: id.to_owned(),
      name: Some(format!("item {id}")),
      item_type: Some("Movie".to_owned()),
      media_type: Some("Video".to_owned()),
      path: Some(path.to_owned()),
      video_type: Some(video_type.to_owned()),
      run_time_ticks: Some(72_000_000_000),
      media_sources: Some(vec![MediaSourceInfo {
        id: id.to_owned(),
        path: Some(path.to_owned()),
        protocol: Some("File".to_owned()),
        video_type: Some(video_type.to_owned()),
        media_streams: Vec::new(),
      }]),
    }
  }

  fn with_tracks(mut item: Item) -> Item {
    let stream = |index: i32, kind: &str, default: bool, external: bool| MediaStream {
      index,
      kind: kind.to_owned(),
      is_default: default,
      is_external: external,
      ..MediaStream::default()
    };
    if let Some(source) = item.media_sources.as_mut().and_then(|s| s.first_mut()) {
      source.media_streams = vec![
        stream(0, "Video", true, false),
        stream(1, "Audio", true, false),
        stream(2, "Audio", false, false),
        stream(3, "Audio", false, false),
        stream(4, "Subtitle", false, false),
        stream(5, "Subtitle", false, false),
      ];
    }
    item
  }

  impl FakeLibrary {
    fn new() -> Arc<Self> {
      let items = [
        file_item(MOVIE, "/media/movies/Avatar/avatar.mkv", "VideoFile"),
        file_item(EPISODE, "/media/tv/Show/Season 1/e02.mkv", "VideoFile"),
        // The server lists a disc's tracks, but MC's order isn't known.
        with_tracks(file_item(
          DISC,
          "/media/movies/Star Trek/Into Darkness",
          "BluRay",
        )),
        file_item(ISO, "/media/movies/Aliens/aliens.iso", "Iso"),
        with_tracks(file_item(
          TRACKS,
          "/media/movies/Tracks/tracks.mkv",
          "VideoFile",
        )),
      ]
      .into_iter()
      .map(|item| (item.id.clone(), item))
      .collect();
      Arc::new(Self {
        items,
        registrations: Mutex::new(0),
        register_failures: Mutex::new(0),
        now_playing: Mutex::new(None),
      })
    }
  }

  impl Library for FakeLibrary {
    async fn item(&self, item_id: &str) -> Result<Item, JellyfinError> {
      self
        .items
        .get(item_id)
        .cloned()
        .ok_or(JellyfinError::Status {
          what: "look up the item",
          status: 404,
          detail: String::new(),
        })
    }

    async fn register(&self) -> Result<(), JellyfinError> {
      *self.registrations.lock().unwrap() += 1;
      let mut failures = self.register_failures.lock().unwrap();
      if *failures > 0 {
        *failures -= 1;
        return Err(JellyfinError::Status {
          what: "register as a cast target",
          status: 503,
          detail: String::new(),
        });
      }
      Ok(())
    }

    async fn now_playing(&self) -> Result<Option<NowPlaying>, JellyfinError> {
      Ok(self.now_playing.lock().unwrap().clone())
    }
  }

  struct Harness {
    ctrl: Controller<FakePlayer, FakeLibrary>,
    player: Arc<FakePlayer>,
    library: Arc<FakeLibrary>,
    record: Arc<MemoryRecord>,
    events: mpsc::Receiver<Event>,
    reports: mpsc::UnboundedReceiver<Report>,
  }

  fn harness() -> Harness {
    harness_sharing(FakePlayer::new(), Arc::new(MemoryRecord::default()))
  }

  /// A bridge started again: the same MC and playback record.
  fn harness_sharing(player: Arc<FakePlayer>, record: Arc<MemoryRecord>) -> Harness {
    let library = FakeLibrary::new();
    let mapper = PathMapper::new(&[
      PathRule {
        from: "/media/movies".to_owned(),
        to: r"\\nas\movies".to_owned(),
      },
      PathRule {
        from: "/media/tv".to_owned(),
        to: r"\\nas\tv".to_owned(),
      },
    ])
    .unwrap();
    let (reporter, reports) = Reporter::channel();
    let (tx, events) = mpsc::channel(16);
    let ctrl = Controller::new(
      player.clone(),
      library.clone(),
      Arc::new(mapper),
      reporter,
      ControllerOptions::default(),
      tx,
    )
    .with_record(record.clone());
    // Tests start with an MC that has answered (the start-up grace has its
    // own tests).
    let mut ctrl = ctrl;
    ctrl.watchdog.mc_appeared = None;
    Harness {
      ctrl,
      player,
      library,
      record,
      events,
      reports,
    }
  }

  fn play(item: &str, start_ticks: Option<i64>) -> Event {
    play_request(vec![item.to_owned()], start_ticks, None, "PlayNow")
  }

  fn play_request(
    items: Vec<String>,
    start_ticks: Option<i64>,
    start_index: Option<i32>,
    command: &str,
  ) -> Event {
    Event::Command(JellyfinCommand::Play(PlayRequest {
      item_ids: items,
      start_position_ticks: start_ticks,
      play_command: command.to_owned(),
      media_source_id: None,
      audio_stream_index: None,
      subtitle_stream_index: None,
      start_index,
    }))
  }

  fn playstate(command: &str, seek: Option<i64>) -> Event {
    Event::Command(JellyfinCommand::Playstate(PlaystateRequest {
      command: command.to_owned(),
      seek_position_ticks: seek,
    }))
  }

  impl Harness {
    async fn send(&mut self, event: Event) -> PollHint {
      self.ctrl.handle(event).await
    }

    /// Feed the start task's result back in (and any other task results
    /// that arrive first).
    async fn finish_start(&mut self) {
      loop {
        let event = tokio::time::timeout(Duration::from_secs(60), self.events.recv())
          .await
          .expect("start finished in time")
          .expect("channel open");
        let done = matches!(event, Event::StartFinished { .. });
        self.ctrl.handle(event).await;
        if done {
          return;
        }
      }
    }

    /// Handle the next event a task sent (e.g. the restart check).
    async fn next_event(&mut self) {
      let event = tokio::time::timeout(Duration::from_secs(60), self.events.recv())
        .await
        .expect("an event in time")
        .expect("channel open");
      self.ctrl.handle(event).await;
    }

    async fn start(&mut self, item: &str, start_ticks: Option<i64>) {
      self.send(play(item, start_ticks)).await;
      self.finish_start().await;
    }

    fn reports(&mut self) -> Vec<Report> {
      let mut out = Vec::new();
      while let Ok(report) = self.reports.try_recv() {
        out.push(report);
      }
      out
    }

    /// The file key MC uses for the current playback.
    fn key(&self) -> i64 {
      *self.player.next_key.lock().unwrap()
    }
  }

  fn starts(reports: &[Report]) -> Vec<&ProgressReport> {
    reports
      .iter()
      .filter_map(|r| match r {
        Report::Start(p) => Some(p),
        _ => None,
      })
      .collect()
  }

  fn progresses(reports: &[Report]) -> Vec<&ProgressReport> {
    reports
      .iter()
      .filter_map(|r| match r {
        Report::Progress(p) => Some(p),
        _ => None,
      })
      .collect()
  }

  fn stops(reports: &[Report]) -> Vec<&StopReport> {
    reports
      .iter()
      .filter_map(|r| match r {
        Report::Stopped(s) => Some(s),
        _ => None,
      })
      .collect()
  }

  #[tokio::test(start_paused = true)]
  async fn play_maps_checks_starts_and_reports_start() {
    let mut h = harness();
    h.start(MOVIE, Some(12_000_000_000)).await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert_eq!(
      h.player.calls(),
      vec![
        Call::Check(r"\\nas\movies\Avatar\avatar.mkv".to_owned()),
        Call::Play(r"\\nas\movies\Avatar\avatar.mkv".to_owned(), 1_200_000),
      ]
    );
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    let start = starts(&reports)[0];
    assert_eq!(start.item_id, MOVIE);
    assert_eq!(start.media_source_id.as_deref(), Some(MOVIE));
    assert_eq!(start.position_ticks, Ticks(12_003_000_000));
    assert_eq!(start.play_method, PlayMethod::DirectPlay);
    assert!(!start.is_paused);
  }

  #[tokio::test(start_paused = true)]
  async fn blu_ray_folders_play_their_index() {
    let mut h = harness();
    h.start(DISC, None).await;
    assert_eq!(
      h.player.calls()[1],
      Call::Play(
        r"\\nas\movies\Star Trek\Into Darkness\BDMV\index.bdmv".to_owned(),
        0
      )
    );
  }

  #[tokio::test(start_paused = true)]
  async fn progress_every_ten_seconds_and_stop_at_last_polled_position_when_the_file_ends() {
    let mut h = harness();
    h.start(EPISODE, None).await;
    h.reports();
    let key = h.key();
    for second in 1..=12 {
      h.player
        .set_status(vec![Ok(info(PlayState::Playing, key, second * 1000))]);
      tokio::time::advance(Duration::from_secs(1)).await;
      h.ctrl.poll().await;
    }
    let reports = h.reports();
    let progress = progresses(&reports);
    assert_eq!(progress.len(), 1, "{reports:?}");
    assert_eq!(progress[0].position_ticks, Ticks(10_000 * 10_000));

    // The file ends: MC's last position while playing, then State 0 at 0.
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 7_199_000))]);
    h.ctrl.poll().await;
    h.player
      .set_status(vec![Ok(info(PlayState::Stopped, key, 0))]);
    h.ctrl.poll().await;
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].position_ticks, Ticks(7_199_000 * 10_000));
    assert!(!stop[0].failed);
    assert_eq!(h.ctrl.state_name(), "idle");
    // Idle: no more polling effects.
    h.ctrl.poll().await;
    assert!(h.reports().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn pause_and_resume_report_on_the_next_poll() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    let key = h.key();
    assert_eq!(h.send(playstate("Pause", None)).await, PollHint::Now);
    h.player
      .set_status(vec![Ok(info(PlayState::Paused, key, 5_000))]);
    h.ctrl.poll().await;
    assert_eq!(h.send(playstate("Unpause", None)).await, PollHint::Now);
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 5_100))]);
    h.ctrl.poll().await;
    h.send(playstate("PlayPause", None)).await;
    let reports = h.reports();
    let progress = progresses(&reports);
    assert_eq!(progress.len(), 2);
    assert!(progress[0].is_paused);
    assert!(!progress[1].is_paused);
    assert_eq!(
      h.player.calls()[2..],
      [Call::Pause, Call::Resume, Call::Toggle]
    );
  }

  #[tokio::test(start_paused = true)]
  async fn seek_converts_ticks_and_reports_the_new_position() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    h.send(playstate("Seek", Some(18_000_000_000))).await;
    assert_eq!(h.player.calls().last(), Some(&Call::Seek(1_800_000)));
    let reports = h.reports();
    assert_eq!(
      progresses(&reports)[0].position_ticks,
      Ticks(18_000_000_000)
    );
  }

  #[tokio::test(start_paused = true)]
  async fn stop_from_jellyfin_stops_jriver_and_reports_once() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 600_000))]);
    h.send(playstate("Stop", None)).await;
    assert_eq!(h.player.calls().last(), Some(&Call::Stop));
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    let stop = stops(&reports)[0];
    assert_eq!(stop.position_ticks, Ticks(600_000 * 10_000));
    assert!(!stop.failed);
    assert_eq!(h.ctrl.state_name(), "idle");
    // A second Stop does nothing.
    h.send(playstate("Stop", None)).await;
    assert!(h.reports().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn stopping_in_jriver_is_reported_with_the_last_position() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 1_000_000))]);
    h.ctrl.poll().await;
    h.player
      .set_status(vec![Ok(info(PlayState::Stopped, key, 1_000_400))]);
    h.ctrl.poll().await;
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].position_ticks, Ticks(1_000_000 * 10_000));
    assert!(h.player.calls().iter().all(|c| *c != Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn another_file_in_jriver_ends_the_playback() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, 9, 3_000))]);
    h.ctrl.poll().await;
    assert_eq!(stops(&h.reports()).len(), 1);
    assert_eq!(h.ctrl.state_name(), "idle");
  }

  #[tokio::test(start_paused = true)]
  async fn a_momentary_missing_file_key_is_not_a_stop() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, -1, 0))]);
    h.ctrl.poll().await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert!(h.reports().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn mcws_silence_ends_the_playback_after_five_polls() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 2_000_000))]);
    h.ctrl.poll().await;
    h.player.set_status(vec![Err(())]);
    for _ in 0..4 {
      h.ctrl.poll().await;
    }
    assert_eq!(h.ctrl.state_name(), "active");
    h.ctrl.poll().await;
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].position_ticks, Ticks(2_000_000 * 10_000));
    assert_eq!(h.ctrl.state_name(), "idle");
  }

  #[tokio::test(start_paused = true)]
  async fn new_play_reports_the_old_one_stopped_first() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 300_000))]);
    h.start(EPISODE, None).await;
    let reports = h.reports();
    assert_eq!(reports.len(), 3, "{reports:?}");
    let first = match &reports[0] {
      Report::Start(r) => r.play_session_id.clone(),
      other => panic!("{other:?}"),
    };
    match &reports[1] {
      Report::Stopped(s) => {
        assert_eq!(s.item_id, MOVIE);
        assert_eq!(s.play_session_id, first);
        assert_eq!(s.position_ticks, Ticks(300_000 * 10_000));
        assert!(!s.failed);
      }
      other => panic!("{other:?}"),
    }
    match &reports[2] {
      Report::Start(r) => {
        assert_eq!(r.item_id, EPISODE);
        assert_ne!(r.play_session_id, first);
      }
      other => panic!("{other:?}"),
    }
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_start_reports_a_failed_stop_at_the_resume_point() {
    let mut h = harness();
    *h.player.fail_start.lock().unwrap() = true;
    h.start(MOVIE, Some(6_000_000_000)).await;
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    let stop = stops(&reports)[0];
    assert!(stop.failed);
    assert_eq!(stop.position_ticks, Ticks(6_000_000_000));
    assert_eq!(h.ctrl.state_name(), "idle");
  }

  #[tokio::test(start_paused = true)]
  async fn missing_files_unknown_items_and_isos_never_reach_jriver() {
    for (item, missing) in [(MOVIE, true), ("ffff", false), (ISO, false)] {
      let mut h = harness();
      *h.player.missing.lock().unwrap() = missing;
      h.start(item, None).await;
      assert!(
        h.player
          .calls()
          .iter()
          .all(|c| !matches!(c, Call::Play(..))),
        "{item}"
      );
      let reports = h.reports();
      assert_eq!(reports.len(), 1, "{item}");
      assert!(stops(&reports)[0].failed, "{item}");
    }
  }

  #[tokio::test(start_paused = true)]
  async fn stop_while_starting_cancels_the_start_and_reports_once() {
    let mut h = harness();
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(30);
    h.send(play(MOVIE, Some(10_000_000))).await;
    // Let the start task reach JRiver.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_eq!(h.ctrl.state_name(), "starting");
    h.send(playstate("Stop", None)).await;
    assert_eq!(h.ctrl.state_name(), "idle");
    assert_eq!(h.player.calls().last(), Some(&Call::Stop));
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    assert!(stops(&reports)[0].failed);
    // The cancelled task never reports back.
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(h.events.try_recv(), Err(TryRecvError::Empty)));
  }

  #[tokio::test(start_paused = true)]
  async fn a_stale_start_result_is_ignored() {
    let mut h = harness();
    h.send(play(MOVIE, None)).await;
    let stale = h.events.recv().await.unwrap();
    // A second Play replaces the first before its result is handled.
    h.send(play(EPISODE, None)).await;
    h.ctrl.handle(stale).await;
    assert_eq!(h.ctrl.state_name(), "starting");
    h.finish_start().await;
    let reports = h.reports();
    // The first start: one failed stop. The second: one start.
    assert_eq!(stops(&reports).len(), 1);
    assert_eq!(starts(&reports).len(), 1);
    assert_eq!(starts(&reports)[0].item_id, EPISODE);
  }

  #[tokio::test(start_paused = true)]
  async fn queues_start_at_start_index_and_other_play_commands_are_ignored() {
    let mut h = harness();
    h.send(play_request(
      vec![MOVIE.to_owned(), EPISODE.to_owned()],
      None,
      Some(1),
      "PlayNow",
    ))
    .await;
    h.finish_start().await;
    assert_eq!(starts(&h.reports())[0].item_id, EPISODE);

    let mut h = harness();
    h.send(play_request(vec![MOVIE.to_owned()], None, None, "PlayNext"))
      .await;
    h.send(play_request(
      vec![MOVIE.to_owned()],
      None,
      Some(5),
      "PlayNow",
    ))
    .await;
    assert_eq!(h.ctrl.state_name(), "idle");
    assert!(h.player.calls().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn commands_without_a_playback_do_nothing() {
    let mut h = harness();
    for command in ["Pause", "Unpause", "PlayPause", "NextTrack"] {
      h.send(playstate(command, None)).await;
    }
    h.send(playstate("Seek", Some(1))).await;
    h.send(Event::Command(JellyfinCommand::GeneralCommand(
      GeneralCommand {
        name: "SetVolume".to_owned(),
        arguments: None,
      },
    )))
    .await;
    assert!(h.player.calls().is_empty());
    assert!(h.reports().is_empty());
  }

  fn play_with_tracks(item: &str, audio: Option<i32>, subtitles: Option<i32>) -> Event {
    Event::Command(JellyfinCommand::Play(PlayRequest {
      item_ids: vec![item.to_owned()],
      start_position_ticks: None,
      play_command: "PlayNow".to_owned(),
      media_source_id: None,
      audio_stream_index: audio,
      subtitle_stream_index: subtitles,
      start_index: None,
    }))
  }

  fn track_command(name: &str, index: serde_json::Value) -> Event {
    Event::Command(JellyfinCommand::GeneralCommand(GeneralCommand {
      name: name.to_owned(),
      arguments: Some(serde_json::json!({ "Index": index })),
    }))
  }

  fn track_calls(h: &Harness) -> Vec<Call> {
    h.player
      .calls()
      .into_iter()
      .filter(|call| matches!(call, Call::Track(..)))
      .collect()
  }

  #[tokio::test(start_paused = true)]
  async fn a_cast_picks_the_tracks_asked_for_once_the_file_plays() {
    let mut h = harness();
    h.send(play_with_tracks(TRACKS, Some(3), Some(5))).await;
    h.finish_start().await;
    // Audio counts from 0, subtitles from 1: the third audio track is 2,
    // the second subtitle track 2. Both after the Play.
    let calls = h.player.calls();
    let play_at = calls
      .iter()
      .position(|call| matches!(call, Call::Play(..)))
      .unwrap();
    assert_eq!(
      calls[play_at + 1..].to_vec(),
      vec![
        Call::Track(TrackKind::Audio, 2),
        Call::Track(TrackKind::Subtitle, 2)
      ]
    );
    let reports = h.reports();
    let start = starts(&reports)[0];
    assert_eq!(start.audio_stream_index, Some(3));
    assert_eq!(start.subtitle_stream_index, Some(5));
  }

  #[tokio::test(start_paused = true)]
  async fn a_cast_sends_even_the_files_defaults_and_nothing_when_none_is_asked() {
    let mut h = harness();
    // The default audio track and no subtitles: sent all the same, as MC
    // may remember other tracks for the file.
    h.send(play_with_tracks(TRACKS, Some(1), Some(-1))).await;
    h.finish_start().await;
    assert_eq!(
      track_calls(&h),
      vec![
        Call::Track(TrackKind::Audio, 0),
        Call::Track(TrackKind::Subtitle, 0)
      ]
    );
    h.player.calls.lock().unwrap().clear();
    let reports = h.reports();
    assert_eq!(starts(&reports)[0].audio_stream_index, Some(1));
    assert_eq!(starts(&reports)[0].subtitle_stream_index, Some(-1));

    // Nothing asked: nothing sent, and the defaults reported.
    h.send(play_with_tracks(TRACKS, None, None)).await;
    h.finish_start().await;
    assert!(track_calls(&h).is_empty());
    let reports = h.reports();
    assert_eq!(starts(&reports)[0].audio_stream_index, Some(1));
    assert_eq!(starts(&reports)[0].subtitle_stream_index, Some(-1));

    // An external subtitle file has no JRiver number: the start goes on.
    h.send(play_with_tracks(TRACKS, None, Some(6))).await;
    h.finish_start().await;
    assert!(track_calls(&h).is_empty());
    assert_eq!(h.ctrl.state_name(), "active");
    let reports = h.reports();
    assert_eq!(starts(&reports)[0].subtitle_stream_index, Some(-1));
  }

  #[tokio::test(start_paused = true)]
  async fn with_jriver_picking_the_tracks_no_track_command_is_sent() {
    let mut h = harness();
    h.ctrl.options.tracks_from_jellyfin = false;
    h.send(play_with_tracks(TRACKS, Some(3), Some(5))).await;
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    h.send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    assert!(track_calls(&h).is_empty());
    // Which tracks MC plays isn't known: none are reported.
    let reports = h.reports();
    assert_eq!(starts(&reports)[0].audio_stream_index, None);
    assert_eq!(starts(&reports)[0].subtitle_stream_index, None);
  }

  #[tokio::test(start_paused = true)]
  async fn the_remote_switches_tracks_while_playing() {
    let mut h = harness();
    // Nothing plays yet: ignored.
    h.send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    assert!(track_calls(&h).is_empty());

    h.start(TRACKS, None).await;
    h.reports();
    h.send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    h.send(track_command("SetSubtitleStreamIndex", "4".into()))
      .await;
    h.send(track_command("SetSubtitleStreamIndex", "-1".into()))
      .await;
    // An external file, a stream of the other kind, no index: ignored.
    h.send(track_command("SetSubtitleStreamIndex", "6".into()))
      .await;
    h.send(track_command("SetAudioStreamIndex", "4".into()))
      .await;
    h.send(Event::Command(JellyfinCommand::GeneralCommand(
      GeneralCommand {
        name: "SetAudioStreamIndex".to_owned(),
        arguments: None,
      },
    )))
    .await;
    assert_eq!(
      track_calls(&h),
      vec![
        Call::Track(TrackKind::Audio, 1),
        Call::Track(TrackKind::Subtitle, 1),
        Call::Track(TrackKind::Subtitle, 0),
      ]
    );
    // Each switch is reported at once, so the remote shows it picked.
    let reports = h.reports();
    let progress = progresses(&reports);
    assert_eq!(progress.len(), 3);
    assert_eq!(progress[0].audio_stream_index, Some(2));
    assert_eq!(progress[1].subtitle_stream_index, Some(4));
    assert_eq!(progress[2].subtitle_stream_index, Some(-1));
    assert_eq!(progress[2].audio_stream_index, Some(2));
  }

  #[test]
  fn track_command_index_is_read_from_a_string_or_a_number() {
    let index = |value: serde_json::Value| command_index(Some(&value));
    assert_eq!(index(serde_json::json!({"Index": "3"})), Some(3));
    assert_eq!(index(serde_json::json!({"Index": "-1"})), Some(-1));
    assert_eq!(index(serde_json::json!({"Index": 7})), Some(7));
    assert_eq!(index(serde_json::json!({"Index": "x"})), None);
    assert_eq!(index(serde_json::json!({})), None);
    assert_eq!(command_index(None), None);
  }

  #[tokio::test(start_paused = true)]
  async fn reconnect_registers_again_and_resends_progress() {
    let mut h = harness();
    h.send(Event::Connected).await;
    tokio::task::yield_now().await;
    assert!(h.reports().is_empty());
    h.start(MOVIE, None).await;
    h.reports();
    h.send(Event::Connected).await;
    tokio::task::yield_now().await;
    assert_eq!(*h.library.registrations.lock().unwrap(), 2);
    assert_eq!(progresses(&h.reports()).len(), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn quitting_stops_jriver_reports_once_and_minimizes() {
    let h = harness();
    let Harness {
      mut ctrl,
      player,
      events,
      mut reports,
      ..
    } = h;
    let (tx, mut rx) = mpsc::channel(16);
    ctrl.events = tx.clone();
    drop(events);
    ctrl.handle(play(MOVIE, None)).await;
    let finished = rx.recv().await.unwrap();
    ctrl.handle(finished).await;
    tx.send(Event::Shutdown).await.unwrap();
    ctrl.run(&mut rx).await;
    let mut all = Vec::new();
    while let Ok(r) = reports.try_recv() {
      all.push(r);
    }
    assert_eq!(starts(&all).len(), 1);
    assert_eq!(stops(&all).len(), 1);
    assert!(!stops(&all)[0].failed);
    // SPEC: Quit stops JRiver; MC was full screen, so it is minimized too.
    let calls = player.calls();
    assert_eq!(calls[calls.len() - 2..], [Call::Stop, Call::Minimize]);
  }

  #[tokio::test(start_paused = true)]
  async fn quitting_while_idle_leaves_jriver_alone() {
    let h = harness();
    let Harness {
      ctrl,
      player,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Shutdown).await.unwrap();
    ctrl.run(&mut rx).await;
    assert!(player.calls().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn stop_from_jellyfin_minimizes_jriver_after_a_full_screen_film() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 60_000))]);
    h.ctrl.poll().await;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let calls = h.player.calls();
    assert_eq!(calls[calls.len() - 2..], [Call::Stop, Call::Minimize]);
  }

  #[tokio::test(start_paused = true)]
  async fn a_film_ending_in_jriver_minimizes_it() {
    let mut h = harness();
    h.start(EPISODE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 7_199_000))]);
    h.ctrl.poll().await;
    // MC ends the film and leaves Display view by itself.
    *h.player.ui.lock().unwrap() = 0;
    h.player
      .set_status(vec![Ok(info(PlayState::Stopped, key, 0))]);
    h.ctrl.poll().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.player.calls().last(), Some(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn after_escape_jriver_is_not_minimized() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    // Escape: MC leaves Display view, the film plays on.
    *h.player.ui.lock().unwrap() = 0;
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 60_000))]);
    h.ctrl.poll().await;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn no_minimize_when_jriver_switches_files_or_a_new_play_replaces_the_film() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, 9, 3_000))]);
    h.ctrl.poll().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));

    let mut h = harness();
    h.start(MOVIE, None).await;
    h.start(EPISODE, None).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_play_cancels_a_pending_minimize() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    // MC is slow to leave Display view, so the minimize waits...
    *h.player.stays_full_screen.lock().unwrap() = true;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // ...and a new cast arrives meanwhile.
    h.start(EPISODE, None).await;
    *h.player.ui.lock().unwrap() = 0;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn jriver_still_full_screen_after_the_wait_is_not_minimized() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    *h.player.stays_full_screen.lock().unwrap() = true;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn jriver_that_cant_be_started_fails_the_cast() {
    let mut h = harness();
    *h.player.not_running.lock().unwrap() = true;
    h.start(MOVIE, Some(6_000_000_000)).await;
    assert!(h
      .player
      .calls()
      .iter()
      .all(|c| !matches!(c, Call::Play(..))));
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    assert!(stops(&reports)[0].failed);
  }

  #[tokio::test(start_paused = true)]
  async fn status_follows_the_playback() {
    let mut h = harness();
    let (tx, mut rx) = watch::channel(PlaybackStatus::Idle);
    h.ctrl = h.ctrl.with_status(tx);
    h.send(play(MOVIE, None)).await;
    assert_eq!(*rx.borrow_and_update(), PlaybackStatus::Starting);
    h.finish_start().await;
    assert_eq!(
      *rx.borrow_and_update(),
      PlaybackStatus::Playing {
        name: format!("item {MOVIE}"),
        paused: false
      }
    );
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Paused, key, 5_000))]);
    h.ctrl.poll().await;
    assert!(matches!(
      *rx.borrow_and_update(),
      PlaybackStatus::Playing { paused: true, .. }
    ));
    h.send(playstate("Stop", None)).await;
    assert_eq!(*rx.borrow_and_update(), PlaybackStatus::Idle);
  }

  #[tokio::test(start_paused = true)]
  async fn run_loop_polls_once_a_second_only_while_playing() {
    let h = harness();
    let Harness {
      mut ctrl,
      player,
      events,
      mut reports,
      ..
    } = h;
    let (tx, mut rx) = mpsc::channel(16);
    ctrl.events = tx.clone();
    drop(events);
    let run = tokio::spawn(async move { ctrl.run(&mut rx).await });
    // Idle for a minute: no MCWS polls.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(player.calls().is_empty());

    tx.send(play(MOVIE, None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let key = *player.next_key.lock().unwrap();
    player.set_status(vec![Ok(info(PlayState::Playing, key, 1_000))]);
    // Then the playback stops in JRiver; the next poll notices.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    player.set_status(vec![Ok(info(PlayState::Stopped, key, 0))]);
    tokio::time::sleep(Duration::from_secs(2)).await;
    tx.send(Event::Shutdown).await.unwrap();
    run.await.unwrap();

    let mut all = Vec::new();
    while let Ok(r) = reports.try_recv() {
      all.push(r);
    }
    assert_eq!(starts(&all).len(), 1, "{all:?}");
    let stop = stops(&all);
    assert_eq!(stop.len(), 1, "{all:?}");
    assert_eq!(stop[0].position_ticks, Ticks(1_000 * 10_000));
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_play_that_fails_early_still_stops_the_old_film() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 400_000))]);
    // An ISO is refused before JRiver is touched.
    h.start(ISO, None).await;
    assert_eq!(h.ctrl.state_name(), "idle");
    assert_eq!(h.player.calls().last(), Some(&Call::Stop));
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 2, "{reports:?}");
    assert_eq!(stop[0].item_id, MOVIE);
    assert!(!stop[0].failed);
    assert_eq!(stop[0].position_ticks, Ticks(400_000 * 10_000));
    assert_eq!(stop[1].item_id, ISO);
    assert!(stop[1].failed);
  }

  #[tokio::test(start_paused = true)]
  async fn a_play_while_idle_does_not_stop_jriver() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    assert!(h.player.calls().iter().all(|c| *c != Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn registration_is_retried_until_it_takes() {
    let mut h = harness();
    *h.library.register_failures.lock().unwrap() = 2;
    h.send(Event::Connected).await;
    // 1 s and 2 s of backoff.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(*h.library.registrations.lock().unwrap(), 3);
    assert_eq!(*h.library.register_failures.lock().unwrap(), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_connection_replaces_a_pending_registration_retry() {
    let mut h = harness();
    *h.library.register_failures.lock().unwrap() = 1;
    h.send(Event::Connected).await;
    tokio::task::yield_now().await;
    // The first attempt failed and waits 1 s; a reconnect arrives first.
    h.send(Event::Connected).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    // One failed attempt, then the reconnect's successful one; the old
    // retry never runs.
    assert_eq!(*h.library.registrations.lock().unwrap(), 2);
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_seek_keeps_the_position_and_reports_nothing() {
    let mut h = harness();
    h.start(MOVIE, Some(10_000_000)).await;
    h.reports();
    *h.player.fail_seek.lock().unwrap() = true;
    h.send(playstate("Seek", Some(18_000_000_000))).await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert!(h.reports().is_empty());
    // The next progress report still carries the old position.
    h.send(Event::Connected).await;
    let reports = h.reports();
    assert_eq!(
      progresses(&reports)[0].position_ticks,
      Ticks(1_300 * 10_000)
    );
  }

  #[tokio::test(start_paused = true)]
  async fn quitting_while_starting_cancels_the_start_then_stops_jriver() {
    let mut h = harness();
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(30);
    h.send(play(MOVIE, Some(10_000_000))).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_eq!(h.ctrl.state_name(), "starting");
    h.ctrl.shut_down().await;
    // The start task is gone before the stop, so nothing plays afterwards.
    tokio::time::advance(Duration::from_secs(60)).await;
    let calls = h.player.calls();
    let stop = calls
      .iter()
      .position(|c| *c == Call::Stop)
      .expect("JRiver stopped");
    assert!(calls[stop..].iter().all(|c| !matches!(c, Call::Play(..))));
    assert!(!calls.contains(&Call::Minimize));
    let reports = h.reports();
    assert_eq!(reports.len(), 1);
    assert!(stops(&reports)[0].failed);
    assert!(matches!(h.events.try_recv(), Err(TryRecvError::Empty)));
  }

  #[tokio::test(start_paused = true)]
  async fn quitting_lets_a_pending_minimize_finish() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    *h.player.stays_full_screen.lock().unwrap() = true;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // MC leaves Display view while the bridge is quitting.
    *h.player.ui.lock().unwrap() = 0;
    h.ctrl.shut_down().await;
    assert_eq!(h.player.calls().last(), Some(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn minimize_can_be_turned_off() {
    let mut h = harness();
    h.ctrl.options.minimize_after_playback = false;
    h.start(MOVIE, None).await;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn mcws_going_silent_does_not_minimize() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.player.set_status(vec![Err(())]);
    for _ in 0..5 {
      h.ctrl.poll().await;
    }
    assert_eq!(h.ctrl.state_name(), "idle");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  #[tokio::test(start_paused = true)]
  async fn escape_just_before_a_stop_still_counts() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 60_000))]);
    h.ctrl.poll().await;
    // Escape, then Stop from the phone before the next poll.
    *h.player.ui.lock().unwrap() = 0;
    h.send(playstate("Stop", None)).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!h.player.calls().contains(&Call::Minimize));
  }

  // --- After a bridge restart (SPEC.md "The bridge killed") ---

  const AVATAR_UNC: &str = r"\\nas\movies\Avatar\avatar.mkv";

  fn left_over(item: &str, position_ms: u64) -> Option<NowPlaying> {
    Some(NowPlaying {
      item_id: item.to_owned(),
      media_source_id: Some(item.to_owned()),
      position_ticks: Millis(position_ms).to_ticks().0,
    })
  }

  fn playing_file(key: i64, pos: u64, filename: &str) -> PlaybackInfo {
    PlaybackInfo {
      filename: Some(filename.to_owned()),
      ..info(PlayState::Playing, key, pos)
    }
  }

  #[tokio::test(start_paused = true)]
  async fn a_film_jriver_still_plays_after_a_bridge_restart_is_taken_over() {
    let mut h = harness();
    *h.library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    // MC reports the path in its own case: still the same file.
    h.player.set_status(vec![Ok(playing_file(
      7,
      600_000,
      &AVATAR_UNC.to_uppercase(),
    ))]);
    *h.player.ui.lock().unwrap() = UI_MODE_DISPLAY;
    h.send(Event::Connected).await;
    h.next_event().await;
    assert_eq!(h.ctrl.state_name(), "active");
    let reports = h.reports();
    let start = starts(&reports);
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].item_id, MOVIE);
    assert_eq!(start[0].position_ticks, Ticks(600_000 * 10_000));
    assert!(stops(&reports).is_empty());
    // MC was left alone, and the remote works again.
    assert!(h.player.calls().is_empty());
    h.send(playstate("Stop", None)).await;
    let after = h.reports();
    assert_eq!(stops(&after).len(), 1);
    assert_eq!(stops(&after)[0].play_session_id, start[0].play_session_id);
    assert!(h.player.calls().contains(&Call::Stop));
  }

  #[tokio::test(start_paused = true)]
  async fn a_paused_left_over_film_is_taken_over_paused() {
    let mut h = harness();
    *h.library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    h.player.set_status(vec![Ok(PlaybackInfo {
      filename: Some(AVATAR_UNC.to_owned()),
      ..info(PlayState::Paused, 7, 600_000)
    })]);
    h.send(Event::Connected).await;
    h.next_event().await;
    let reports = h.reports();
    assert!(starts(&reports)[0].is_paused);
  }

  #[tokio::test(start_paused = true)]
  async fn a_stale_film_is_reported_stopped_when_jriver_plays_something_else_or_nothing() {
    for status in [
      Ok(playing_file(7, 600_000, r"\\nas\movies\Other\other.mkv")),
      Ok(info(PlayState::Stopped, -1, 0)),
      Err(()),
    ] {
      let mut h = harness();
      *h.library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
      h.player.set_status(vec![status]);
      h.send(Event::Connected).await;
      h.next_event().await;
      assert_eq!(h.ctrl.state_name(), "idle");
      let reports = h.reports();
      let stop = stops(&reports);
      assert_eq!(stop.len(), 1);
      assert_eq!(stop[0].item_id, MOVIE);
      assert_eq!(stop[0].position_ticks, Ticks(590_000 * 10_000));
      assert!(!stop[0].failed, "the server's resume point is kept");
      // Whatever MC plays is left alone.
      assert!(h.player.calls().is_empty());
    }
  }

  fn recorded(position_ms: u64, paused: bool) -> PlaybackRecord {
    PlaybackRecord {
      item_id: MOVIE.to_owned(),
      media_source_id: Some(MOVIE.to_owned()),
      play_session_id: "recordedsession".to_owned(),
      name: "Avatar".to_owned(),
      path: AVATAR_UNC.to_owned(),
      position_ms,
      paused,
      written_at: now_secs(),
      audio_stream_index: None,
      subtitle_stream_index: None,
    }
  }

  /// The record as it stands, without its write time.
  fn stored(record: &MemoryRecord) -> Option<PlaybackRecord> {
    record.get().map(|r| PlaybackRecord { written_at: 0, ..r })
  }

  fn recorded_as(record: &PlaybackRecord, position_ms: u64, paused: bool) -> PlaybackRecord {
    PlaybackRecord {
      position_ms,
      paused,
      written_at: 0,
      ..record.clone()
    }
  }

  fn aged(position_ms: u64, age: Duration) -> PlaybackRecord {
    PlaybackRecord {
      written_at: now_secs() - age.as_secs(),
      ..recorded(position_ms, false)
    }
  }

  #[tokio::test(start_paused = true)]
  async fn the_playing_film_is_recorded_while_it_plays_and_forgotten_when_it_ends() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let session = starts(&h.reports())[0].play_session_id.clone();
    let record = h.record.get().expect("recorded at the start");
    assert!(record.age() < Duration::from_secs(5));
    assert_eq!(record.item_id, MOVIE);
    assert_eq!(record.path, AVATAR_UNC);
    assert_eq!(record.play_session_id, session);
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 5_000))]);
    h.ctrl.poll().await;
    assert_eq!(stored(&h.record), Some(recorded_as(&record, 5_000, false)));
    h.player
      .set_status(vec![Ok(info(PlayState::Paused, key, 6_000))]);
    h.ctrl.poll().await;
    assert_eq!(stored(&h.record), Some(recorded_as(&record, 6_000, true)));
    // Paused at the same place: not written again.
    h.record.clear();
    h.ctrl.poll().await;
    assert_eq!(h.record.get(), None);
    h.send(playstate("Stop", None)).await;
    assert_eq!(h.record.get(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_recorded_film_jriver_still_plays_is_taken_over_after_a_kill() {
    for paused in [false, true] {
      let mut h = harness();
      // The server forgot the film when the old bridge's socket closed.
      h.record.save(&aged(590_000, Duration::from_secs(3600)));
      h.player.set_status(vec![Ok(PlaybackInfo {
        filename: Some(AVATAR_UNC.to_uppercase()),
        ..info(
          if paused {
            PlayState::Paused
          } else {
            PlayState::Playing
          },
          7,
          600_000,
        )
      })]);
      h.send(Event::Connected).await;
      h.next_event().await;
      assert_eq!(h.ctrl.state_name(), "active");
      let reports = h.reports();
      let start = starts(&reports);
      assert_eq!(start.len(), 1);
      assert_eq!(start[0].item_id, MOVIE);
      assert_eq!(start[0].play_session_id, "recordedsession");
      assert_eq!(start[0].position_ticks, Ticks(600_000 * 10_000));
      assert_eq!(start[0].is_paused, paused);
      assert!(stops(&reports).is_empty());
      assert!(h.player.calls().is_empty(), "MC was left alone");
      assert_eq!(
        stored(&h.record),
        Some(recorded_as(&recorded(0, false), 600_000, paused)),
        "recorded again for the next kill"
      );
      // The remote works again.
      h.send(playstate("Pause", None)).await;
      assert!(h.player.calls().contains(&Call::Pause));
    }
  }

  #[tokio::test(start_paused = true)]
  async fn a_recorded_film_jriver_no_longer_plays_is_reported_stopped_where_it_was() {
    for status in [
      Ok(info(PlayState::Stopped, -1, 0)),
      Ok(playing_file(7, 600_000, r"\\nas\movies\Other\other.mkv")),
      Err(()),
    ] {
      let mut h = harness();
      h.record.save(&recorded(590_000, false));
      h.player.set_status(vec![status]);
      h.send(Event::Connected).await;
      h.next_event().await;
      assert_eq!(h.ctrl.state_name(), "idle");
      let reports = h.reports();
      let stop = stops(&reports);
      assert_eq!(stop.len(), 1);
      assert_eq!(stop[0].item_id, MOVIE);
      assert_eq!(stop[0].play_session_id, "recordedsession");
      assert_eq!(stop[0].position_ticks, Ticks(590_000 * 10_000));
      assert!(!stop[0].failed, "the resume point is kept");
      assert!(
        h.player.calls().is_empty(),
        "whatever MC plays is left alone"
      );
      assert_eq!(h.record.get(), None, "handled once");
    }
  }

  #[tokio::test(start_paused = true)]
  async fn old_records_are_not_reported_or_taken_over() {
    // Stopped long ago: the user may have watched on elsewhere since.
    let mut h = harness();
    h.record.save(&aged(590_000, Duration::from_secs(11 * 60)));
    h.player
      .set_status(vec![Ok(info(PlayState::Stopped, -1, 0))]);
    h.send(Event::Connected).await;
    h.next_event().await;
    assert!(h.reports().is_empty());
    assert_eq!(h.record.get(), None);
    // Days old: what MC plays now was started some other way.
    let mut h = harness();
    h.record
      .save(&aged(590_000, Duration::from_secs(13 * 3600)));
    h.player
      .set_status(vec![Ok(playing_file(7, 600_000, AVATAR_UNC))]);
    h.send(Event::Connected).await;
    h.next_event().await;
    assert_eq!(h.ctrl.state_name(), "idle");
    assert!(h.reports().is_empty());
    assert!(h.player.calls().is_empty());
    assert_eq!(h.record.get(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_play_during_the_check_still_reports_the_killed_film_stopped() {
    for mc_plays_it in [true, false] {
      let mut h = harness();
      h.record.save(&recorded(590_000, false));
      h.player.set_status(vec![Ok(if mc_plays_it {
        playing_file(7, 600_000, AVATAR_UNC)
      } else {
        info(PlayState::Stopped, -1, 0)
      })]);
      h.send(Event::Connected).await;
      h.send(play(EPISODE, None)).await;
      h.finish_start().await;
      tokio::time::sleep(Duration::from_millis(10)).await;
      while let Ok(event) = h.events.try_recv() {
        h.ctrl.handle(event).await;
      }
      assert_eq!(h.ctrl.state_name(), "active");
      let reports = h.reports();
      let stop = stops(&reports);
      assert_eq!(stop.len(), 1, "the old session is closed");
      assert_eq!(stop[0].item_id, MOVIE);
      assert_eq!(stop[0].play_session_id, "recordedsession");
      let expected = if mc_plays_it { 600_000 } else { 590_000 };
      assert_eq!(stop[0].position_ticks, Ticks(expected * 10_000));
      assert_eq!(starts(&reports)[0].item_id, EPISODE);
      // The record is the new film's now.
      assert_eq!(h.record.get().unwrap().item_id, EPISODE);
    }
  }

  #[tokio::test(start_paused = true)]
  async fn a_play_during_the_check_that_fails_still_stops_the_left_over_film() {
    let mut h = harness();
    h.record.save(&recorded(590_000, false));
    h.player
      .set_status(vec![Ok(playing_file(7, 600_000, AVATAR_UNC))]);
    h.send(Event::Connected).await;
    // An item the server doesn't know: the start fails before JRiver.
    h.send(play("unknownitem", None)).await;
    h.finish_start().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    while let Ok(event) = h.events.try_recv() {
      h.ctrl.handle(event).await;
    }
    assert_eq!(h.ctrl.state_name(), "idle");
    assert!(
      h.player.calls().contains(&Call::Stop),
      "nothing may play on untracked"
    );
    let reports = h.reports();
    assert!(stops(&reports)
      .iter()
      .any(|stop| stop.play_session_id == "recordedsession"));
    assert_eq!(h.record.get(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_paused_film_s_record_is_rewritten_every_minute() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Paused, key, 6_000))]);
    h.ctrl.poll().await;
    h.record.clear();
    tokio::time::advance(Duration::from_secs(30)).await;
    h.ctrl.poll().await;
    assert_eq!(h.record.get(), None, "nothing changed");
    tokio::time::advance(Duration::from_secs(31)).await;
    h.ctrl.poll().await;
    assert_eq!(
      h.record.get().map(|r| (r.position_ms, r.paused)),
      Some((6_000, true)),
      "the heartbeat keeps the record's time current"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_quit_during_the_check_leaves_the_record_for_the_next_run() {
    let h = harness();
    h.record.save(&recorded(590_000, false));
    // MCWS is slow to answer, so the check is still running at Quit.
    h.player.hang();
    let Harness {
      ctrl,
      record,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Connected).await.unwrap();
    tx.send(Event::Shutdown).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Shutdown);
    assert_eq!(
      stored(&record),
      Some(recorded_as(&recorded(0, false), 590_000, false))
    );
  }

  /// A record written under another Jellyfin user of the same account.
  struct OtherUsersRecord(Mutex<Option<PlaybackRecord>>);

  impl RecordStore for OtherUsersRecord {
    fn load(&self) -> Option<Found> {
      self.0.lock().unwrap().clone().map(Found::OtherUser)
    }
    fn save(&self, _: &PlaybackRecord) {}
    fn clear(&self) {
      *self.0.lock().unwrap() = None;
    }
    fn flush(&self) -> crate::record::BoxFuture<'_> {
      Box::pin(async {})
    }
  }

  #[tokio::test(start_paused = true)]
  async fn another_user_s_film_is_stopped_and_never_reported() {
    for still_playing in [true, false] {
      let mut h = harness();
      let record = Arc::new(OtherUsersRecord(Mutex::new(Some(recorded(590_000, false)))));
      h.ctrl = h.ctrl.with_record(record.clone());
      h.player.set_status(vec![Ok(if still_playing {
        playing_file(7, 600_000, AVATAR_UNC)
      } else {
        info(PlayState::Stopped, -1, 0)
      })]);
      h.send(Event::Connected).await;
      h.next_event().await;
      assert_eq!(h.ctrl.state_name(), "idle");
      assert!(h.reports().is_empty());
      assert_eq!(h.player.calls().contains(&Call::Stop), still_playing);
      assert!(record.0.lock().unwrap().is_none(), "handled once");
    }
  }

  #[tokio::test(start_paused = true)]
  async fn nothing_left_over_means_nothing_to_do_and_the_check_runs_once() {
    let mut h = harness();
    h.player
      .set_status(vec![Ok(playing_file(7, 600_000, AVATAR_UNC))]);
    h.send(Event::Connected).await;
    h.next_event().await;
    *h.library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    h.send(Event::Connected).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(h.events.try_recv().is_err(), "no second check");
    assert_eq!(h.ctrl.state_name(), "idle");
    assert!(h.reports().is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn a_play_during_the_restart_check_wins() {
    let mut h = harness();
    *h.library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    h.player
      .set_status(vec![Ok(playing_file(7, 600_000, AVATAR_UNC))]);
    h.send(Event::Connected).await;
    h.send(play(EPISODE, None)).await;
    // Handles the check's result (ignored) and the start.
    h.finish_start().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    while let Ok(event) = h.events.try_recv() {
      h.ctrl.handle(event).await;
    }
    let reports = h.reports();
    let start = starts(&reports);
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].item_id, EPISODE);
    assert!(stops(&reports).is_empty());
  }

  // --- A hung JRiver (SPEC.md "JRiver hung") ---

  /// One second of the run loop's Windows check. (The fake says "Not
  /// responding" at once; Windows itself takes 5 s.)
  async fn tick(h: &mut Harness) {
    h.ctrl.watch_windows().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
  }

  /// Check Windows once a second until `done`, at most 30 rounds.
  async fn tick_until(h: &mut Harness, done: impl Fn(&Harness) -> bool) {
    for _ in 0..30 {
      if done(h) {
        return;
      }
      tick(h).await;
    }
  }

  /// Poll and check Windows once a second until `done`, at most 30 rounds.
  async fn poll_until(h: &mut Harness, done: impl Fn(&Harness) -> bool) {
    for _ in 0..30 {
      if done(h) {
        return;
      }
      h.ctrl.poll().await;
      tick(h).await;
    }
  }

  fn restarts(player: &FakePlayer) -> usize {
    player
      .calls()
      .iter()
      .filter(|c| **c == Call::Restart)
      .count()
  }

  fn plays(player: &FakePlayer) -> Vec<(String, u64)> {
    player
      .calls()
      .into_iter()
      .filter_map(|c| match c {
        Call::Play(path, ms) => Some((path, ms)),
        _ => None,
      })
      .collect()
  }

  #[tokio::test(start_paused = true)]
  async fn a_hung_jriver_is_restarted_and_the_film_resumed_from_the_last_position() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 2_000_000))]);
    h.ctrl.poll().await;
    h.reports();
    let hung_at = Instant::now();
    h.player.hang();
    poll_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
    // Polls time out after 2 s, and Windows' "Not responding" must hold for
    // 3 s: restarted 3-6 s after the fake hung (plus Windows' own 5 s).
    assert!(hung_at.elapsed() >= Duration::from_secs(3));
    assert!(hung_at.elapsed() < Duration::from_secs(7));
    // The film was reported stopped where it was, then started again there.
    assert_eq!(h.ctrl.state_name(), "starting");
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert_eq!(
      plays(&h.player).last(),
      Some(&(AVATAR_UNC.to_owned(), 2_000_000))
    );
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].position_ticks, Ticks(2_000_000 * 10_000));
    assert!(!stop[0].failed);
    assert_eq!(starts(&reports).len(), 1);
    assert_ne!(starts(&reports)[0].play_session_id, stop[0].play_session_id);
  }

  #[tokio::test(start_paused = true)]
  async fn silence_while_windows_says_jriver_responds_ends_the_film_without_a_restart() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 2_000_000))]);
    h.ctrl.poll().await;
    h.reports();
    *h.player.errors_are_timeouts.lock().unwrap() = true;
    h.player.set_status(vec![Err(())]);
    let silent_at = Instant::now();
    // Timeouts don't count as the five missed polls of a vanished MC.
    for _ in 0..10 {
      h.ctrl.poll().await;
      tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(h.ctrl.state_name(), "active");
    for _ in 0..40 {
      if h.ctrl.state_name() == "idle" {
        break;
      }
      h.ctrl.poll().await;
      tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(h.ctrl.state_name(), "idle");
    assert!(silent_at.elapsed() >= Duration::from_secs(30));
    assert_eq!(restarts(&h.player), 0);
    let reports = h.reports();
    assert_eq!(stops(&reports).len(), 1);
    assert_eq!(stops(&reports)[0].position_ticks, Ticks(2_000_000 * 10_000));
  }

  #[tokio::test(start_paused = true)]
  async fn a_second_hang_soon_after_a_resume_is_not_resumed_again() {
    let mut h = harness();
    *h.player.hangs_again.lock().unwrap() = true;
    h.start(MOVIE, None).await;
    h.player.hang();
    poll_until(&mut h, |h| restarts(&h.player) > 0).await;
    // Still hung after the restart: the resumed start times out too.
    h.finish_start().await;
    assert_eq!(plays(&h.player).len(), 2);
    tick_until(&mut h, |h| restarts(&h.player) > 1).await;
    assert_eq!(restarts(&h.player), 2);
    assert_eq!(h.ctrl.state_name(), "idle");
    assert_eq!(plays(&h.player).len(), 2, "no second resume");
  }

  #[tokio::test(start_paused = true)]
  async fn an_idle_hung_jriver_is_restarted_at_most_three_times_an_hour() {
    let mut h = harness();
    *h.player.hangs_again.lock().unwrap() = true;
    h.player.hang();
    for _ in 0..60 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 3);
    assert!(plays(&h.player).is_empty());
    assert!(h.reports().is_empty());
    // An hour after the first restart, a fourth is allowed (MC has been
    // hung all along, so the next check restarts it).
    tokio::time::sleep(HOUR).await;
    tick(&mut h).await;
    tick(&mut h).await;
    assert_eq!(restarts(&h.player), 4);
  }

  #[tokio::test(start_paused = true)]
  async fn a_jriver_that_isnt_running_is_never_restarted() {
    let mut h = harness();
    *h.player.not_running.lock().unwrap() = true;
    // Even if Windows called some other process of that name hung.
    *h.player.not_responding.lock().unwrap() = true;
    for _ in 0..20 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_start_that_failed_just_before_a_hang_is_played_after_the_restart() {
    let mut h = harness();
    *h.player.fail_start.lock().unwrap() = true;
    h.start(MOVIE, Some(Millis(300_000).to_ticks().0)).await;
    assert_eq!(h.ctrl.state_name(), "idle");
    *h.player.fail_start.lock().unwrap() = false;
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert_eq!(
      plays(&h.player).last(),
      Some(&(AVATAR_UNC.to_owned(), 300_000))
    );
  }

  #[tokio::test(start_paused = true)]
  async fn the_run_loop_asks_windows_every_second() {
    let h = harness();
    let Harness {
      ctrl,
      player,
      events,
      ..
    } = h;
    drop(events);
    player.hang();
    *player.hangs_again.lock().unwrap() = true;
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(async move { ctrl.run(&mut rx).await });
    // The first check (at once) sees "Not responding" and probes MCWS, which
    // times out after 2 s; the check at 3 s finds it held 3 s: restarted.
    // On a real MC add Windows' own 5 s: about 8-9 s after the hang.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(restarts(&player), 0);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(restarts(&player), 1);
    tx.send(Event::Shutdown).await.unwrap();
    assert_eq!(run.await.unwrap(), RunEnd::Shutdown);
  }

  // --- Log out and a new login ---

  #[tokio::test(start_paused = true)]
  async fn log_out_ends_the_run_like_quit() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.reports();
    let Harness {
      ctrl,
      player,
      record,
      events,
      mut reports,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Logout).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Logout);
    assert!(player.calls().contains(&Call::Stop));
    let mut stopped = 0;
    while let Ok(report) = reports.try_recv() {
      stopped += usize::from(matches!(report, Report::Stopped(_)));
    }
    assert_eq!(stopped, 1);
    assert_eq!(record.get(), None, "the record ends with the playback");
  }

  #[tokio::test(start_paused = true)]
  async fn after_a_new_login_the_film_plays_on_and_the_next_run_takes_it_over() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let first_session = starts(&h.reports())[0].play_session_id.clone();
    let key = h.key();
    h.player
      .set_status(vec![Ok(playing_file(key, 30_000, AVATAR_UNC))]);
    let Harness {
      ctrl,
      player,
      record,
      events,
      mut reports,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::LoginChanged).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::LoginChanged);
    // The old token is revoked: nothing is reported, and MC plays on.
    assert!(!player.calls().contains(&Call::Stop));
    assert!(reports.try_recv().is_err());

    // The run with the new login (same user) takes the film over.
    let mut next = harness_sharing(player, record);
    next.send(Event::Connected).await;
    next.next_event().await;
    assert_eq!(next.ctrl.state_name(), "active");
    let reports = next.reports();
    let start = starts(&reports);
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].item_id, MOVIE);
    assert_eq!(start[0].position_ticks, Ticks(30_000 * 10_000));
    assert_eq!(start[0].play_session_id, first_session);
    assert!(stops(&reports).is_empty());
  }

  /// End `ctrl`'s run with a reload after `after`.
  fn reload_after(ctrl: &Controller<FakePlayer, FakeLibrary>, after: Duration) {
    let tx = ctrl.events.clone();
    tokio::spawn(async move {
      tokio::time::sleep(after).await;
      let _ = tx.send(Event::Reload).await;
    });
  }

  #[tokio::test(start_paused = true)]
  async fn a_reload_in_place_carries_the_film_over_and_no_command_is_lost() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.ctrl = h.ctrl.with_carry(slot.clone());
    h.send(play_with_tracks(TRACKS, Some(3), None)).await;
    h.finish_start().await;
    let session = starts(&h.reports())[0].play_session_id.clone();
    let key = h.key();
    h.player.set_status(vec![Ok(playing_file(
      key,
      30_000,
      r"\\nas\movies\Tracks\tracks.mkv",
    ))]);
    let Harness {
      ctrl,
      player,
      record,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);
    assert!(!player.calls().contains(&Call::Stop), "plays on");

    // The next controller in the same Jellyfin session. The socket stayed
    // up, so no Connected comes; the remote's commands sent while the
    // settings were applied wait in the channel and reach the film.
    let Harness {
      ctrl,
      library,
      mut events,
      mut reports,
      ..
    } = harness_sharing(player.clone(), record);
    let ctrl = ctrl.with_carry(slot).with_socket_up(true);
    let tx = ctrl.events.clone();
    tx.send(track_command("SetAudioStreamIndex", "2".into()))
      .await
      .unwrap();
    tx.send(playstate("Stop", None)).await.unwrap();
    reload_after(&ctrl, Duration::from_secs(3));
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    assert_eq!(*library.registrations.lock().unwrap(), 1);
    let calls = player.calls();
    let track = calls
      .iter()
      .rposition(|call| *call == Call::Track(TrackKind::Audio, 1));
    let stop = calls.iter().rposition(|call| *call == Call::Stop);
    assert!(
      matches!((track, stop), (Some(t), Some(s)) if t < s),
      "{calls:?}"
    );
    let mut all = Vec::new();
    while let Ok(report) = reports.try_recv() {
      all.push(report);
    }
    assert!(starts(&all).is_empty(), "the same playback goes on");
    assert!(progresses(&all)
      .iter()
      .all(|p| p.play_session_id == session));
    let stopped = stops(&all);
    assert_eq!(stopped.len(), 1, "stopped exactly once");
    assert_eq!(stopped[0].play_session_id, session);
  }

  #[tokio::test(start_paused = true)]
  async fn within_a_kept_session_the_server_is_not_asked_for_a_left_over_film() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.ctrl = h.ctrl.with_carry(slot.clone());
    h.send(Event::Connected).await;
    h.next_event().await;
    h.start(MOVIE, None).await;
    h.reports();
    // The new settings move MC's zone: the film is stopped and reported,
    // and that report may still be on its way when the next run starts.
    h.ctrl = h.ctrl.with_reload_check(Box::new(|| false));
    let Harness {
      ctrl,
      player,
      record,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);

    let Harness {
      ctrl,
      library,
      mut events,
      mut reports,
      ..
    } = harness_sharing(player, record);
    // The server still shows the film.
    *library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    let ctrl = ctrl.with_carry(slot).with_socket_up(true);
    reload_after(&ctrl, Duration::from_secs(3));
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    assert!(reports.try_recv().is_err(), "no second Stopped");
  }

  #[tokio::test(start_paused = true)]
  async fn a_track_command_that_fails_at_the_start_leaves_jrivers_choice() {
    let mut h = harness();
    *h.player.fail_tracks.lock().unwrap() = true;
    h.send(play_with_tracks(TRACKS, Some(3), Some(5))).await;
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active", "the film plays on");
    let reports = h.reports();
    // The defaults are reported: the file's audio track, no subtitles.
    assert_eq!(starts(&reports)[0].audio_stream_index, Some(1));
    assert_eq!(starts(&reports)[0].subtitle_stream_index, Some(-1));
  }

  #[tokio::test(start_paused = true)]
  async fn a_remote_track_command_that_times_out_counts_toward_a_hang() {
    let mut h = harness();
    h.start(TRACKS, None).await;
    h.reports();
    *h.player.hung.lock().unwrap() = true;
    h.send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    assert!(h.ctrl.watchdog.silent_since.is_some());
    assert!(progresses(&h.reports()).is_empty(), "nothing changed");
  }

  #[tokio::test(start_paused = true)]
  async fn a_disc_folder_gets_no_track_commands() {
    let mut h = harness();
    h.send(play_with_tracks(DISC, Some(3), Some(5))).await;
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    h.send(track_command("SetSubtitleStreamIndex", "-1".into()))
      .await;
    h.send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    assert!(track_calls(&h).is_empty());
  }

  /// End a first run (a bridge restarted with film A recorded, its check
  /// for A still running) with a reload; return what the next run needs.
  async fn reload_during_the_restart_check(
    h: Harness,
    slot: &CarrySlot,
  ) -> (Arc<FakePlayer>, Arc<MemoryRecord>) {
    let Harness {
      ctrl,
      player,
      record,
      events,
      ..
    } = h;
    drop(events);
    let ctrl = ctrl.with_carry(slot.clone());
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);
    (player, record)
  }

  fn all_reports(reports: &mut mpsc::UnboundedReceiver<Report>) -> Vec<Report> {
    let mut all = Vec::new();
    while let Ok(report) = reports.try_recv() {
      all.push(report);
    }
    all
  }

  /// The check's finding for film A (recorded, no longer playing).
  fn stale_a() -> Recovery {
    Recovery::Stale(StopReport {
      item_id: MOVIE.to_owned(),
      media_source_id: None,
      play_session_id: "recordedsession".to_owned(),
      position_ticks: Ticks(300_000_000),
      failed: false,
    })
  }

  #[tokio::test(start_paused = true)]
  async fn a_reload_waits_for_the_restart_check_and_carries_the_film_it_took_over() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.ctrl = h.ctrl.with_carry(slot.clone());
    h.record.save(&recorded(30_000, false));
    h.player
      .set_status(vec![Ok(playing_file(1, 31_000, AVATAR_UNC))]);
    // The first connect starts the check; the settings are saved before
    // its result is handled.
    h.send(Event::Connected).await;
    h.ctrl.events.send(Event::Reload).await.unwrap();
    let Harness {
      ctrl,
      mut events,
      mut reports,
      ..
    } = h;
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    let all = all_reports(&mut reports);
    assert_eq!(starts(&all).len(), 1, "taken over before the hand-over");
    assert!(stops(&all).is_empty());
    let carry = slot.lock().unwrap();
    let carry = carry.as_ref().unwrap();
    assert!(!carry.recovery_pending);
    assert_eq!(
      carry.active.as_ref().unwrap().session.play_session_id,
      "recordedsession"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_login_during_the_restart_check_leaves_the_record_for_the_next_login() {
    let mut h = harness();
    h.record.save(&recorded(30_000, false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    // A new login is saved during the check; its result comes after it.
    let tx = h.ctrl.events.clone();
    tx.send(Event::LoginChanged).await.unwrap();
    tx.send(Event::RecoveryFinished {
      generation: 0,
      recovery: stale_a(),
    })
    .await
    .unwrap();
    let Harness {
      ctrl,
      record,
      mut events,
      mut reports,
      ..
    } = h;
    assert_eq!(ctrl.run(&mut events).await, RunEnd::LoginChanged);
    // The old token is revoked: nothing is reported with it, and the next
    // login's check reports A from the record.
    assert!(reports.try_recv().is_err());
    assert_eq!(record.get().unwrap().play_session_id, "recordedsession");
  }

  #[tokio::test(start_paused = true)]
  async fn a_cast_during_the_reload_wait_finishes_and_is_carried() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.ctrl = h.ctrl.with_carry(slot.clone());
    h.record.save(&recorded(30_000, false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    // Settings saved, then B cast, then the check's finding (A is gone).
    let tx = h.ctrl.events.clone();
    tx.send(Event::Reload).await.unwrap();
    tx.send(play(EPISODE, None)).await.unwrap();
    tx.send(Event::RecoveryFinished {
      generation: 0,
      recovery: stale_a(),
    })
    .await
    .unwrap();
    let Harness {
      ctrl,
      player,
      mut events,
      mut reports,
      ..
    } = h;
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    let all = all_reports(&mut reports);
    let stopped = stops(&all);
    assert_eq!(stopped.len(), 1, "A, once; B isn't cancelled");
    assert_eq!(stopped[0].play_session_id, "recordedsession");
    assert_eq!(starts(&all).len(), 1);
    assert!(!player.calls().ends_with(&[Call::Stop]));
    let carry = slot.lock().unwrap();
    let active = carry.as_ref().unwrap().active.as_ref().expect("B carried");
    assert_eq!(active.session.item_id, EPISODE);
  }

  #[tokio::test(start_paused = true)]
  async fn quit_during_the_reload_wait_ends_at_once_and_keeps_the_record() {
    let mut h = harness();
    h.record.save(&recorded(30_000, false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    let tx = h.ctrl.events.clone();
    tx.send(Event::Reload).await.unwrap();
    tx.send(Event::Shutdown).await.unwrap();
    let Harness {
      ctrl,
      record,
      mut events,
      mut reports,
      ..
    } = h;
    let started = Instant::now();
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Shutdown);
    assert!(started.elapsed() < Duration::from_secs(5), "no 30 s wait");
    assert!(reports.try_recv().is_err());
    assert_eq!(
      record.get().unwrap().play_session_id,
      "recordedsession",
      "for the next start's check"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_login_during_the_reload_wait_reaches_the_next_run() {
    let mut h = harness();
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    let tx = h.ctrl.events.clone();
    tx.send(Event::Reload).await.unwrap();
    tx.send(Event::LoginChanged).await.unwrap();
    let Harness {
      ctrl, mut events, ..
    } = h;
    // No result comes: the wait times out.
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    assert!(matches!(events.try_recv(), Ok(Event::LoginChanged)));
  }

  #[tokio::test(start_paused = true)]
  async fn a_kept_session_reports_a_film_once_even_if_the_check_was_pending() {
    let slot = CarrySlot::default();
    let mut h = harness();
    // The new settings move MC's zone: the film is stopped and reported.
    h.ctrl = h.ctrl.with_reload_check(Box::new(|| false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    h.start(MOVIE, None).await;
    let (player, record) = reload_during_the_restart_check(h, &slot).await;

    let Harness {
      ctrl,
      library,
      mut events,
      mut reports,
      ..
    } = harness_sharing(player, record);
    // The server still shows the film: its Stopped is on its way.
    *library.now_playing.lock().unwrap() = left_over(MOVIE, 590_000);
    let ctrl = ctrl.with_carry(slot).with_socket_up(true);
    reload_after(&ctrl, Duration::from_secs(3));
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    assert!(reports.try_recv().is_err(), "no second Stopped");
  }

  #[tokio::test(start_paused = true)]
  async fn a_reload_in_place_keeps_the_watchdog_and_the_tray_status() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.ctrl = h.ctrl.with_carry(slot.clone());
    let status = h.ctrl.status.subscribe();
    h.start(MOVIE, None).await;
    h.ctrl.watchdog.restarts.push_back(Instant::now());
    let Harness { ctrl, events, .. } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);
    assert!(
      matches!(*status.borrow(), PlaybackStatus::Playing { .. }),
      "the tray goes on showing the film"
    );
    let carry = slot.lock().unwrap();
    assert_eq!(
      carry.as_ref().unwrap().watchdog.restarts.len(),
      1,
      "the restart limit isn't reset by saving settings"
    );
  }

  #[tokio::test(start_paused = true)]
  async fn the_last_runs_restart_check_is_ignored_and_the_check_runs_again() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.record.save(&recorded(30_000, false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    let (player, record) = reload_during_the_restart_check(h, &slot).await;
    // MC still plays A.
    player.set_status(vec![Ok(playing_file(1, 31_000, AVATAR_UNC))]);

    let Harness {
      ctrl,
      mut events,
      mut reports,
      ..
    } = harness_sharing(player, record);
    let ctrl = ctrl.with_carry(slot).with_socket_up(true);
    // The last run's result, sent just before that run ended.
    ctrl
      .events
      .send(Event::RecoveryFinished {
        generation: 0,
        recovery: Recovery::Stale(StopReport {
          item_id: MOVIE.to_owned(),
          media_source_id: None,
          play_session_id: "recordedsession".to_owned(),
          position_ticks: Ticks(0),
          failed: false,
        }),
      })
      .await
      .unwrap();
    reload_after(&ctrl, Duration::from_secs(3));
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    let all = all_reports(&mut reports);
    assert!(stops(&all).is_empty(), "A plays on: not reported stopped");
    let start = starts(&all);
    assert_eq!(start.len(), 1, "A is taken over");
    assert_eq!(start[0].play_session_id, "recordedsession");
  }

  #[tokio::test(start_paused = true)]
  async fn a_film_carried_during_the_restart_check_still_gets_the_old_film_reported() {
    let slot = CarrySlot::default();
    let mut h = harness();
    h.record.save(&recorded(30_000, false));
    h.ctrl.recovery_started = true;
    h.ctrl.recovery_pending = true;
    // B is cast while the check for A runs.
    h.start(EPISODE, None).await;
    let b_session = starts(&h.reports())[0].play_session_id.clone();
    let key = h.key();
    h.player.set_status(vec![Ok(playing_file(
      key,
      60_000,
      r"\\nas\tv\Show\Season 1\e02.mkv",
    ))]);
    let (player, record) = reload_during_the_restart_check(h, &slot).await;
    assert_eq!(
      record.get().unwrap().item_id,
      MOVIE,
      "A's record is kept for the check"
    );

    let Harness {
      ctrl,
      mut events,
      mut reports,
      ..
    } = harness_sharing(player, record.clone());
    let ctrl = ctrl.with_carry(slot).with_socket_up(true);
    reload_after(&ctrl, Duration::from_secs(3));
    assert_eq!(ctrl.run(&mut events).await, RunEnd::Reload);
    let all = all_reports(&mut reports);
    let stopped = stops(&all);
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].play_session_id, "recordedsession", "A, once");
    assert!(starts(&all).is_empty(), "B goes on in its session");
    assert!(progresses(&all)
      .iter()
      .all(|p| p.play_session_id == b_session));
    assert_eq!(record.get().unwrap().item_id, EPISODE, "now B's");
  }

  #[tokio::test(start_paused = true)]
  async fn a_film_taken_over_keeps_its_tracks_and_the_remote_can_switch_them() {
    let mut h = harness();
    h.send(play_with_tracks(TRACKS, Some(3), Some(5))).await;
    h.finish_start().await;
    // Switched in the remote: the record follows.
    h.send(track_command("SetSubtitleStreamIndex", "4".into()))
      .await;
    let key = h.key();
    h.player.set_status(vec![Ok(playing_file(
      key,
      30_000,
      r"\\nas\movies\Tracks\tracks.mkv",
    ))]);
    let Harness {
      ctrl,
      player,
      record,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);

    let mut next = harness_sharing(player, record);
    next.send(Event::Connected).await;
    next.next_event().await;
    assert_eq!(next.ctrl.state_name(), "active");
    let reports = next.reports();
    assert_eq!(starts(&reports)[0].audio_stream_index, Some(3));
    assert_eq!(starts(&reports)[0].subtitle_stream_index, Some(4));
    let before = track_calls(&next).len();
    next
      .send(track_command("SetAudioStreamIndex", "2".into()))
      .await;
    assert_eq!(
      track_calls(&next)[before..].to_vec(),
      vec![Call::Track(TrackKind::Audio, 1)]
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_login_cancels_a_start_in_progress() {
    let mut h = harness();
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(600);
    h.send(play(MOVIE, None)).await;
    tokio::task::yield_now().await;
    let Harness {
      ctrl,
      player,
      record,
      events,
      ..
    } = h;
    drop(events);
    let (tx, mut rx) = mpsc::channel(16);
    tx.send(Event::LoginChanged).await.unwrap();
    assert_eq!(ctrl.run(&mut rx).await, RunEnd::LoginChanged);
    assert!(player.calls().contains(&Call::Stop));
    assert_eq!(record.get(), None);
  }

  #[tokio::test(start_paused = true)]
  async fn a_hang_while_a_film_starts_restarts_mc_and_starts_the_film_again() {
    let mut h = harness();
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(600);
    h.send(play(MOVIE, Some(Millis(300_000).to_ticks().0)))
      .await;
    tokio::task::yield_now().await;
    assert_eq!(h.ctrl.state_name(), "starting");
    // The resumed start won't hang.
    *h.player.play_delay.lock().unwrap() = Duration::ZERO;
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    assert_eq!(
      plays(&h.player),
      vec![
        (AVATAR_UNC.to_owned(), 300_000),
        (AVATAR_UNC.to_owned(), 300_000)
      ]
    );
    // The cancelled start is reported once, as failed (it never played).
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert!(stop[0].failed);
    assert_eq!(starts(&reports).len(), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn a_cast_to_a_hung_jriver_restarts_it_and_plays_the_cast() {
    // A real case: MC hung, Play from Jellyfin. The Windows check finds
    // the hang while the cast starts, and the cast plays after the restart.
    let h = harness();
    let Harness {
      ctrl,
      player,
      mut events,
      mut reports,
      ..
    } = h;
    let tx = ctrl.events.clone();
    player.hang();
    let run = tokio::spawn(async move { ctrl.run(&mut events).await });
    tx.send(play(MOVIE, Some(Millis(300_000).to_ticks().0)))
      .await
      .unwrap();
    // While a cast starts, and after its start timed out, a hang needs 20 s
    // of "Not responding" (MC may be opening the file from sleeping disks).
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(restarts(&player), 0, "not before the 20 s");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(restarts(&player), 1, "restarted within 25 s of the Play");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
      plays(&player),
      vec![
        (AVATAR_UNC.to_owned(), 300_000),
        (AVATAR_UNC.to_owned(), 300_000)
      ]
    );
    let mut started = 0;
    while let Ok(report) = reports.try_recv() {
      started += usize::from(matches!(report, Report::Start(_)));
    }
    assert_eq!(started, 1, "the cast is playing, so Jellyfin has controls");
    tx.send(Event::Shutdown).await.unwrap();
    assert_eq!(run.await.unwrap(), RunEnd::Shutdown);
  }

  #[tokio::test(start_paused = true)]
  async fn a_command_that_times_out_counts_toward_a_hang() {
    for (command, seek) in [
      ("Pause", None),
      ("Unpause", None),
      ("PlayPause", None),
      ("Seek", Some(Millis(60_000).to_ticks().0)),
    ] {
      let mut h = harness();
      h.start(MOVIE, None).await;
      h.player.hang();
      h.send(playstate(command, seek)).await;
      assert!(h.ctrl.watchdog.silent_since.is_some(), "{command}");
      tick_until(&mut h, |h| restarts(&h.player) > 0).await;
      assert_eq!(restarts(&h.player), 1, "{command}");
    }
  }

  #[tokio::test(start_paused = true)]
  async fn two_short_freezes_with_an_answer_between_are_not_one_hang() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.player.hang();
    h.send(playstate("Pause", None)).await;
    tick(&mut h).await;
    assert!(h.ctrl.watchdog.not_responding_since.is_some());
    // MC recovers and answers a command: that hang is over.
    *h.player.hung.lock().unwrap() = false;
    *h.player.not_responding.lock().unwrap() = false;
    h.send(playstate("Unpause", None)).await;
    tick(&mut h).await;
    assert!(h.ctrl.watchdog.silent_since.is_none());
    assert!(h.ctrl.watchdog.not_responding_since.is_none());
    // A new freeze is timed from its own start.
    h.player.hang();
    h.send(playstate("Pause", None)).await;
    h.ctrl.watch_windows().await;
    assert_eq!(restarts(&h.player), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn over_the_restart_limit_the_hang_stays_known_without_new_warnings() {
    let mut h = harness();
    *h.player.hangs_again.lock().unwrap() = true;
    h.player.hang();
    for _ in 0..60 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 3);
    // Still hung and over the limit: the hang stays known, so later checks
    // neither warn again nor probe MCWS every second.
    assert!(h.ctrl.watchdog.not_responding_since.is_some());
    assert!(h.ctrl.watchdog.limit_logged);
    let probes = *h.player.health_checks.lock().unwrap();
    for _ in 0..20 {
      tick(&mut h).await;
    }
    assert_eq!(*h.player.health_checks.lock().unwrap(), probes);
    assert_eq!(restarts(&h.player), 3);
  }

  #[tokio::test(start_paused = true)]
  async fn while_a_cast_opens_a_file_mc_gets_the_longer_hang_time() {
    let mut h = harness();
    // The file takes long to open (NAS disks spinning up), and MC's window
    // freezes meanwhile.
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(600);
    h.send(play(MOVIE, None)).await;
    tokio::task::yield_now().await;
    h.player.hang();
    for _ in 0..12 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0, "not after 3 s while starting");
    assert_eq!(h.ctrl.state_name(), "starting");
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1, "but after 20 s");
  }

  #[tokio::test(start_paused = true)]
  async fn a_start_that_timed_out_still_gets_the_longer_hang_time() {
    let mut h = harness();
    // MC freezes while opening the file: the start's calls time out.
    h.player.hang();
    h.send(play(MOVIE, None)).await;
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "idle", "the start failed on a timeout");
    for _ in 0..12 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0, "not after 3 s");
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1, "but after 20 s");
    // The failed cast is played after the restart.
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
  }

  #[tokio::test(start_paused = true)]
  async fn a_long_start_time_hang_setting_still_plays_the_failed_cast() {
    let mut h = harness();
    h.ctrl.options.hang_confirm_starting = Duration::from_secs(90);
    h.player.hang();
    h.send(play(MOVIE, None)).await;
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "idle");
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    for _ in 0..90 {
      if restarts(&h.player) > 0 {
        break;
      }
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 1, "after the 90 s");
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active", "and the cast is played");
  }

  #[tokio::test(start_paused = true)]
  async fn a_reload_hands_a_film_over_only_if_the_target_stays() {
    for keeps in [true, false] {
      let mut h = harness();
      h.ctrl = h.ctrl.with_reload_check(Box::new(move || keeps));
      h.start(MOVIE, None).await;
      h.reports();
      let Harness {
        ctrl,
        player,
        record,
        events,
        mut reports,
        ..
      } = h;
      drop(events);
      let (tx, mut rx) = mpsc::channel(16);
      tx.send(Event::Reload).await.unwrap();
      assert_eq!(ctrl.run(&mut rx).await, RunEnd::Reload);
      let mut stopped = 0;
      while let Ok(report) = reports.try_recv() {
        stopped += usize::from(matches!(report, Report::Stopped(_)));
      }
      if keeps {
        assert!(!player.calls().contains(&Call::Stop), "plays on");
        assert_eq!(stopped, 0);
        assert!(record.get().is_some(), "for the next run to take over");
      } else {
        assert!(player.calls().contains(&Call::Stop), "stopped as on Quit");
        assert_eq!(stopped, 1);
        assert_eq!(record.get(), None);
      }
    }
  }

  #[tokio::test(start_paused = true)]
  async fn no_restart_when_restarts_are_turned_off() {
    let mut h = harness();
    h.ctrl.options.max_restarts_per_hour = 0;
    h.player.hang();
    for _ in 0..20 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0);
    assert!(h.ctrl.watchdog.limit_logged);
  }

  #[tokio::test(start_paused = true)]
  async fn a_restart_never_rests_on_an_old_windows_answer() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.player.hang();
    tick(&mut h).await;
    assert!(h.ctrl.watchdog.not_responding_since.is_some());
    // Windows says MC responds again, but MCWS still times out (a second,
    // separate freeze) and no Windows check has run since.
    *h.player.not_responding.lock().unwrap() = false;
    tokio::time::sleep(Duration::from_secs(5)).await;
    h.ctrl.poll().await;
    assert_eq!(restarts(&h.player), 0, "a timeout alone restarts nothing");
    tick(&mut h).await;
    assert!(h.ctrl.watchdog.not_responding_since.is_none());
    assert_eq!(restarts(&h.player), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_failing_tasklist_breaks_the_chain_but_is_not_responds() {
    let mut h = harness();
    *h.player.hangs_again.lock().unwrap() = true;
    h.player.hang();
    for _ in 0..30 {
      tick(&mut h).await;
    }
    assert!(h.ctrl.watchdog.limit_logged, "over the limit, logged");
    *h.player.windows_unknown.lock().unwrap() = true;
    for _ in 0..10 {
      tick(&mut h).await;
    }
    assert!(
      h.ctrl.watchdog.not_responding_since.is_none(),
      "no '3 s in a row'"
    );
    assert!(h.ctrl.watchdog.limit_logged, "and not 'responds again'");
    // Windows answers again: the 3 s start over.
    *h.player.windows_unknown.lock().unwrap() = false;
    tick(&mut h).await;
    assert!(h.ctrl.watchdog.not_responding_since.is_some());
  }

  #[tokio::test(start_paused = true)]
  async fn a_playing_film_still_ends_when_windows_cannot_tell() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    let key = h.key();
    h.player
      .set_status(vec![Ok(info(PlayState::Playing, key, 2_000_000))]);
    h.ctrl.poll().await;
    h.reports();
    // One "Not responding", then tasklist fails while MCWS times out.
    h.player.hang();
    tick(&mut h).await;
    *h.player.windows_unknown.lock().unwrap() = true;
    for _ in 0..40 {
      if h.ctrl.state_name() == "idle" {
        break;
      }
      h.ctrl.poll().await;
      tick(&mut h).await;
    }
    assert_eq!(h.ctrl.state_name(), "idle", "the 30 s rule ended it");
    assert_eq!(restarts(&h.player), 0);
    let reports = h.reports();
    assert_eq!(stops(&reports).len(), 1);
    assert_eq!(stops(&reports)[0].position_ticks, Ticks(2_000_000 * 10_000));
  }

  #[tokio::test(start_paused = true)]
  async fn a_cast_while_mc_stays_hung_past_the_limit_is_reported_failed() {
    let mut h = harness();
    *h.player.hangs_again.lock().unwrap() = true;
    h.player.hang();
    for _ in 0..30 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 3);
    h.reports();
    *h.player.play_delay.lock().unwrap() = Duration::from_secs(600);
    h.send(play(MOVIE, None)).await;
    tokio::task::yield_now().await;
    assert_eq!(h.ctrl.state_name(), "starting");
    // The 1 s check while starting keeps MCWS's silence known.
    h.ctrl.health_check().await;
    tick(&mut h).await;
    assert_eq!(h.ctrl.state_name(), "idle");
    assert_eq!(restarts(&h.player), 3, "still left alone");
    let reports = h.reports();
    let stop = stops(&reports);
    assert_eq!(stop.len(), 1);
    assert!(stop[0].failed);
  }

  #[tokio::test(start_paused = true)]
  async fn an_mc_loading_when_the_bridge_starts_gets_a_minute() {
    let mut h = harness();
    // As in a fresh Controller: MC hasn't answered yet.
    h.ctrl.watchdog.mc_appeared = Some(Instant::now());
    h.player.hang();
    for _ in 0..40 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0, "within its first minute");
    tokio::time::sleep(MC_STARTUP_GRACE).await;
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn a_poll_that_finds_mc_gone_gives_a_new_mc_its_grace() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    // MC vanishes mid-film (refused), and the user starts it again at once.
    *h.player.not_running.lock().unwrap() = true;
    h.player.set_status(vec![Err(())]);
    h.ctrl.poll().await;
    *h.player.not_running.lock().unwrap() = false;
    h.player.hang();
    h.ctrl.poll().await;
    assert!(h.ctrl.watchdog.mc_appeared.is_some());
    for _ in 0..20 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0);
  }

  #[tokio::test(start_paused = true)]
  async fn an_automatic_resume_that_hangs_again_is_not_resumed_again() {
    let mut h = harness();
    h.start(MOVIE, Some(Millis(300_000).to_ticks().0)).await;
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    // The automatic resume starts fine and plays.
    h.finish_start().await;
    assert_eq!(h.ctrl.state_name(), "active");
    // MC hangs again within 10 min.
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 1).await;
    assert_eq!(restarts(&h.player), 2);
    assert_eq!(h.ctrl.state_name(), "idle", "not resumed a second time");
    assert_eq!(plays(&h.player).len(), 2);
  }

  #[tokio::test(start_paused = true)]
  async fn a_slow_mc_launch_is_not_a_hang() {
    let mut h = harness();
    *h.player.launching.lock().unwrap() = true;
    h.player.hang();
    for _ in 0..20 {
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0);
    // Once the launch window is over, the same hang counts.
    *h.player.launching.lock().unwrap() = false;
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn an_mc_started_by_someone_else_gets_a_minute_to_load() {
    let mut h = harness();
    // Not running (refused), then started by the user or at logon, and
    // still loading: its web server answers, the rest doesn't yet.
    *h.player.not_running.lock().unwrap() = true;
    h.ctrl.health_check().await;
    *h.player.not_running.lock().unwrap() = false;
    h.player.hang();
    // 15 rounds of a 2 s check and a 1 s tick: 45 s.
    for _ in 0..15 {
      h.ctrl.health_check().await;
      tick(&mut h).await;
    }
    assert_eq!(restarts(&h.player), 0, "within its first minute");
    tokio::time::sleep(MC_STARTUP_GRACE).await;
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1, "a minute on, a hang is a hang");
  }

  #[tokio::test(start_paused = true)]
  async fn an_mc_that_answered_once_gets_no_start_up_grace() {
    let mut h = harness();
    *h.player.not_running.lock().unwrap() = true;
    h.ctrl.health_check().await;
    *h.player.not_running.lock().unwrap() = false;
    h.ctrl.health_check().await;
    assert!(h.ctrl.watchdog.mc_appeared.is_none(), "it answered: loaded");
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    assert_eq!(restarts(&h.player), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn a_new_cast_soon_after_an_automatic_resume_is_still_played() {
    let mut h = harness();
    h.start(MOVIE, None).await;
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 0).await;
    // The automatic resume.
    h.finish_start().await;
    assert_eq!(plays(&h.player).len(), 2);
    // The user casts something else, and MC hangs again within 10 min.
    h.start(EPISODE, None).await;
    h.player.hang();
    tick_until(&mut h, |h| restarts(&h.player) > 1).await;
    assert_eq!(restarts(&h.player), 2);
    assert_eq!(h.ctrl.state_name(), "starting", "the user's cast is played");
    h.finish_start().await;
    let episode = r"\\nas\tv\Show\Season 1\e02.mkv";
    assert_eq!(plays(&h.player).last().unwrap().0, episode);
  }
}
