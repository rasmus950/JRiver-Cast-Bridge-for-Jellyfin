//! The bridge service: Jellyfin login and socket, JRiver, the controller and
//! the report queue, wired together and run until shutdown. Used by the tray
//! program (`jriver-cast-bridge`) and by `bridge-cli run` in a console.
//!
//! Without a saved login it waits for one (the tray opens the login window)
//! instead of stopping. Log out, or a new login, ends the Jellyfin session
//! and starts over with the same JRiver; after a new login a playing film
//! is taken over through the playback record. The hung-MC watchdog belongs to
//! the session's controller: its limits start over with each login, and
//! it doesn't run while logged out.

use std::{error::Error, path::PathBuf, sync::Arc, time::Duration};

use crate::socket::{JellyfinWebSocket, JellyfinWebSocketEvent};
use jriver_mcws::{JRiverPlayer, McwsClient, PlayerOptions};
use log::{debug, error, info, warn};
use tokio::sync::{mpsc, watch};

use crate::{
  config::{self, Config, ConfigError, ConfigSource, JellyfinSection},
  controller::{CarrySlot, Controller, Event, PlaybackStatus, RunEnd},
  credentials::{self, CredentialStore, StoredLogin},
  ipc,
  jellyfin::{Identity, JellyfinClient, DEFAULT_TIMEOUT},
  jriver::JRiver,
  path_map::PathMapper,
  player_start,
  record::{MemoryRecord, RecordFile, RecordStore},
  reporter::{self, Reporter, ReporterOptions},
  settings,
  status::{Account, BridgeStatus, Connection},
};

/// How long shutdown waits for queued reports to reach the server.
const REPORT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30, 60];

pub type BoxError = Box<dyn Error + Send + Sync>;

/// Why [`serve`] returned without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
  /// Tray Quit, `--quit`, or Ctrl+C in a console.
  Quit,
  /// The settings were saved: load the config again and serve again.
  Reload,
}

/// Run the bridge until [`Event::Shutdown`] (tray Quit, `--quit`, Ctrl+C).
/// When the settings are saved ([`Event::Reload`]) the config at
/// `config_arg` (or the default one) is loaded again and the bridge starts
/// over with it. A config that can't be used is published as the status's
/// error (and logged), and the bridge waits for the settings to be saved
/// again; Quit still ends it normally.
///
/// `start_player`: start Jellyfin Media Player once registered (the tray
/// does; `bridge-cli run`, a test tool, doesn't).
pub async fn run(
  config_arg: Option<PathBuf>,
  loaded: Result<(Config, ConfigSource), ConfigError>,
  events_tx: mpsc::Sender<Event>,
  mut events_rx: mpsc::Receiver<Event>,
  status: watch::Sender<BridgeStatus>,
  start_player: bool,
) -> Result<(), BoxError> {
  // Set by the first successful registration as a cast target.
  let (registered, registered_rx) = watch::channel(false);
  let player_start = start_player.then(|| {
    tokio::spawn(player_start::start_after_registration(
      registered_rx,
      config_arg.clone(),
    ))
  });
  // Quit, new logins and reloads from other programs (installer, login and
  // settings windows). Made once: it listens across reloads, and while the
  // config is unusable.
  let (account_tx, account_rx) = watch::channel(String::new());
  let _pipe = match ipc::serve(events_tx.clone(), account_rx) {
    Ok(pipe) => Some(pipe),
    Err(err) => {
      warn!(
        "can't listen on the local pipe ({err}); --quit, login notices and saved settings won't reach this bridge"
      );
      None
    }
  };
  let ctrl_c = {
    let events_tx = events_tx.clone();
    tokio::spawn(async move {
      match tokio::signal::ctrl_c().await {
        Ok(()) => {
          info!("Ctrl+C: shutting down");
          let _ = events_tx.send(Event::Shutdown).await;
        }
        // Keep running; only Ctrl+C itself is unavailable.
        Err(err) => warn!("can't listen for Ctrl+C ({err})"),
      }
    })
  };

  // The config a reload decided on (see `run_session`): the next run uses
  // that same reading, so a second save in between can't slip in.
  let reloaded: Reloaded = Arc::default();
  let mut loaded = loaded;
  loop {
    status.send_modify(|s| s.error = None);
    match serve(
      loaded,
      &events_tx,
      &mut events_rx,
      &status,
      &account_tx,
      &reloaded,
      &registered,
    )
    .await
    {
      Ok(Stopped::Quit) => break,
      Ok(Stopped::Reload) => {}
      Err(err) => {
        error!("{err}");
        status.send_modify(|s| {
          s.error = Some(err.to_string());
          // No account to log in or out of until the config works.
          s.account = Account::Unknown;
        });
        // Quit (also over the pipe) ends the bridge as usual; the error
        // was logged and shown.
        if !wait_for_reload(&mut events_rx).await {
          break;
        }
      }
    }
    info!("loading the settings again");
    let decided = reloaded
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .take();
    loaded = decided.unwrap_or_else(|| config::load(config_arg.as_deref()));
  }
  ctrl_c.abort();
  if let Some(task) = player_start {
    task.abort();
  }
  Ok(())
}

