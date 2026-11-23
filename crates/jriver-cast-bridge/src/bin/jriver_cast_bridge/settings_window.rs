//! The settings window: `jriver-cast-bridge --settings` (SPEC.md "Settings").
//!
//! Every setting of config.toml in one window. Saving writes the file in
//! place (its comments stay), but only what the bridge would accept, and
//! tells the running bridge to reload it; a playing film plays on. It runs
//! as its own process, as the login window does. One window at a time.

use std::{
  path::{Path, PathBuf},
  process::ExitCode,
};

use iced::{
  widget::{
    button, checkbox, column, container, pick_list, row, rule, scrollable, text, text_input, Space,
  },
  Alignment, Element, Length, Size, Task,
};
use jriver_cast_bridge::{
  config,
  credentials::{CredentialManager, Secrets},
  instance, ipc, player_start,
  settings::{self, Form, PathRow, LOG_LEVELS},
};
use log::{error, info, warn};

pub const SETTINGS_LOCK_FILE: &str = "settings.lock";

pub fn run(path: Option<PathBuf>) -> ExitCode {
  let _lock = match config::app_dir()
    .map_err(|err| err.to_string())
    .and_then(|dir| {
      instance::acquire_file(&dir, SETTINGS_LOCK_FILE).map_err(|err| match err {
        instance::InstanceError::AlreadyRunning(_) => String::new(),
        other => other.to_string(),
      })
    }) {
    Ok(lock) => lock,
    Err(err) if err.is_empty() => {
      info!("the settings window is already open");
      return ExitCode::SUCCESS;
    }
    Err(err) => {
      error!("can't open the settings window: {err}");
      return ExitCode::FAILURE;
    }
  };
  let Some(path) = path else {
    error!("no config location (%APPDATA% isn't set)");
    return ExitCode::FAILURE;
  };
  let result = iced::application(
    move || SettingsWindow::boot(path.clone()),
    SettingsWindow::update,
    SettingsWindow::view,
  )
  .title("JRiver Cast Bridge: settings")
  .window_size(Size::new(680.0, 760.0))
  .centered()
  .run();
  match result {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      error!("the settings window failed: {err}");
      ExitCode::FAILURE
    }
  }
}

struct SettingsWindow {
  path: PathBuf,
  form: Form,
  /// What the form was loaded from, to tell unsaved changes.
  saved: Form,
  /// The file's text the form was loaded from or last saved as (`None`: no
  /// file), to notice changes made outside the window.
  file_text: Option<String>,
  /// The file changed outside the window: offer to reload it.
  changed_outside: bool,
  /// A Media Network password is saved in Credential Manager for the user
  /// name and URL as loaded.
  password_saved: bool,
  /// Jellyfin Media Player's shortcuts in the Startup folder, read on
  /// (re)load rather than on every redraw.
  startup_shortcuts: Vec<PathBuf>,
  status: Status,
}

enum Status {
  None,
  Info(String),
  Error(String),
}

/// A text field of the form.
#[derive(Debug, Clone, Copy)]
enum Field {
  JellyfinUrl,
  DeviceName,
  Profile,
  McwsUrl,
  ZoneId,
  TimeoutMs,
  ExePath,
  PlayerPath,
  Username,
  Password,
  PollIntervalMs,
  ProgressIntervalS,
  HangAfterS,
  HangWhileStartingS,
  MaxRestartsPerHour,
  KeepDays,
}

#[derive(Debug, Clone)]
enum Message {
  Edit(Field, String),
  Minimize(bool),
  TracksFromJellyfin(bool),
  StartPlayer(bool),
  LogLevel(String),
  RuleFrom(usize, String),
  RuleTo(usize, String),
  AddRule,
  RemoveRule(usize),
  Save,
  Notified(Result<ipc::Reply, String>),
  OpenFile,
  ReloadFile,
  Close,
}

impl SettingsWindow {
  fn boot(path: PathBuf) -> (Self, Task<Message>) {
    let mut window = Self {
      path,
      form: Form::from_config(&config::Config::default()),
      saved: Form::from_config(&config::Config::default()),
      file_text: None,
      changed_outside: false,
      password_saved: false,
      startup_shortcuts: Vec::new(),
      status: Status::None,
    };
    window.load();
    (window, Task::none())
  }

