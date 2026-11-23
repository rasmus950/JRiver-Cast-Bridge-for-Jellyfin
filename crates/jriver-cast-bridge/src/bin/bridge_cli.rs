//! `bridge-cli`: log in to Jellyfin, check the cast registration, and
//! drive JRiver through MCWS by hand, for manual checks.
//!
//! Uses the same config, path mapper, Jellyfin client and player as the
//! bridge.

use std::{
  error::Error,
  io::{BufRead, Write},
  path::PathBuf,
  process::ExitCode,
  sync::Arc,
  time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use jriver_cast_bridge::socket::{JellyfinWebSocket, JellyfinWebSocketEvent};
use jriver_cast_bridge::{
  config::{self, Config, ConfigSource},
  credentials::{self, CredentialStore, StoredLogin},
  instance, ipc,
  jellyfin::{Identity, JellyfinClient, DEFAULT_TIMEOUT},
  path_map::{check_exists, MediaShape},
  resolve::select_target,
  service,
  status::BridgeStatus,
  tracks::{self, TrackKind as JellyfinTrack},
  units::{parse_position, Millis},
};
use jriver_mcws::{JRiverPlayer, McwsClient, PlayState, PlaybackInfo, PlayerOptions};
use log::{info, warn};
use tracing_subscriber::{fmt::time::ChronoLocal, EnvFilter};

/// Password for `login` without a prompt (for scripted tests). Never logged.
const PASSWORD_ENV: &str = "JRIVER_BRIDGE_PASSWORD";

/// How long the share may take to answer an existence check.
const EXISTS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a pause/resume/stop may take to show in MC's state.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `quit` waits for the running bridge to stop.
const QUIT_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Parser)]
#[command(
  name = "bridge-cli",
  version,
  about = "Drive JRiver Media Center through MCWS (jriver-cast-bridge)"
)]
struct Cli {
  /// Config file (default: %APPDATA%\jriver-cast-bridge\config.toml).
  #[arg(long, global = true)]
  config: Option<PathBuf>,
  /// Log every MCWS call.
  #[arg(short, long, global = true)]
  verbose: bool,
  #[command(subcommand)]
  command: Command,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum TrackKind {
  Audio,
  Subtitles,
}

#[derive(Debug, Subcommand)]
enum Command {
  /// Check that MC answers and show its version.
  Alive,
  /// Show the playback state of the configured zone.
  Info,
  /// Replace Playing Now with one file and play it (the bridge's start sequence).
  Play {
    /// HTPC path (UNC), or a Jellyfin server path with --server-path.
    path: String,
    /// Start position: milliseconds, m:ss or h:mm:ss.
    #[arg(long, default_value = "0")]
    start: String,
    /// Treat PATH as a Jellyfin server path and map it through path_map.
    #[arg(long)]
    server_path: bool,
    /// PATH is a Blu-ray folder (Jellyfin VideoType BluRay).
    #[arg(long)]
    bluray: bool,
  },
  /// Pause.
  Pause,
  /// Unpause.
  Resume,
  /// Toggle pause.
  Toggle,
  /// Seek to an absolute position (milliseconds, m:ss or h:mm:ss).
  Seek { position: String },
  /// Stop playback.
  Stop,
  /// Pick a track of the playing file (MCC_SET_AUDIO_STREAM or
  /// MCC_SET_SUBTITLES), to measure how MC numbers its tracks: watch the
  /// screen and the receiver. -1 is the next track, -2 the previous.
  Track {
    #[arg(value_enum)]
    kind: TrackKind,
    #[arg(allow_hyphen_values = true)]
    index: i32,
  },
  /// Show how a Jellyfin server path maps to an HTPC path, and whether it exists.
  Map {
    server_path: String,
    /// The path is a Blu-ray folder (Jellyfin VideoType BluRay).
    #[arg(long)]
    bluray: bool,
  },
  /// Run the bridge in this console until Ctrl+C: the cast receiver without
  /// the tray icon, for testing.
  Run,
  /// Ask the running bridge to quit (as the tray's Quit) and wait for it.
  Quit,
  /// Ask the running bridge to reload its config, as saving in the
  /// Settings window does.
  Reload,
  /// Log in to Jellyfin and save the login in Windows Credential Manager.
  /// The password is prompted for (or read from JRIVER_BRIDGE_PASSWORD).
  Login {
    /// Jellyfin user name (prompted for if left out).
    #[arg(long)]
    user: Option<String>,
  },
  /// Revoke the saved Jellyfin login on the server and delete it here.
  Logout,
  /// Show the saved login and this device's session on the server.
  Whoami,
  /// Register as a cast target and print the commands Jellyfin sends,
  /// without driving JRiver (the registration smoke test).
  Listen {
    /// How long to listen, in seconds.
    #[arg(long, default_value_t = 120)]
    seconds: u64,
  },
  /// Look up a Jellyfin item and show the file JRiver would open.
  Item {
    item_id: String,
    /// The media source (version) to use; default: the first.
    #[arg(long)]
    media_source: Option<String>,
  },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
  let cli = Cli::parse();
  init_logging(cli.verbose);
  match run(cli).await {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      eprintln!("error: {err}");
      let mut source = err.source();
      while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
      }
      ExitCode::FAILURE
    }
  }
}