/// The config read by a reload's check, kept for the next run.
type Reloaded = Arc<std::sync::Mutex<Option<Result<(Config, ConfigSource), ConfigError>>>>;

/// With an unusable config: `true` once the settings are saved again,
/// `false` on Quit.
async fn wait_for_reload(events_rx: &mut mpsc::Receiver<Event>) -> bool {
  loop {
    match events_rx.recv().await {
      Some(Event::Reload) => return true,
      None | Some(Event::Shutdown) => return false,
      Some(other) => debug!("the config can't be used; ignoring {other:?}"),
    }
  }
}

/// What stays the same across Jellyfin logins: everything a config sets up
/// besides the Jellyfin connection. Rebuilt in place by a reload that keeps
/// the `[jellyfin]` settings.
struct Shared {
  jellyfin: JellyfinSection,
  player: Arc<JRiver<McwsClient>>,
  mapper: Arc<PathMapper>,
  config: Config,
  /// The config file, read again on a reload to decide whether a playing
  /// film can be handed over.
  config_path: PathBuf,
  /// Where that reading is kept for the next run.
  reloaded: Reloaded,
  /// Set by a successful registration (to start Jellyfin Media Player).
  registered: watch::Sender<bool>,
  /// Shows the player's "Starting JRiver…" through the tray.
  notice_forward: tokio::task::JoinHandle<()>,
}

impl Shared {
  async fn build(
    config: Config,
    config_path: PathBuf,
    reloaded: &Reloaded,
    registered: &watch::Sender<bool>,
    status: &watch::Sender<BridgeStatus>,
  ) -> Result<Self, BoxError> {
    let jellyfin = config.jellyfin()?.clone();
    // A Media Network password still in the file (before 0.6.0) moves to
    // Credential Manager; this run uses it either way.
    match settings::move_password_to_credentials(
      &config_path,
      &config,
      &credentials::CredentialManager,
    ) {
      Ok(true) => {
        info!("moved the Media Network password from the config file to Credential Manager")
      }
      Ok(false) => {}
      Err(err) => warn!(
        "can't move the Media Network password to Credential Manager ({err}); it stays in the config file for now"
      ),
    }
    let mcws = McwsClient::new(config.mcws_config(credentials::mcws_password(&config.mcws)))?;
    match mcws.alive().await {
      Ok(alive) => info!(
        "JRiver: {} {} on {}",
        alive.program_name, alive.program_version, alive.friendly_name
      ),
      Err(err) => warn!("JRiver isn't answering yet ({err}); it will be started for the first cast if mcws.exe_path is set"),
    }
    let player = Arc::new(JRiver::new(
      JRiverPlayer::new(mcws, PlayerOptions::default()),
      config.launch_options(),
    ));
    let mapper = Arc::new(config.path_mapper()?);
    if mapper.is_empty() {
      warn!("the config has no [[path_map]] rules, so no cast can be played");
    }
    // "Starting JRiver…" in the middle of the screen, through the tray.
    let notice_forward = {
      let mut notices = player.notices();
      let status = status.clone();
      tokio::spawn(async move {
        while notices.changed().await.is_ok() {
          let notice = *notices.borrow_and_update();
          status.send_modify(|s| s.jriver = notice);
        }
      })
    };
    Ok(Self {
      jellyfin,
      player,
      mapper,
      config,
      config_path,
      reloaded: reloaded.clone(),
      registered: registered.clone(),
      notice_forward,
    })
  }