  /// (Re)load the form from the file.
  fn load(&mut self) {
    self.startup_shortcuts = player_start::startup_shortcuts();
    let loaded = settings::load(&self.path);
    let mut problem = loaded.problem;
    self.password_saved = match settings::password_entry(&loaded.form) {
      None => false,
      Some(entry) => match CredentialManager.load(&entry) {
        Ok(found) => found.is_some(),
        Err(err) => {
          warn!(
            "can't read the Media Network password for {}: {err}",
            entry.account()
          );
          problem.get_or_insert(format!(
            "Can't read the Media Network password from Credential Manager: {err}"
          ));
          false
        }
      },
    };
    self.saved = loaded.form.clone();
    self.form = loaded.form;
    self.file_text = loaded.text;
    self.changed_outside = false;
    self.status = problem.map_or(Status::None, Status::Error);
  }

  fn field(&mut self, field: Field) -> &mut String {
    let form = &mut self.form;
    match field {
      Field::JellyfinUrl => &mut form.jellyfin_url,
      Field::DeviceName => &mut form.device_name,
      Field::Profile => &mut form.profile,
      Field::McwsUrl => &mut form.mcws_url,
      Field::ZoneId => &mut form.zone_id,
      Field::TimeoutMs => &mut form.timeout_ms,
      Field::ExePath => &mut form.exe_path,
      Field::PlayerPath => &mut form.player_path,
      Field::Username => &mut form.username,
      Field::Password => &mut form.password,
      Field::PollIntervalMs => &mut form.poll_interval_ms,
      Field::ProgressIntervalS => &mut form.progress_interval_s,
      Field::HangAfterS => &mut form.hang_after_s,
      Field::HangWhileStartingS => &mut form.hang_while_starting_s,
      Field::MaxRestartsPerHour => &mut form.max_restarts_per_hour,
      Field::KeepDays => &mut form.keep_days,
    }
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    // An edit after a save: the "Saved" line gives way to "Unsaved changes".
    let edits = matches!(
      message,
      Message::Edit(..)
        | Message::Minimize(_)
        | Message::TracksFromJellyfin(_)
        | Message::StartPlayer(_)
        | Message::LogLevel(_)
        | Message::RuleFrom(..)
        | Message::RuleTo(..)
        | Message::AddRule
        | Message::RemoveRule(_)
    );
    if edits && matches!(self.status, Status::Info(_)) {
      self.status = Status::None;
    }
    match message {
      Message::Edit(field, value) => *self.field(field) = value,
      Message::Minimize(on) => self.form.minimize_after_playback = on,
      Message::TracksFromJellyfin(on) => self.form.tracks_from_jellyfin = on,
      Message::StartPlayer(on) => self.form.start_player = on,
      Message::LogLevel(level) => self.form.log_level = level,
      Message::RuleFrom(i, from) => {
        if let Some(rule) = self.form.path_map.get_mut(i) {
          rule.from = from;
        }
      }
      Message::RuleTo(i, to) => {
        if let Some(rule) = self.form.path_map.get_mut(i) {
          rule.to = to;
        }
      }
      Message::AddRule => self.form.path_map.push(PathRow::default()),
      Message::RemoveRule(i) => {
        if i < self.form.path_map.len() {
          self.form.path_map.remove(i);
        }
      }
      Message::Save => return self.save(),
      Message::Notified(Ok(ipc::Reply::Done)) => {
        self.status = Status::Info("Saved. The bridge is using the new settings.".to_owned());
      }
      Message::Notified(Ok(ipc::Reply::NotRunning)) => {
        self.status =
          Status::Info("Saved. The bridge isn't running; it uses them when it starts.".to_owned());
      }
      Message::Notified(Err(err)) => {
        warn!("couldn't tell the bridge to reload the settings: {err}");
        self.status = Status::Error(format!(
          "Saved, but the bridge couldn't be told ({err}). Quit and start it to use them."
        ));
      }
      Message::OpenFile => open_with("notepad.exe", &self.path),
      Message::ReloadFile => self.load(),
      Message::Close => return iced::exit(),
    }
    Task::none()
  }

