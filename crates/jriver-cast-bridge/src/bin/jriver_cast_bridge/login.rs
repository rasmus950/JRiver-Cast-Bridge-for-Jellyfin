//! The login window: `jriver-cast-bridge --login` (SPEC.md "Login").
//!
//! It runs as its own process because the tray already owns the one event
//! loop a process may have. It logs in to the server named in the config,
//! saves the login in Credential Manager, and tells the running bridge over
//! the local pipe, which reconnects with it. Only one window at a time.

use std::process::ExitCode;

use iced::{
  widget::{button, column, operation, row, text, text_input, Space},
  Alignment, Element, Length, Size, Task,
};
use jriver_cast_bridge::{
  config::{self, Config, ConfigError, ConfigSource, JellyfinSection},
  credentials, instance, ipc,
};
use log::{error, info, warn};

const USER_FIELD: &str = "user";

pub fn run(loaded: Result<(Config, ConfigSource), ConfigError>) -> ExitCode {
  let _lock = match config::app_dir()
    .map_err(|err| err.to_string())
    .and_then(|dir| {
      instance::acquire_file(&dir, instance::LOGIN_LOCK_FILE).map_err(|err| match err {
        instance::InstanceError::AlreadyRunning(_) => String::new(),
        other => other.to_string(),
      })
    }) {
    Ok(lock) => lock,
    Err(err) if err.is_empty() => {
      info!("a login window is already open");
      return ExitCode::SUCCESS;
    }
    Err(err) => {
      error!("can't open the login window: {err}");
      return ExitCode::FAILURE;
    }
  };
  let setup = loaded
    .map_err(|err| err.to_string())
    .and_then(|(config, source)| match source {
      ConfigSource::File(_) => config.jellyfin().cloned(),
      ConfigSource::Defaults(path) => Err(format!(
        "{} not found; use Open config in the tray menu first",
        path.display()
      )),
    });
  let result = iced::application(
    move || LoginWindow::boot(setup.clone()),
    LoginWindow::update,
    LoginWindow::view,
  )
  .title("JRiver Cast Bridge: log in to Jellyfin")
  .window_size(Size::new(440.0, 320.0))
  .resizable(false)
  .centered()
  .run();
  match result {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      error!("the login window failed: {err}");
      ExitCode::FAILURE
    }
  }
}

struct LoginWindow {
  /// The server to log in to, or why there is none.
  server: Result<JellyfinSection, String>,
  user: String,
  password: String,
  busy: bool,
  error: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
  User(String),
  Password(String),
  Submit,
  Cancel,
  Done(Result<(), String>),
}

impl LoginWindow {
  fn boot(server: Result<JellyfinSection, String>) -> (Self, Task<Message>) {
    (
      Self {
        server,
        user: String::new(),
        password: String::new(),
        busy: false,
        error: None,
      },
      operation::focus(USER_FIELD),
    )
  }

  fn can_submit(&self) -> bool {
    self.server.is_ok() && !self.busy && !self.user.trim().is_empty()
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::User(user) => self.user = user,
      Message::Password(password) => self.password = password,
      Message::Submit => {
        let Ok(server) = &self.server else {
          return Task::none();
        };
        if !self.can_submit() {
          return Task::none();
        }
        self.busy = true;
        self.error = None;
        let server = server.clone();
        let user = self.user.trim().to_owned();
        let password = std::mem::take(&mut self.password);
        return Task::perform(log_in(server, user, password), Message::Done);
      }
      Message::Cancel => return iced::exit(),
      Message::Done(Ok(())) => return iced::exit(),
      Message::Done(Err(err)) => {
        self.busy = false;
        self.error = Some(err);
      }
    }
    Task::none()
  }

  fn view(&self) -> Element<'_, Message> {
    let heading = text("Log in to Jellyfin").size(22);
    let body: Element<'_, Message> = match &self.server {
      Err(err) => column![
        text(err.clone()).style(text::danger),
        Space::new().height(Length::Fill),
        row![
          Space::new().width(Length::Fill),
          button("Close").on_press(Message::Cancel)
        ],
      ]
      .spacing(12)
      .into(),
      Ok(server) => {
        let mut user = text_input("User name", &self.user).id(USER_FIELD);
        let mut password = text_input("Password", &self.password).secure(true);
        if !self.busy {
          user = user.on_input(Message::User).on_submit(Message::Submit);
          password = password
            .on_input(Message::Password)
            .on_submit(Message::Submit);
        }
        let status = match (&self.error, self.busy) {
          (Some(err), _) => text(err.clone()).style(text::danger),
          (None, true) => text("Logging in…"),
          (None, false) => text(""),
        };
        column![
          text(format!("Server: {}", server.url.trim_end_matches('/'))).size(14),
          text(format!("Cast target: {}", server.device_name)).size(14),
          user,
          password,
          status.size(14),
          Space::new().height(Length::Fill),
          row![
            Space::new().width(Length::Fill),
            button("Cancel")
              .style(button::secondary)
              .on_press(Message::Cancel),
            button("Log in").on_press_maybe(self.can_submit().then_some(Message::Submit)),
          ]
          .spacing(8)
          .align_y(Alignment::Center),
        ]
        .spacing(10)
        .into()
      }
    };
    column![heading, body]
      .spacing(14)
      .padding(20)
      .width(Length::Fill)
      .height(Length::Fill)
      .into()
  }
}

/// Log in, save the login, and tell the running bridge.
async fn log_in(server: JellyfinSection, user: String, password: String) -> Result<(), String> {
  let result = credentials::log_in(&server, &user, &password).await;
  drop(password);
  let (stored, store) = result.map_err(|err| err.to_string())?;
  info!(
    "logged in to Jellyfin as {} (saved as {})",
    stored.user_name,
    store.account()
  );
  let notice = ipc::Request::Login {
    account: store.account().to_owned(),
  };
  match ipc::send(&notice, None).await {
    Ok(ipc::Reply::Done) => info!("told the running bridge about the new login"),
    Ok(ipc::Reply::NotRunning) => {
      info!("no bridge is running; it will use the login when it starts")
    }
    Err(err) => warn!("couldn't tell the running bridge about the new login ({err}); restart it"),
  }
  Ok(())
}
