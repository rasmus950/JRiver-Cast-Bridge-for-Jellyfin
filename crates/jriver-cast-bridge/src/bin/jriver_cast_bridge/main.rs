//! `jriver-cast-bridge`: makes JRiver Media Center a Jellyfin cast target.
//!
//! Runs at login with a tray icon (Status, Log in… / Log out, Settings…,
//! Open log, About, Quit) and, in release builds, no console window. More
//! modes share the exe:
//! - `--login`: the login window (the tray opens it; see [`login`]).
//! - `--settings`: the settings window (see [`settings_window`]).
//! - `--about`: the About window (see [`about_window`]).
//! - `--quit`: ask the running bridge to quit and wait for it (installer).
//! - `--notice <kind>`: the "Starting JRiver…" notice (the tray opens and
//!   closes it; see [`notice`]).
//!
//! To run the bridge in a console instead (testing), use `bridge-cli run`.

// Release builds have no console window; debug builds keep one for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about_window;
mod login;
mod notice;
mod settings_window;

use std::{
  error::Error,
  ffi::OsString,
  path::{Path, PathBuf},
  process::ExitCode,
  time::{Duration, Instant},
};

use clap::Parser;
use jriver_cast_bridge::{
  config::{self, Config, ConfigError, ConfigSource},
  controller::Event,
  instance, ipc, logging,
  service::{self, BoxError},
  status::{icon_rgba, Account, BridgeStatus, JRiverNotice, ICON_SIZE},
};
use log::{error, info, warn};
use tokio::sync::{mpsc, watch};
use tray_icon::{
  menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
  Icon, TrayIcon, TrayIconBuilder,
};
use winit::{
  application::ApplicationHandler,
  event::{StartCause, WindowEvent},
  event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
  window::WindowId,
};

/// After Quit, how long the tray waits for the bridge before exiting anyway
/// (stopping JRiver, minimizing it and flushing reports take a few seconds).
const QUIT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `--quit` waits for the running bridge to exit.
const REMOTE_QUIT_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Parser)]
#[command(
  name = "jriver-cast-bridge",
  version,
  about = "Make JRiver Media Center a Jellyfin cast target"
)]
struct Args {
  /// Config file (default: %APPDATA%\jriver-cast-bridge\config.toml).
  #[arg(long)]
  config: Option<PathBuf>,
  /// Log every MCWS and Jellyfin call (overrides log.level).
  #[arg(short, long)]
  verbose: bool,
  /// Show the Jellyfin login window instead of running the bridge.
  #[arg(long, conflicts_with = "quit")]
  login: bool,
  /// Show the settings window instead of running the bridge.
  #[arg(long, conflicts_with_all = ["quit", "login"])]
  settings: bool,
  /// Show the About window instead of running the bridge.
  #[arg(long, conflicts_with_all = ["quit", "login", "settings"])]
  about: bool,
  /// Ask the running bridge to quit, wait until it has, and exit: 0 when no
  /// bridge runs any more, 1 if it didn't stop in time.
  #[arg(long)]
  quit: bool,
  /// Show the notice that JRiver is starting (`starting` or `restarting`).
  #[arg(long, value_name = "KIND", hide = true, conflicts_with_all = ["quit", "login", "settings", "about"])]
  notice: Option<String>,
}

fn main() -> ExitCode {
  let args = Args::parse();
  let loaded = config::load(args.config.as_deref());
  let (level, keep_days) = match &loaded {
    Ok((config, _)) => (config.log_level(), config.log.keep_days),
    Err(_) => (log::LevelFilter::Info, 14),
  };
  let level = if args.verbose {
    log::LevelFilter::Debug
  } else {
    level
  };
  let log_dir = config::log_dir().ok();
  let _log_guard = logging::init(level, log_dir.as_deref(), keep_days);

  let config_path = match &loaded {
    Ok((_, ConfigSource::File(path) | ConfigSource::Defaults(path))) => Some(path.clone()),
    Err(_) => args.config.clone().or_else(|| config::default_path().ok()),
  };
  if args.quit {
    return quit_running_bridge();
  }
  if args.login {
    return login::run(loaded);
  }
  if args.settings {
    return settings_window::run(config_path);
  }
  if args.about {
    return about_window::run(config_path, log_dir);
  }
  if let Some(kind) = &args.notice {
    return notice::run(kind);
  }

  info!("jriver-cast-bridge {} starting", env!("CARGO_PKG_VERSION"));
  let _lock = match config::app_dir()
    .map_err(BoxError::from)
    .and_then(|dir| instance::acquire(&dir).map_err(BoxError::from))
  {
    Ok(lock) => lock,
    Err(err) => {
      error!("{err}");
      return ExitCode::FAILURE;
    }
  };

  // The windows (login, settings, About) get the same config.
  let mut window_args = Vec::new();
  if let Some(path) = &args.config {
    window_args.push(OsString::from("--config"));
    window_args.push(path.into());
  }
  run_with_tray(loaded, args.config, log_dir, window_args)
}

