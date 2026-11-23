//! The About window: `jriver-cast-bridge --about` (SPEC.md "About").
//!
//! Name, version, build, license and credits, and where the config and logs
//! live. Its own process, as the other windows; one at a time.

use std::{
  path::{Path, PathBuf},
  process::ExitCode,
};

use iced::{
  widget::{button, column, container, row, rule, scrollable, text, Space},
  Alignment, Element, Length, Size, Task,
};
use jriver_cast_bridge::{config, instance};
use log::{error, info};

pub const ABOUT_LOCK_FILE: &str = "about.lock";

const NAME: &str = "JRiver Cast Bridge";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("BRIDGE_COMMIT");
const BUILD_DATE: &str = env!("BRIDGE_BUILD_DATE");

/// The version line, also what "Copy version info" copies.
fn version_info() -> String {
  format!(
    "{NAME} {VERSION} (commit {COMMIT}, built {BUILD_DATE}) on {} {}",
    std::env::consts::OS,
    std::env::consts::ARCH
  )
}

pub fn run(config_path: Option<PathBuf>, log_dir: Option<PathBuf>) -> ExitCode {
  let _lock = match config::app_dir()
    .map_err(|err| err.to_string())
    .and_then(|dir| {
      instance::acquire_file(&dir, ABOUT_LOCK_FILE).map_err(|err| match err {
        instance::InstanceError::AlreadyRunning(_) => String::new(),
        other => other.to_string(),
      })
    }) {
    Ok(lock) => lock,
    Err(err) if err.is_empty() => {
      info!("the About window is already open");
      return ExitCode::SUCCESS;
    }
    Err(err) => {
      error!("can't open the About window: {err}");
      return ExitCode::FAILURE;
    }
  };
  let result = iced::application(
    move || {
      (
        About {
          config_path: config_path.clone(),
          log_dir: log_dir.clone(),
          copied: false,
        },
        Task::none(),
      )
    },
    About::update,
    About::view,
  )
  .title("About JRiver Cast Bridge")
  .window_size(Size::new(520.0, 560.0))
  .resizable(false)
  .centered()
  .run();
  match result {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      error!("the About window failed: {err}");
      ExitCode::FAILURE
    }
  }
}

struct About {
  config_path: Option<PathBuf>,
  log_dir: Option<PathBuf>,
  copied: bool,
}

#[derive(Debug, Clone)]
enum Message {
  Copy,
  OpenConfigFolder,
  OpenLogFolder,
  Close,
}

impl About {
  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::Copy => {
        self.copied = true;
        return iced::clipboard::write(version_info());
      }
      Message::OpenConfigFolder => {
        if let Some(dir) = self.config_path.as_deref().and_then(Path::parent) {
          open_folder(dir);
        }
      }
      Message::OpenLogFolder => {
        if let Some(dir) = &self.log_dir {
          open_folder(dir);
        }
      }
      Message::Close => return iced::exit(),
    }
    Task::none()
  }

  fn view(&self) -> Element<'_, Message> {
    let shown = |path: Option<&Path>| {
      path.map_or_else(|| "unknown".to_owned(), |path| path.display().to_string())
    };
    let body = column![
      text(NAME).size(26),
      text(format!("Version {VERSION}")).size(16),
      text(format!("Commit {COMMIT}, built {BUILD_DATE}")).size(13),
      Space::new().height(4),
      text(
        "Makes JRiver Media Center a Jellyfin cast target: cast from the Jellyfin app \
         and the film plays in Media Center, with Jellyfin's remote, progress and \
         resume points."
      )
      .size(14),
      rule::horizontal(1),
      text("Files").size(16),
      text(format!("Config: {}", shown(self.config_path.as_deref()))).size(13),
      text(format!("Logs: {}", shown(self.log_dir.as_deref()))).size(13),
      text("The Jellyfin login is kept in Windows Credential Manager.").size(13),
      rule::horizontal(1),
      text("License and credits").size(16),
      text("MIT License. Copyright (c) 2026 rasmus950.").size(13),
      text(
        "Includes the Jellyfin WebSocket code of JellyPilot (formerly JMSR) by hewel and \
         the JellyPilot contributors, used under its MIT license (Copyright (c) 2025 \
         JellyPilot Contributors). Built with Rust, iced, tokio and reqwest. JRiver Media \
         Center is a product of JRiver, Inc.; Jellyfin is free software by the Jellyfin \
         project. This bridge isn't affiliated with either."
      )
      .size(13),
      text("github.com/rasmus950/JRiver-Cast-Bridge-for-Jellyfin").size(13),
    ]
    .spacing(8)
    .padding([16, 20]);

    let copy = if self.copied {
      "Copied"
    } else {
      "Copy version info"
    };
    let buttons = row![
      button(copy)
        .style(button::secondary)
        .on_press(Message::Copy),
      button("Config folder")
        .style(button::secondary)
        .on_press_maybe(
          self
            .config_path
            .is_some()
            .then_some(Message::OpenConfigFolder)
        ),
      button("Log folder")
        .style(button::secondary)
        .on_press_maybe(self.log_dir.is_some().then_some(Message::OpenLogFolder)),
      Space::new().width(Length::Fill),
      button("Close").on_press(Message::Close),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    column![
      scrollable(body).height(Length::Fill),
      rule::horizontal(1),
      container(buttons).padding([10, 20]),
    ]
    .into()
  }
}

fn open_folder(dir: &Path) {
  if let Err(err) = std::process::Command::new("explorer.exe").arg(dir).spawn() {
    error!("can't open {}: {err}", dir.display());
  }
}