  /// After a reload's controller run ended: if the new settings keep the
  /// `[jellyfin]` section, rebuild the rest from them and return `true`, so
  /// the Jellyfin session (and Jellyfin Media Player's choice of this cast
  /// target) stays. `false`: the bridge starts over with the new settings,
  /// which also shows a config that can't be used.
  async fn reload_in_place(&mut self, status: &watch::Sender<BridgeStatus>) -> bool {
    let next = in_place_config(
      &self
        .reloaded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner),
      &self.jellyfin,
    );
    let Some((config, path)) = next else {
      return false;
    };
    match Self::build(config, path, &self.reloaded, &self.registered, status).await {
      Ok(next) => {
        self
          .reloaded
          .lock()
          .unwrap_or_else(std::sync::PoisonError::into_inner)
          .take();
        *self = next;
        status.send_modify(|s| s.jriver = None);
        true
      }
      Err(err) => {
        warn!("the new settings can't be applied in place ({err}); starting over with them");
        false
      }
    }
  }
}

/// The config a reload read, if it can be applied without closing the
/// Jellyfin session: a usable config file with the same `[jellyfin]`
/// section. The slot itself is left alone.
fn in_place_config(
  reloaded: &Option<Result<(Config, ConfigSource), ConfigError>>,
  jellyfin: &JellyfinSection,
) -> Option<(Config, PathBuf)> {
  match reloaded.as_ref() {
    Some(Ok((new, ConfigSource::File(path)))) if new.jellyfin.as_ref() == Some(jellyfin) => {
      Some((new.clone(), path.clone()))
    }
    _ => None,
  }
}

impl Drop for Shared {
  fn drop(&mut self) {
    self.notice_forward.abort();
  }
}

/// The new settings keep the Jellyfin account and the MC zone a playing
/// film belongs to, so the next run can take it over.
fn same_target(old: &Config, new: &Config) -> bool {
  // The saved-login account, which ignores e.g. a trailing `/`.
  let account = |config: &Config| {
    config.jellyfin.as_ref().map(|j| {
      CredentialStore::new(&j.url, &j.profile)
        .account()
        .to_owned()
    })
  };
  account(old) == account(new)
    && old.mcws.url == new.mcws.url
    && old.mcws.zone_id == new.mcws.zone_id
}

async fn serve(
  loaded: Result<(Config, ConfigSource), ConfigError>,
  events_tx: &mpsc::Sender<Event>,
  events_rx: &mut mpsc::Receiver<Event>,
  status: &watch::Sender<BridgeStatus>,
  account_tx: &watch::Sender<String>,
  reloaded: &Reloaded,
  registered: &watch::Sender<bool>,
) -> Result<Stopped, BoxError> {
  let (config, source) = loaded?;
  let config_path = match source {
    ConfigSource::File(path) => {
      info!("config: {}", path.display());
      path
    }
    ConfigSource::Defaults(path) => {
      return Err(format!("{} not found; use Settings… to create it", path.display()).into())
    }
  };
  let mut shared = Shared::build(config, config_path, reloaded, registered, status).await?;
  // Reloads in place keep the `[jellyfin]` section, so this stays right.
  let store = CredentialStore::new(&shared.jellyfin.url, &shared.jellyfin.profile);
  account_tx.send_replace(store.account().to_owned());

  let result = loop {
    let stored = match store.load() {
      Ok(Some(stored)) => stored,
      Ok(None) => {
        info!(
          "not logged in to Jellyfin ({}); waiting for a login",
          store.account()
        );
        status.send_modify(|s| {
          s.account = Account::LoggedOut;
          s.connection = Connection::Connecting;
        });
        match wait_for_login(events_rx).await {
          Some(stopped) => break Ok(stopped),
          None => continue,
        }
      }
      Err(err) => {
        error!("{err}");
        status.send_modify(|s| s.account = Account::LoggedOut);
        match wait_for_login(events_rx).await {
          Some(stopped) => break Ok(stopped),
          None => continue,
        }
      }
    };
    status.send_modify(|s| {
      s.account = Account::LoggedIn {
        user: stored.user_name.clone(),
      };
      s.connection = Connection::Connecting;
      s.playback = PlaybackStatus::Idle;
    });
    match run_session(&mut shared, &store, stored, events_tx, events_rx, status).await {
      Ok(RunEnd::Shutdown) => break Ok(Stopped::Quit),
      Ok(RunEnd::Reload) => break Ok(Stopped::Reload),
      Ok(RunEnd::Logout | RunEnd::LoginChanged) => {}
      Err(err) => break Err(err),
    }
  };
  drop(shared);
  status.send_modify(|s| s.jriver = None);
  result
}