/// `--quit`: what the installer runs before replacing or removing the exe.
fn quit_running_bridge() -> ExitCode {
  let outcome = runtime().map_err(BoxError::from).and_then(|rt| {
    rt.block_on(async {
      let dir = config::app_dir()?;
      Ok::<_, BoxError>(ipc::quit_running_bridge(&dir, REMOTE_QUIT_TIMEOUT).await?)
    })
  });
  match outcome {
    Ok(ipc::Reply::NotRunning) => {
      info!("--quit: no bridge is running");
      ExitCode::SUCCESS
    }
    Ok(ipc::Reply::Done) => {
      info!("--quit: the bridge has stopped");
      ExitCode::SUCCESS
    }
    Err(err) => {
      error!("--quit: {err}");
      ExitCode::FAILURE
    }
  }
}

fn log_error_chain(err: &(dyn Error + 'static)) {
  error!("{err}");
  let mut source = err.source();
  while let Some(cause) = source {
    error!("  caused by: {cause}");
    source = cause.source();
  }
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
  tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
}

#[derive(Debug)]
enum UserEvent {
  Status(BridgeStatus),
  Menu(MenuEvent),
  BridgeStopped(Option<String>),
}

fn run_with_tray(
  loaded: Result<(Config, ConfigSource), ConfigError>,
  config_arg: Option<PathBuf>,
  log_dir: Option<PathBuf>,
  window_args: Vec<OsString>,
) -> ExitCode {
  let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
    Ok(event_loop) => event_loop,
    Err(err) => {
      error!("can't start the tray event loop ({err}); use `bridge-cli run` instead");
      return ExitCode::FAILURE;
    }
  };
  let proxy = event_loop.create_proxy();
  {
    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
      let _ = proxy.send_event(UserEvent::Menu(event));
    }));
  }

  let (events_tx, events_rx) = mpsc::channel(64);
  let bridge_thread = {
    let events_tx = events_tx.clone();
    let proxy = proxy.clone();
    std::thread::Builder::new()
      .name("bridge".to_owned())
      .spawn(move || run_bridge_thread(loaded, config_arg, events_tx, events_rx, &proxy))
  };
  if let Err(err) = bridge_thread {
    error!("can't start the bridge thread: {err}");
    return ExitCode::FAILURE;
  }

  let mut app = TrayApp {
    tray: None,
    status_item: MenuItem::new("Status: starting", false, None),
    account: MenuItem::new("Log in…", false, None),
    settings: MenuItem::new("Settings…", true, None),
    open_log: MenuItem::new("Open log", true, None),
    about: MenuItem::new("About", true, None),
    quit: MenuItem::new("Quit", true, None),
    events: events_tx,
    log_dir,
    window_args,
    login_offered: false,
    notice: None,
    bridge_done: false,
    quit_deadline: None,
    status: BridgeStatus::default(),
  };
  if let Err(err) = event_loop.run_app(&mut app) {
    error!("tray event loop failed: {err}");
    return ExitCode::FAILURE;
  }
  info!("stopped");
  ExitCode::SUCCESS
}

fn run_bridge_thread(
  loaded: Result<(Config, ConfigSource), ConfigError>,
  config_arg: Option<PathBuf>,
  events_tx: mpsc::Sender<Event>,
  events_rx: mpsc::Receiver<Event>,
  proxy: &EventLoopProxy<UserEvent>,
) {
  let outcome = match runtime() {
    Ok(rt) => rt.block_on(async {
      let (status_tx, mut status_rx) = watch::channel(BridgeStatus::default());
      let forward = {
        let proxy = proxy.clone();
        tokio::spawn(async move {
          loop {
            let status = status_rx.borrow_and_update().clone();
            if proxy.send_event(UserEvent::Status(status)).is_err() {
              break;
            }
            if status_rx.changed().await.is_err() {
              break;
            }
          }
        })
      };
      // Saved settings are loaded again from `config_arg` inside.
      // The tray starts Jellyfin Media Player once the bridge is registered.
      let result = service::run(config_arg, loaded, events_tx, events_rx, status_tx, true).await;
      forward.abort();
      result
    }),
    Err(err) => Err(BoxError::from(format!(
      "can't start the async runtime: {err}"
    ))),
  };
  let error = outcome.err().map(|err| {
    log_error_chain(err.as_ref());
    err.to_string()
  });
  let _ = proxy.send_event(UserEvent::BridgeStopped(error));
}

