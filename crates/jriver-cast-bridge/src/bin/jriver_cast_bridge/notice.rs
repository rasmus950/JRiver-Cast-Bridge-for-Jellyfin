//! The "Starting JRiver…" notice: `jriver-cast-bridge --notice <kind>`
//! (SPEC.md "JRiver not running" and "JRiver hung").
//!
//! A small borderless window in the middle of the screen, above everything,
//! while the bridge starts or restarts MC. The tray starts this process and
//! closes it when MC answers. It runs as its own process because the tray
//! owns the one event loop a process may have. It also closes itself after
//! a while, so a tray that died can't leave it up.

use std::{process::ExitCode, time::Duration};

use iced::{
  widget::{column, container, text},
  window, Alignment, Element, Length, Size, Task, Theme,
};
use jriver_cast_bridge::status::JRiverNotice;
use log::error;

/// Longer than any start or restart the bridge waits for (a 15 s kill plus
/// a 60 s launch).
const MAX_SHOWN: Duration = Duration::from_secs(120);

pub fn run(kind: &str) -> ExitCode {
  let Some(notice) = JRiverNotice::from_arg(kind) else {
    error!("unknown notice {kind:?}");
    return ExitCode::FAILURE;
  };
  let result = iced::application(
    move || (Notice(notice), Task::none()),
    Notice::update,
    Notice::view,
  )
  .title("JRiver Cast Bridge")
  .window(window::Settings {
    size: Size::new(520.0, 130.0),
    position: window::Position::Centered,
    resizable: false,
    decorations: false,
    level: window::Level::AlwaysOnTop,
    platform_specific: window::settings::PlatformSpecific {
      skip_taskbar: true,
      ..Default::default()
    },
    ..Default::default()
  })
  .theme(|_: &Notice| Theme::Dark)
  .subscription(|_: &Notice| iced::time::every(MAX_SHOWN).map(|_| Expired))
  .run();
  match result {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      error!("the notice window failed: {err}");
      ExitCode::FAILURE
    }
  }
}

struct Notice(JRiverNotice);

#[derive(Debug, Clone, Copy)]
struct Expired;

impl Notice {
  fn update(&mut self, _: Expired) -> Task<Expired> {
    iced::exit()
  }

  fn view(&self) -> Element<'_, Expired> {
    let (heading, detail) = self.0.text();
    container(
      column![text(heading).size(24), text(detail).size(15)]
        .spacing(10)
        .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .style(container::bordered_box)
    .into()
  }
}