/// While logged out: `None` once a login is saved, or why the bridge stops.
async fn wait_for_login(events_rx: &mut mpsc::Receiver<Event>) -> Option<Stopped> {
  loop {
    match events_rx.recv().await {
      Some(Event::LoginChanged) => return None,
      None | Some(Event::Shutdown) => return Some(Stopped::Quit),
      Some(Event::Reload) => return Some(Stopped::Reload),
      Some(other) => debug!("not logged in; ignoring {other:?}"),
    }
  }
}

/// One Jellyfin login: register, take casts, until Quit, Log out or a new
/// login. Log out revokes and deletes the login here. A reload that keeps
/// the `[jellyfin]` settings stays in the session: the socket stays up, so
/// the server keeps the session and Jellyfin Media Player keeps this cast
/// target picked; only the controller and JRiver side start over.
async fn run_session(
  shared: &mut Shared,
  store: &CredentialStore,
  stored: StoredLogin,
  events_tx: &mpsc::Sender<Event>,
  events_rx: &mut mpsc::Receiver<Event>,
  status: &watch::Sender<BridgeStatus>,
) -> Result<RunEnd, BoxError> {
  let jellyfin = &shared.jellyfin;
  let client = Arc::new(JellyfinClient::new(
    &jellyfin.url,
    Identity {
      device_name: jellyfin.device_name.clone(),
      device_id: stored.device_id.clone(),
    },
    Some(stored.login()),
    DEFAULT_TIMEOUT,
  )?);
  info!(
    "Jellyfin: {} as {}, cast target \"{}\"",
    client.server_url(),
    stored.user_name,
    jellyfin.device_name
  );

  let (reporter, reporter_task) = reporter::spawn(client.clone(), ReporterOptions::default());
  let link = Link {
    client: client.clone(),
    reporter,
    record: playback_record(store, &stored),
    carry: CarrySlot::default(),
  };

  let socket = Arc::new(JellyfinWebSocket::without_session_updates());
  let mut socket_events = socket
    .take_event_receiver()
    .ok_or("the socket's event receiver was already taken")?;
  let forward = {
    let events_tx = events_tx.clone();
    let status = status.clone();
    tokio::spawn(async move {
      while let Some(event) = socket_events.recv().await {
        let event = match event {
          JellyfinWebSocketEvent::Connected | JellyfinWebSocketEvent::Reconnected => {
            info!("Jellyfin: socket connected");
            status.send_modify(|s| s.connection = Connection::Connected);
            Event::Connected
          }
          JellyfinWebSocketEvent::ConnectionLost => {
            status.send_modify(|s| s.connection = Connection::Lost);
            Event::ConnectionLost
          }
          JellyfinWebSocketEvent::Command(command) => Event::Command(command),
        };
        if events_tx.send(event).await.is_err() {
          break;
        }
      }
    })
  };
  let connect = {
    let socket = socket.clone();
    let url = client.websocket_url()?;
    let token = stored.access_token.clone();
    let status = status.clone();
    let user = stored.user_name.clone();
    tokio::spawn(async move { connect_with_retry(&socket, &url, &token, &status, &user).await })
  };

  info!("waiting for casts");
  let mut socket_up = false;
  let end = loop {
    let end = run_controller(shared, &link, socket_up, events_tx, events_rx, status).await;
    if end != RunEnd::Reload || !shared.reload_in_place(status).await {
      break end;
    }
    info!("settings applied; the Jellyfin session is kept, so cast menus keep this target");
    // Once up, the socket reconnects by itself and says so; until then a
    // new controller gets its Connected from the socket as usual.
    socket_up = matches!(status.borrow().connection, Connection::Connected);
  };

  connect.abort();
  socket.disconnect().await;
  forward.abort();
  // The report queue ends once every sender is gone and it's empty.
  drop(link);
  if tokio::time::timeout(REPORT_FLUSH_TIMEOUT, reporter_task)
    .await
    .is_err()
  {
    warn!("Jellyfin: some playback reports could not be delivered before exit");
  }
  match end {
    RunEnd::Shutdown => info!("bridge stopped"),
    RunEnd::LoginChanged => info!("Jellyfin: reconnecting with the new login"),
    RunEnd::Reload => info!("reloading the settings"),
    RunEnd::Logout => {
      match client.logout().await {
        Ok(()) => info!(
          "Jellyfin: logged out {} (the login is revoked on the server)",
          stored.user_name
        ),
        Err(err) => warn!("Jellyfin: couldn't revoke the login on the server: {err}"),
      }
      match store.delete() {
        Ok(_) => info!("deleted the saved login {}", store.account()),
        Err(err) => error!("couldn't delete the saved login {}: {err}", store.account()),
      }
    }
  }
  status.send_modify(|s| {
    s.connection = Connection::Connecting;
    s.playback = PlaybackStatus::Idle;
  });
  Ok(end)
}