struct TrayApp {
  tray: Option<TrayIcon>,
  status_item: MenuItem,
  /// "Log in…" or "Log out", following the status's account.
  account: MenuItem,
  settings: MenuItem,
  open_log: MenuItem,
  about: MenuItem,
  quit: MenuItem,
  events: mpsc::Sender<Event>,
  log_dir: Option<PathBuf>,
  /// `--config <path>` for the window processes, when one was given.
  window_args: Vec<OsString>,
  /// The login window opens by itself once, when the bridge starts without
  /// a usable login.
  login_offered: bool,
  /// The notice window while the bridge starts JRiver, and which one.
  notice: Option<(JRiverNotice, std::process::Child)>,
  bridge_done: bool,
  quit_deadline: Option<Instant>,
  status: BridgeStatus,
}

impl TrayApp {
  fn create_tray(&mut self) {
    let menu = Menu::new();
    let separator = PredefinedMenuItem::separator();
    let separator2 = PredefinedMenuItem::separator();
    if let Err(err) = menu.append_items(&[
      &self.status_item,
      &separator,
      &self.account,
      &self.settings,
      &self.open_log,
      &separator2,
      &self.about,
      &self.quit,
    ]) {
      error!("can't build the tray menu: {err}");
      return;
    }
    let icon = match Icon::from_rgba(icon_rgba(), ICON_SIZE, ICON_SIZE) {
      Ok(icon) => icon,
      Err(err) => {
        error!("can't build the tray icon image: {err}");
        return;
      }
    };
    match TrayIconBuilder::new()
      .with_menu(Box::new(menu))
      .with_tooltip(self.status.tooltip())
      .with_icon(icon)
      .build()
    {
      Ok(tray) => self.tray = Some(tray),
      Err(err) => error!("can't show the tray icon: {err}"),
    }
  }

  fn show_status(&mut self, status: BridgeStatus) {
    self
      .status_item
      .set_text(format!("Status: {}", status.summary()));
    if let Some(tray) = &self.tray {
      if let Err(err) = tray.set_tooltip(Some(status.tooltip())) {
        warn!("can't update the tray tooltip: {err}");
      }
    }
    let (text, enabled) = match &status.account {
      _ if self.bridge_done || self.quit_deadline.is_some() => ("Log in…", false),
      Account::Unknown => ("Log in…", false),
      Account::LoggedIn { .. } => ("Log out", true),
      Account::LoggedOut | Account::Rejected { .. } => ("Log in…", true),
    };
    self.account.set_text(text);
    self.account.set_enabled(enabled);
    self.show_notice(status.jriver);
    let needs_login = matches!(
      status.account,
      Account::LoggedOut | Account::Rejected { .. }
    );
    if !self.login_offered && status.account != Account::Unknown {
      self.login_offered = true;
      if needs_login {
        info!("no usable Jellyfin login; opening the login window");
        self.open_login();
      }
    }
    self.status = status;
  }

  fn on_menu(&mut self, event_loop: &ActiveEventLoop, event: &MenuEvent) {
    if event.id == *self.quit.id() {
      self.request_quit(event_loop);
    } else if event.id == *self.account.id() {
      self.on_account();
    } else if event.id == *self.open_log.id() {
      self.open_log();
    } else if event.id == *self.settings.id() {
      self.open_window("--settings", "the settings window");
    } else if event.id == *self.about.id() {
      self.open_window("--about", "the About window");
    }
  }

  fn on_account(&mut self) {
    if let Account::LoggedIn { user } = &self.status.account {
      info!("Log out of Jellyfin ({user}) from the tray");
      if self.events.try_send(Event::Logout).is_err() {
        warn!("couldn't ask the bridge to log out");
      }
    } else {
      self.open_login();
    }
  }