fn init_logging(verbose: bool) {
  let filter = if verbose {
    "info,jriver_mcws=debug,jriver_cast_bridge=debug,bridge_cli=debug"
  } else {
    "warn,jriver_mcws=info,jriver_cast_bridge=info,bridge_cli=info"
  };
  tracing_subscriber::fmt()
    .with_timer(ChronoLocal::rfc_3339())
    .with_env_filter(EnvFilter::new(filter))
    .with_writer(std::io::stderr)
    .with_target(verbose)
    .init();
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
  let (config, source) = config::load(cli.config.as_deref())?;
  match &source {
    ConfigSource::File(path) => info!("config: {}", path.display()),
    ConfigSource::Defaults(path) => info!(
      "config: {} not found, using defaults (no path_map)",
      path.display()
    ),
  }

  match &cli.command {
    Command::Map {
      server_path,
      bluray,
    } => {
      let mapped = config.path_mapper()?.map(server_path, shape(*bluray))?;
      println!("{server_path}\n  -> {mapped}");
      check_exists(&mapped, EXISTS_TIMEOUT).await?;
      println!("  exists");
      return Ok(());
    }
    Command::Run => return run_service(config, source).await,
    Command::Quit => return quit().await,
    Command::Reload => {
      match ipc::send(&ipc::Request::Reload, None).await? {
        ipc::Reply::NotRunning => println!("no bridge is running"),
        ipc::Reply::Done => println!("the bridge is reloading its settings"),
      }
      return Ok(());
    }
    Command::Login { user } => return login(&config, user.clone()).await,
    Command::Logout => return logout(&config).await,
    Command::Whoami => return whoami(&config).await,
    Command::Listen { seconds } => return listen(&config, Duration::from_secs(*seconds)).await,
    Command::Item {
      item_id,
      media_source,
    } => return item(&config, item_id, media_source.as_deref()).await,
    _ => {}
  }

  let client = McwsClient::new(config.mcws_config(credentials::mcws_password(&config.mcws)))?;
  let player = JRiverPlayer::new(client, PlayerOptions::default());
  let mcws = player.mcws();

  match cli.command {
    Command::Alive => {
      let alive = mcws.alive().await?;
      println!(
        "{} {} on {} ({})",
        alive.program_name, alive.program_version, alive.friendly_name, alive.platform
      );
    }
    Command::Info => print_status(&player.status().await?, mcws).await,
    Command::Play {
      path,
      start,
      server_path,
      bluray,
    } => {
      let start = parse_position(&start)?;
      let target = if server_path {
        let mapped = config.path_mapper()?.map(&path, shape(bluray))?;
        println!("mapped {path}\n    -> {mapped}");
        mapped
      } else {
        if !path.starts_with(r"\\") {
          warn!("{path} is not a UNC path; MC will list it separately from the UNC form");
        }
        path
      };
      check_exists(&target, EXISTS_TIMEOUT).await?;
      let started = player.play(&target, start.0).await?;
      println!(
        "playing {}\n  at {} of {} (key {})",
        started.filename.as_deref().unwrap_or(&target),
        Millis(started.position_ms),
        Millis(started.duration_ms),
        started.file_key
      );
      println!(
        "  start position {} ({}), display view: {}",
        if started.position_confirmed {
          "confirmed"
        } else {
          "NOT confirmed"
        },
        start,
        match started.display_view {
          Some(true) => "yes",
          Some(false) => "NO",
          None => "unknown",
        }
      );
    }
    Command::Pause => {
      if player.status().await?.state == PlayState::Stopped {
        return Err("nothing is playing, so there is nothing to pause".into());
      }
      player.pause().await?;
      settle(&player, "paused", |s| s == PlayState::Paused).await?;
    }
    Command::Resume => {
      if player.status().await?.state == PlayState::Stopped {
        return Err("nothing is playing, so there is nothing to resume".into());
      }
      player.resume().await?;
      settle(&player, "playing", |s| s == PlayState::Playing).await?;
    }
    Command::Toggle => {
      let before = player.status().await?.state;
      if before == PlayState::Stopped {
        return Err("nothing is playing, so there is nothing to toggle".into());
      }
      player.toggle_pause().await?;
      settle(&player, "toggled", |s| s != before).await?;
    }
    Command::Seek { position } => {
      let target = parse_position(&position)?;
      let landed = player.seek(target.0).await?;
      println!("seek to {target}: MC reports {}", Millis(landed));
    }
    Command::Stop => {
      player.stop().await?;
      settle(&player, "stopped", |s| s == PlayState::Stopped).await?;
    }
    Command::Track { kind, index } => {
      match kind {
        TrackKind::Audio => mcws.select_audio_stream(index).await?,
        TrackKind::Subtitles => mcws.select_subtitles(index).await?,
      }
      println!("sent {kind:?} track {index}");
    }
    Command::Map { .. }
    | Command::Login { .. }
    | Command::Logout
    | Command::Whoami
    | Command::Listen { .. }
    | Command::Item { .. }
    | Command::Run
    | Command::Quit
    | Command::Reload => unreachable!("handled above"),
  }
  Ok(())
}