/// What a login's controllers share with the server, kept across reloads
/// in place.
struct Link {
  client: Arc<JellyfinClient>,
  reporter: Reporter,
  record: Arc<dyn RecordStore>,
  /// A reload's playing film, handed from one controller to the next.
  carry: CarrySlot,
}

/// One controller run within a Jellyfin session, with `shared`'s JRiver
/// side, until it ends.
async fn run_controller(
  shared: &Shared,
  link: &Link,
  socket_up: bool,
  events_tx: &mpsc::Sender<Event>,
  events_rx: &mut mpsc::Receiver<Event>,
  status: &watch::Sender<BridgeStatus>,
) -> RunEnd {
  // Registered with the controller's first Connected (or at once, when the
  // socket is up): a reload that changed the setting registers it anew.
  link
    .client
    .set_track_commands(shared.config.bridge.tracks_from_jellyfin);
  let (playback_tx, mut playback_rx) = watch::channel(PlaybackStatus::Idle);
  let controller = Controller::new(
    shared.player.clone(),
    link.client.clone(),
    shared.mapper.clone(),
    link.reporter.clone(),
    shared.config.controller_options(),
    events_tx.clone(),
  )
  .with_status(playback_tx)
  .with_record(link.record.clone())
  .with_carry(link.carry.clone())
  .with_registered(shared.registered.clone())
  .with_socket_up(socket_up)
  .with_reload_check({
    // On a reload: hand a playing film over only if the new settings keep
    // its Jellyfin account and MC zone; otherwise it's stopped and
    // reported, as on Quit. A config that can't be read counts as changed.
    // What's read here is what the next run uses.
    let path = shared.config_path.clone();
    let old = shared.config.clone();
    let reloaded = shared.reloaded.clone();
    Box::new(move || {
      let loaded = config::load(Some(&path));
      let keeps = matches!(&loaded, Ok((new, _)) if same_target(&old, new));
      *reloaded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(loaded);
      keeps
    })
  });
  let playback_forward = {
    let status = status.clone();
    tokio::spawn(async move {
      while playback_rx.changed().await.is_ok() {
        let playback = playback_rx.borrow_and_update().clone();
        status.send_modify(|s| s.playback = playback);
      }
    })
  };
  let end = controller.run(events_rx).await;
  playback_forward.abort();
  end
}

/// The record of the playing film, in the app folder beside the lock (one
/// bridge per Windows user), for this account and Jellyfin user.
fn playback_record(store: &CredentialStore, stored: &StoredLogin) -> Arc<dyn RecordStore> {
  match config::app_dir() {
    Ok(dir) => Arc::new(RecordFile::new(&dir, store.account(), &stored.user_id)),
    Err(err) => {
      warn!("no app folder ({err}); a killed bridge won't be able to take its film over");
      Arc::new(MemoryRecord::default())
    }
  }
}