  /// Open, swap or close the notice window to match `wanted`.
  fn show_notice(&mut self, wanted: Option<JRiverNotice>) {
    if self.notice.as_ref().map(|(kind, _)| *kind) == wanted {
      return;
    }
    self.close_notice();
    let Some(kind) = wanted else {
      return;
    };
    let exe = match std::env::current_exe() {
      Ok(exe) => exe,
      Err(err) => {
        error!("can't find this program to show the notice: {err}");
        return;
      }
    };
    match std::process::Command::new(&exe)
      .args(["--notice", kind.arg()])
      .spawn()
    {
      Ok(child) => self.notice = Some((kind, child)),
      Err(err) => error!("can't show the notice: {err}"),
    }
  }

  fn close_notice(&mut self) {
    if let Some((_, mut child)) = self.notice.take() {
      // Ok also when it already closed itself.
      if let Err(err) = child.kill() {
        warn!("can't close the notice: {err}");
      }
      // Reap it off the event loop, which must never wait on it.
      if let Err(err) = std::thread::Builder::new()
        .name("notice-reaper".to_owned())
        .spawn(move || child.wait())
      {
        warn!("can't wait for the notice to close: {err}");
      }
    }
  }

  /// Start the login window (this exe with `--login`). It saves the login
  /// and tells the bridge itself.
  fn open_login(&self) {
    self.open_window("--login", "the login window");
  }

  /// Start one of the windows (this exe with `mode`, e.g. `--settings`),
  /// with the same config. Each keeps itself to one window at a time.
  fn open_window(&self, mode: &str, what: &str) {
    let exe = match std::env::current_exe() {
      Ok(exe) => exe,
      Err(err) => {
        error!("can't find this program to open {what}: {err}");
        return;
      }
    };
    if let Err(err) = std::process::Command::new(&exe)
      .arg(mode)
      .args(&self.window_args)
      .spawn()
    {
      error!("can't open {what}: {err}");
    }
  }

  fn request_quit(&mut self, event_loop: &ActiveEventLoop) {
    if self.bridge_done {
      event_loop.exit();
      return;
    }
    info!("Quit from the tray");
    self.quit.set_enabled(false);
    self.account.set_enabled(false);
    self.status_item.set_text("Status: stopping");
    if self.events.try_send(Event::Shutdown).is_err() {
      warn!("couldn't ask the bridge to stop; exiting");
      event_loop.exit();
      return;
    }
    let deadline = Instant::now() + QUIT_TIMEOUT;
    self.quit_deadline = Some(deadline);
    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
  }

  fn open_log(&self) {
    let Some(dir) = &self.log_dir else {
      warn!("no log folder (%APPDATA% isn't set)");
      return;
    };
    match logging::newest_log(dir) {
      Some(file) => open_with("notepad.exe", &file),
      None => open_with("explorer.exe", dir),
    }
  }
}

impl Drop for TrayApp {
  fn drop(&mut self) {
    self.close_notice();
  }
}

impl ApplicationHandler<UserEvent> for TrayApp {
  fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
    // The tray icon must be created once the event loop runs.
    if cause == StartCause::Init {
      self.create_tray();
    }
    if let Some(deadline) = self.quit_deadline {
      if Instant::now() >= deadline {
        warn!("the bridge didn't stop within {QUIT_TIMEOUT:?}; exiting anyway");
        event_loop.exit();
      }
    }
  }

  fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

  fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

  fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
    match event {
      UserEvent::Status(status) => self.show_status(status),
      UserEvent::Menu(event) => self.on_menu(event_loop, &event),
      // A clean stop is always a quit: from the menu, or `--quit`.
      UserEvent::BridgeStopped(None) => {
        self.bridge_done = true;
        event_loop.exit();
      }
      UserEvent::BridgeStopped(Some(error)) => {
        self.bridge_done = true;
        if self.quit_deadline.is_some() {
          event_loop.exit();
          return;
        }
        // Stopped on its own (only if its runtime couldn't start; a bad
        // config waits instead): keep the icon so the reason can be seen.
        let status = BridgeStatus {
          error: Some(error),
          ..self.status.clone()
        };
        self.show_status(status);
      }
    }
  }
}

fn open_with(program: &str, path: &Path) {
  if let Err(err) = std::process::Command::new(program).arg(path).spawn() {
    error!("can't open {} with {program}: {err}", path.display());
  }
}