  /// Write the form into the file, if the bridge would accept it, then tell
  /// the bridge.
  fn save(&mut self) -> Task<Message> {
    let current = match std::fs::read_to_string(&self.path) {
      Ok(text) => Some(text),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
      Err(err) => {
        self.status = Status::Error(format!("Can't read {}: {err}", self.path.display()));
        return Task::none();
      }
    };
    // Changed in Notepad (or elsewhere) since the window read it: saving
    // would silently undo that.
    if current != self.file_text {
      self.changed_outside = true;
      self.status = Status::Error(
        "The config file was changed outside this window. Reload it here (your edits here are \
         lost), or close without saving."
          .to_owned(),
      );
      return Task::none();
    }
    let original = current.unwrap_or_else(|| config::EXAMPLE.to_owned());
    let text = match self.form.apply(&original, &self.path) {
      Ok(text) => text,
      Err(err) => {
        self.status = Status::Error(format!("Not saved: {err}"));
        return Task::none();
      }
    };
    // The password first: if Credential Manager fails, nothing is written.
    // An entry the new settings no longer name is deleted only after the
    // file is written, so a failed write loses nothing.
    let stored = match settings::store_password(&self.form, &self.saved, &CredentialManager) {
      Ok(stored) => stored,
      Err(err) => {
        self.status = Status::Error(format!("Not saved: the Media Network password: {err}"));
        return Task::none();
      }
    };
    if let Err(err) = settings::write_config(&self.path, &text) {
      self.status = Status::Error(format!("Can't save {}: {err}", self.path.display()));
      return Task::none();
    }
    info!("settings saved to {}", self.path.display());
    if let Err(err) = settings::drop_stale_password(stored.stale, &CredentialManager) {
      warn!("couldn't delete the old Media Network password entry: {err}");
    }
    // Saved in Credential Manager: not kept in the window.
    self.form.password.clear();
    self.password_saved = stored.saved;
    self.saved = self.form.clone();
    self.file_text = Some(text);
    self.status = Status::Info("Saved; telling the bridge…".to_owned());
    Task::perform(
      async {
        ipc::send(&ipc::Request::Reload, None)
          .await
          .map_err(|err| err.to_string())
      },
      Message::Notified,
    )
  }

  fn view(&self) -> Element<'_, Message> {
    let form = &self.form;
    let content = column![
      section("Jellyfin"),
      labelled(
        "Server URL",
        input(
          &form.jellyfin_url,
          "http://jellyfin.local:8096",
          Field::JellyfinUrl
        )
      ),
      labelled(
        "Name in the cast menu",
        input(&form.device_name, "JRiver (HTPC)", Field::DeviceName)
      ),
      labelled(
        "Login profile",
        input(&form.profile, "default", Field::Profile)
      ),
      note("A different profile keeps a separate saved login (e.g. for testing)."),
      section("JRiver Media Center"),
      labelled(
        "Media Network URL",
        input(
          &form.mcws_url,
          "http://localhost:52199/MCWS/v1/",
          Field::McwsUrl
        )
      ),
      labelled("Zone ID", input(&form.zone_id, "0", Field::ZoneId)),
      labelled(
        "Timeout (ms)",
        input(&form.timeout_ms, "5000", Field::TimeoutMs)
      ),
      labelled(
        "Media Center program",
        input(
          &form.exe_path,
          r"C:\Program Files\J River\Media Center 36\Media Center 36.exe",
          Field::ExePath
        )
      ),
      note("Started when a cast arrives and Media Center isn't running. Empty: never started."),
      labelled(
        "User name",
        input(&form.username, "only with authentication", Field::Username)
      ),
      labelled(
        "Password",
        input(
          &form.password,
          if self.password_saved {
            "saved (type to change)"
          } else {
            "only with authentication"
          },
          Field::Password
        )
        .secure(true)
      ),
      note(
        "Kept in Windows Credential Manager, not in the config file. Clearing the user name \
         removes it."
      ),
      section("Path mapping"),
      note("Server path (as Jellyfin reports it) → UNC path the HTPC opens. The first match wins."),
      self.rules(),
      button("Add rule").on_press(Message::AddRule),
      section("Playback"),
      labelled(
        "Status poll (ms)",
        input(&form.poll_interval_ms, "1000", Field::PollIntervalMs)
      ),
      labelled(
        "Progress report (s)",
        input(&form.progress_interval_s, "10", Field::ProgressIntervalS)
      ),
      checkbox(form.minimize_after_playback)
        .label("Minimize Media Center when a full-screen film ends")
        .on_toggle(Message::Minimize),
      checkbox(form.tracks_from_jellyfin)
        .label("Play the audio and subtitle tracks picked in Jellyfin")
        .on_toggle(Message::TracksFromJellyfin),
      text(
        "Off: Media Center picks the tracks (its own choice, or the one last used for the \
         file), and Jellyfin offers no track choice when casting here."
      )
      .size(13),
      section("Jellyfin Media Player"),
      checkbox(form.start_player)
        .label("Start Jellyfin Media Player when the bridge is ready")
        .on_toggle(Message::StartPlayer),
      labelled(
        "Program",
        input(
          &form.player_path,
          config::DEFAULT_PLAYER_PATH,
          Field::PlayerPath
        )
      ),
      note(
        "Its \"autocast\" picks the bridge only if the bridge is already a cast target when it \
         starts, so the bridge starts it (right after it registers with Jellyfin)."
      ),
      self.startup_warning(),
      section("Hung Media Center"),
      labelled(
        "Restart after (s)",
        input(&form.hang_after_s, "3", Field::HangAfterS)
      ),
      labelled(
        "While a cast starts (s)",
        input(&form.hang_while_starting_s, "20", Field::HangWhileStartingS)
      ),
      labelled(
        "Restarts per hour",
        input(&form.max_restarts_per_hour, "3", Field::MaxRestartsPerHour)
      ),
      note(
        "Seconds Windows must say \"Not responding\" (it says so after 5 s without an answer). \
         A file opening from sleeping NAS disks can take longer. 0 restarts: never restart."
      ),
      section("Log"),
      labelled(
        "Level",
        pick_list(
          &LOG_LEVELS[..],
          LOG_LEVELS
            .iter()
            .copied()
            .find(|level| *level == form.log_level),
          |level: &str| Message::LogLevel(level.to_owned()),
        )
        .width(Length::Fill)
      ),
      labelled("Days kept", input(&form.keep_days, "14", Field::KeepDays)),
      note("Log settings apply when the bridge next starts."),
    ]
    .spacing(8)
    .padding([16, 20]);