/// The socket reconnects on its own once connected; only the first
/// connection needs retrying here (the server may be down at boot). A
/// rejected login shows in the tray, which then offers Log in….
async fn connect_with_retry(
  socket: &JellyfinWebSocket,
  url: &str,
  token: &str,
  status: &watch::Sender<BridgeStatus>,
  user: &str,
) {
  let user_agent = JellyfinClient::user_agent();
  let mut attempt = 0usize;
  loop {
    match socket.connect_with_user_agent(url, Some(&user_agent)).await {
      Ok(()) => {
        status.send_modify(|s| {
          if matches!(s.account, Account::Rejected { .. }) {
            s.account = Account::LoggedIn {
              user: user.to_owned(),
            };
          }
        });
        return;
      }
      Err(err) => {
        let message = err.to_string().replace(token, "<redacted>");
        let delay =
          Duration::from_secs(CONNECT_BACKOFF_SECS[attempt.min(CONNECT_BACKOFF_SECS.len() - 1)]);
        attempt += 1;
        if message.contains("401") {
          error!(
            "Jellyfin: the server rejected the saved login of {user} ({message}); log in again from the tray. Retrying in {delay:?}"
          );
          status.send_modify(|s| {
            s.account = Account::Rejected {
              user: user.to_owned(),
            };
          });
        } else {
          warn!("Jellyfin: can't connect the socket ({message}); retrying in {delay:?}");
        }
        tokio::time::sleep(delay).await;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_reload_keeps_a_film_only_with_the_same_account_and_zone() {
    let base = config::parse(config::EXAMPLE, std::path::Path::new("t.toml")).unwrap();
    let mut new = base.clone();
    new.bridge.poll_interval_ms = 500;
    new.watchdog.hang_after_s = 5;
    new.path_map.clear();
    let url = &mut new.jellyfin.as_mut().unwrap().url;
    url.push('/');
    assert!(same_target(&base, &new), "other settings don't matter");
    let changes: [fn(&mut Config); 4] = [
      |c| c.jellyfin.as_mut().unwrap().url = "http://other:8096".to_owned(),
      |c| c.jellyfin.as_mut().unwrap().profile = "verifier".to_owned(),
      |c| c.mcws.url = "http://127.0.0.1:52199/MCWS/v1/".to_owned(),
      |c| c.mcws.zone_id = 1,
    ];
    for change in changes {
      let mut new = base.clone();
      change(&mut new);
      assert!(!same_target(&base, &new));
    }
  }

  #[test]
  fn a_reload_stays_in_the_jellyfin_session_only_with_the_same_jellyfin_section() {
    let path = PathBuf::from("t.toml");
    let base = config::parse(config::EXAMPLE, &path).unwrap();
    let jellyfin = base.jellyfin.clone().unwrap();
    let read = |config: Config| Some(Ok((config, ConfigSource::File(path.clone()))));

    let mut new = base.clone();
    new.watchdog.hang_after_s = 5;
    new.mcws.zone_id = 1;
    let (config, at) = in_place_config(&read(new), &jellyfin).expect("in place");
    assert_eq!(config.watchdog.hang_after_s, 5);
    assert_eq!(at, path);

    let changes: [fn(&mut Config); 3] = [
      |c| c.jellyfin.as_mut().unwrap().url = "http://other:8096".to_owned(),
      |c| c.jellyfin.as_mut().unwrap().device_name = "Other".to_owned(),
      |c| c.jellyfin.as_mut().unwrap().profile = "verifier".to_owned(),
    ];
    for change in changes {
      let mut new = base.clone();
      change(&mut new);
      assert!(in_place_config(&read(new), &jellyfin).is_none());
    }
    // A config that can't be used, a missing file, or no reading at all:
    // the bridge starts over (and shows the error).
    let missing = Some(Ok((base.clone(), ConfigSource::Defaults(path.clone()))));
    assert!(in_place_config(&missing, &jellyfin).is_none());
    let broken =
      Some(config::parse("[jellyfin", &path).map(|c| (c, ConfigSource::File(path.clone()))));
    assert!(matches!(broken, Some(Err(_))));
    assert!(in_place_config(&broken, &jellyfin).is_none());
    assert!(in_place_config(&None, &jellyfin).is_none());
  }

  #[tokio::test]
  async fn with_an_unusable_config_the_bridge_waits_for_new_settings_or_quit() {
    let (tx, mut rx) = mpsc::channel(8);
    tx.send(Event::LoginChanged).await.unwrap();
    tx.send(Event::Reload).await.unwrap();
    assert!(wait_for_reload(&mut rx).await);
    tx.send(Event::Shutdown).await.unwrap();
    assert!(!wait_for_reload(&mut rx).await);
  }

  #[tokio::test]
  async fn logged_out_the_bridge_waits_for_a_login_or_quit_and_ignores_the_rest() {
    let (tx, mut rx) = mpsc::channel(8);
    tx.send(Event::Logout).await.unwrap();
    tx.send(Event::ConnectionLost).await.unwrap();
    tx.send(Event::LoginChanged).await.unwrap();
    assert_eq!(wait_for_login(&mut rx).await, None);
    tx.send(Event::Reload).await.unwrap();
    assert_eq!(wait_for_login(&mut rx).await, Some(Stopped::Reload));
    tx.send(Event::Shutdown).await.unwrap();
    assert_eq!(wait_for_login(&mut rx).await, Some(Stopped::Quit));
    drop(tx);
    assert_eq!(wait_for_login(&mut rx).await, Some(Stopped::Quit));
  }
}