/// MC applies commands asynchronously. Poll until the state matches, then
/// print it with the time MC took (the "within about 1 s" acceptance check).
async fn settle(
  player: &JRiverPlayer<McwsClient>,
  what: &str,
  done: impl Fn(PlayState) -> bool,
) -> Result<(), Box<dyn Error>> {
  let started = Instant::now();
  loop {
    let status = player.status().await?;
    if done(status.state) {
      println!("{what} after {} ms", started.elapsed().as_millis());
      print_status(&status, player.mcws()).await;
      return Ok(());
    }
    if started.elapsed() >= SETTLE_TIMEOUT {
      print_status(&status, player.mcws()).await;
      return Err(format!("MC was not {what} within {SETTLE_TIMEOUT:?}").into());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

fn shape(bluray: bool) -> MediaShape {
  if bluray {
    MediaShape::BluRayFolder
  } else {
    MediaShape::File
  }
}

/// `bridge-cli run`: the bridge service in this console (Ctrl+C stops it,
/// with the same Quit behaviour as the tray).
async fn run_service(config: Config, source: ConfigSource) -> Result<(), Box<dyn Error>> {
  let _lock = instance::acquire(&config::app_dir()?)?;
  let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
  let (status_tx, _status_rx) = tokio::sync::watch::channel(BridgeStatus::default());
  // The file to load again when the settings are saved.
  let path = match &source {
    ConfigSource::File(path) | ConfigSource::Defaults(path) => path.clone(),
  };
  service::run(
    Some(path),
    Ok((config, source)),
    events_tx,
    events_rx,
    status_tx,
    // A test tool: never starts Jellyfin Media Player.
    false,
  )
  .await
  .map_err(|err| -> Box<dyn Error> { err })
}
/// The saved login and a client for it.
fn logged_in_client(
  config: &Config,
) -> Result<(JellyfinClient, StoredLogin, CredentialStore), Box<dyn Error>> {
  let jellyfin = config.jellyfin()?;
  let store = CredentialStore::new(&jellyfin.url, &jellyfin.profile);
  let stored = store.load()?.ok_or_else(|| {
    format!(
      "no saved Jellyfin login ({}); run `bridge-cli login` first",
      store.account()
    )
  })?;
  let client = JellyfinClient::new(
    &jellyfin.url,
    Identity {
      device_name: jellyfin.device_name.clone(),
      device_id: stored.device_id.clone(),
    },
    Some(stored.login()),
    DEFAULT_TIMEOUT,
  )?;
  Ok((client, stored, store))
}

async fn login(config: &Config, user: Option<String>) -> Result<(), Box<dyn Error>> {
  let jellyfin = config.jellyfin()?;
  let user = match user {
    Some(user) => user,
    None => {
      print!("Jellyfin user: ");
      std::io::stdout().flush()?;
      let mut line = String::new();
      std::io::stdin().lock().read_line(&mut line)?;
      line.trim().to_owned()
    }
  };
  let password = match std::env::var(PASSWORD_ENV) {
    Ok(password) if !password.is_empty() => password,
    _ => rpassword::prompt_password(format!("Password for {user}: "))?,
  };
  let result = credentials::log_in(jellyfin, &user, &password).await;
  drop(password);
  let (stored, store) = result?;
  println!(
    "logged in to {} as {}\n  cast target: \"{}\" (device ID {})\n  saved in Windows Credential Manager as {}",
    jellyfin.url.trim_end_matches('/'),
    stored.user_name,
    jellyfin.device_name,
    stored.device_id,
    store.account()
  );
  let notice = ipc::Request::Login {
    account: store.account().to_owned(),
  };
  match ipc::send(&notice, None).await {
    Ok(ipc::Reply::Done) => println!("  told the running bridge about the new login"),
    Ok(ipc::Reply::NotRunning) => {}
    Err(err) => warn!("couldn't tell the running bridge about the new login ({err}); restart it"),
  }
  Ok(())
}

/// `bridge-cli quit`: what the installer does with `jriver-cast-bridge --quit`.
async fn quit() -> Result<(), Box<dyn Error>> {
  match ipc::quit_running_bridge(&config::app_dir()?, QUIT_TIMEOUT).await? {
    ipc::Reply::NotRunning => println!("no bridge is running"),
    ipc::Reply::Done => println!("the bridge has stopped"),
  }
  Ok(())
}

async fn logout(config: &Config) -> Result<(), Box<dyn Error>> {
  let (client, stored, store) = logged_in_client(config)?;
  match client.logout().await {
    Ok(()) => println!("revoked the login of {} on the server", stored.user_name),
    Err(err) => warn!("couldn't revoke the login on the server: {err}"),
  }
  store.delete()?;
  println!("deleted the saved login {}", store.account());
  Ok(())
}

async fn whoami(config: &Config) -> Result<(), Box<dyn Error>> {
  let (client, stored, store) = logged_in_client(config)?;
  println!("saved login: {} ({})", stored.user_name, store.account());
  println!("device ID:   {}", stored.device_id);
  let sessions = client.own_sessions().await?;
  if sessions.is_empty() {
    println!("session:     none (the bridge isn't connected)");
  }
  for session in sessions {
    println!(
      "session:     {} \"{}\" for {} via {}",
      session.id.as_deref().unwrap_or("?"),
      session.device_name.as_deref().unwrap_or("?"),
      session.user_name.as_deref().unwrap_or("?"),
      session.client.as_deref().unwrap_or("?"),
    );
    println!(
      "  cast target: remote control {}, media control {}, plays {:?}, commands {:?}",
      session.supports_remote_control.unwrap_or(false),
      session.supports_media_control.unwrap_or(false),
      session.playable_media_types.unwrap_or_default(),
      session.supported_commands.unwrap_or_default(),
    );
    if let Some(item) = &session.now_playing_item {
      let state = session.play_state.clone().unwrap_or_default();
      let streams = item.media_streams.as_deref();
      let count = |kind: &str| streams.map(|s| s.iter().filter(|m| m.kind == kind).count());
      println!(
        "  playing:     {} \"{}\"; source {:?}; tracks listed: {} (audio {:?}, subtitles {:?}); in use: audio {:?}, subtitles {:?}",
        item.id,
        item.name.as_deref().unwrap_or("?"),
        state.media_source_id,
        streams.map_or_else(|| "none (no MediaStreams)".to_owned(), |s| s.len().to_string()),
        count("Audio"),
        count("Subtitle"),
        state.audio_stream_index,
        state.subtitle_stream_index,
      );
    }
  }
  Ok(())
}

async fn listen(config: &Config, duration: Duration) -> Result<(), Box<dyn Error>> {
  let (client, stored, _) = logged_in_client(config)?;
  let client = Arc::new(client);
  let socket = JellyfinWebSocket::without_session_updates();
  let mut events = socket
    .take_event_receiver()
    .ok_or("the socket's event receiver was already taken")?;
  socket
    .connect_with_user_agent(
      &client.websocket_url()?,
      Some(&JellyfinClient::user_agent()),
    )
    .await
    .map_err(|err| err.to_string().replace(&stored.access_token, "<redacted>"))?;
  client.register().await?;
  println!(
    "registered as \"{}\"; listening for {} s (this does not drive JRiver)",
    client.identity().device_name,
    duration.as_secs()
  );
  for session in client.own_sessions().await? {
    println!(
      "  session {}: remote control {}, media control {}, plays {:?}",
      session.id.as_deref().unwrap_or("?"),
      session.supports_remote_control.unwrap_or(false),
      session.supports_media_control.unwrap_or(false),
      session.playable_media_types.unwrap_or_default()
    );
  }
  let deadline = tokio::time::Instant::now() + duration;
  loop {
    match tokio::time::timeout_at(deadline, events.recv()).await {
      Err(_) | Ok(None) => break,
      Ok(Some(JellyfinWebSocketEvent::Command(command))) => println!("received: {command:?}"),
      Ok(Some(JellyfinWebSocketEvent::Reconnected)) => {
        println!("socket reconnected; registering again");
        client.register().await?;
      }
      Ok(Some(event)) => println!("socket: {event:?}"),
    }
  }
  socket.disconnect().await;
  println!("done");
  Ok(())
}

async fn item(
  config: &Config,
  item_id: &str,
  media_source: Option<&str>,
) -> Result<(), Box<dyn Error>> {
  let (client, _, _) = logged_in_client(config)?;
  let item = client.item(item_id).await?;
  println!(
    "{} ({}, {}, video type {})",
    item.name.as_deref().unwrap_or("?"),
    item.item_type.as_deref().unwrap_or("?"),
    item.media_type.as_deref().unwrap_or("?"),
    item.video_type.as_deref().unwrap_or("?")
  );
  let target = select_target(&item, media_source)?;
  let mapped = config
    .path_mapper()?
    .map(&target.server_path, target.shape)?;
  println!(
    "  media source {} ({})\n  {}\n  -> {mapped}",
    target.media_source_id, target.shape, target.server_path
  );
  check_exists(&mapped, EXISTS_TIMEOUT).await?;
  println!("  exists");
  // The tracks, with the number `bridge-cli track` would send for each
  // (to compare Jellyfin's numbering with JRiver's).
  let streams = item
    .media_sources
    .as_deref()
    .unwrap_or_default()
    .iter()
    .find(|source| source.id == target.media_source_id)
    .map(|source| source.media_streams.as_slice())
    .unwrap_or_default();
  for (kind, label, jellyfin_type) in [
    (JellyfinTrack::Audio, "audio", "Audio"),
    (JellyfinTrack::Subtitle, "subtitles", "Subtitle"),
  ] {
    for stream in streams.iter().filter(|stream| stream.kind == jellyfin_type) {
      let jriver = tracks::jriver_track(streams, kind, stream.index)
        .map_or_else(|| "-".to_owned(), |n| n.to_string());
      println!(
        "  {label} jellyfin {:>2}  jriver {jriver:>2}  {}{}{}",
        stream.index,
        stream
          .display_title
          .as_deref()
          .or(stream.language.as_deref())
          .unwrap_or("?"),
        if stream.is_default { "  (default)" } else { "" },
        if stream.is_external {
          "  (external file)"
        } else {
          ""
        },
      );
    }
  }
  Ok(())
}

async fn print_status(status: &PlaybackInfo, mcws: &McwsClient) {
  println!("state:       {:?}", status.state);
  println!(
    "position:    {} / {}",
    Millis(status.position_ms),
    Millis(status.duration_ms)
  );
  println!("file key:    {}", status.file_key);
  println!("file:        {}", status.filename.as_deref().unwrap_or("-"));
  println!("playing now: {} track(s)", status.playing_now_tracks);
  match mcws.ui_info().await {
    Ok(ui) => println!(
      "ui mode:     {}{}",
      ui.mode,
      if ui.is_display_view() {
        " (Display view)"
      } else {
        ""
      }
    ),
    Err(err) => println!("ui mode:     unknown ({err})"),
  }
}