    let status: Element<'_, Message> = match &self.status {
      Status::None if self.form != self.saved => text("Unsaved changes").size(14).into(),
      Status::None => text("").into(),
      Status::Info(info) => text(info.clone()).size(14).into(),
      Status::Error(err) => text(err.clone()).size(14).style(text::danger).into(),
    };
    let buttons = row![
      container(status).width(Length::Fill),
      button("Reload from file")
        .style(button::secondary)
        .on_press_maybe(self.changed_outside.then_some(Message::ReloadFile)),
      button("Open config file")
        .style(button::secondary)
        .on_press(Message::OpenFile),
      button("Close")
        .style(button::secondary)
        .on_press(Message::Close),
      button("Save").on_press(Message::Save),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    column![
      scrollable(content).height(Length::Fill),
      rule::horizontal(1),
      container(buttons).padding([10, 20]),
    ]
    .into()
  }

  /// While Jellyfin Media Player still starts itself at logon (a Startup
  /// shortcut), it may start before the bridge: say so.
  fn startup_warning(&self) -> Element<'_, Message> {
    let shortcuts = &self.startup_shortcuts;
    if !self.form.start_player || shortcuts.is_empty() {
      return Space::new().into();
    }
    let names: Vec<String> = shortcuts
      .iter()
      .filter_map(|path| path.file_name())
      .map(|name| name.to_string_lossy().into_owned())
      .collect();
    text(format!(
      "Remove \"{}\" from your Startup folder (shell:startup): it starts Jellyfin Media \
       Player at logon, possibly before the bridge.",
      names.join("\", \"")
    ))
    .size(13)
    .style(text::danger)
    .into()
  }

  fn rules(&self) -> Element<'_, Message> {
    let mut rules = column![].spacing(6);
    for (i, rule) in self.form.path_map.iter().enumerate() {
      rules = rules.push(
        row![
          text_input("/media/movies", &rule.from)
            .on_input(move |from| Message::RuleFrom(i, from))
            .width(Length::FillPortion(1)),
          text("→"),
          text_input(r"\\nas\movies", &rule.to)
            .on_input(move |to| Message::RuleTo(i, to))
            .width(Length::FillPortion(1)),
          button("Remove")
            .style(button::secondary)
            .on_press(Message::RemoveRule(i)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
      );
    }
    rules.into()
  }
}

fn section(title: &str) -> Element<'_, Message> {
  column![
    Space::new().height(6),
    text(title).size(18),
    rule::horizontal(1)
  ]
  .spacing(4)
  .into()
}

fn note(note: &str) -> Element<'_, Message> {
  text(note).size(13).into()
}

fn input<'a>(
  value: &'a str,
  placeholder: &'a str,
  field: Field,
) -> iced::widget::TextInput<'a, Message> {
  text_input(placeholder, value).on_input(move |value| Message::Edit(field, value))
}

fn labelled<'a>(label: &'a str, field: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
  row![text(label).width(190), field.into()]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn open_with(program: &str, path: &Path) {
  if let Err(err) = std::process::Command::new(program).arg(path).spawn() {
    error!("can't open {} with {program}: {err}", path.display());
  }
}
