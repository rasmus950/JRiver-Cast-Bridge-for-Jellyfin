//! What the tray icon shows: one line about the bridge's state, and the
//! icon image itself.

use crate::controller::PlaybackStatus;

/// The socket to the Jellyfin server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
  Connecting,
  Connected,
  Lost,
}

/// The Jellyfin login, which decides the tray's "Log in…" / "Log out" item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
  /// Not known yet (the bridge is starting).
  Unknown,
  LoggedOut,
  LoggedIn {
    user: String,
  },
  /// The server rejected the saved login (revoked or expired).
  Rejected {
    user: String,
  },
}

/// The bridge is starting MC, shown in the middle of the screen (the tray
/// runs `jriver-cast-bridge --notice <arg>` while it lasts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JRiverNotice {
  /// MC wasn't running when a cast came.
  Starting,
  /// MC hung and is being closed and started again.
  Restarting,
}

impl JRiverNotice {
  pub fn arg(self) -> &'static str {
    match self {
      Self::Starting => "starting",
      Self::Restarting => "restarting",
    }
  }

  pub fn from_arg(arg: &str) -> Option<Self> {
    [Self::Starting, Self::Restarting]
      .into_iter()
      .find(|notice| notice.arg() == arg)
  }

  /// Heading and detail for the notice window.
  pub fn text(self) -> (&'static str, &'static str) {
    match self {
      Self::Starting => (
        "Starting JRiver Media Center…",
        "The film from Jellyfin plays as soon as it's ready.",
      ),
      Self::Restarting => (
        "Restarting JRiver Media Center…",
        "It stopped responding, so the bridge closed it and is starting it again.",
      ),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
  pub connection: Connection,
  pub playback: PlaybackStatus,
  pub account: Account,
  /// The bridge is starting MC (for the on-screen notice).
  pub jriver: Option<JRiverNotice>,
  /// The bridge couldn't start or stopped with an error.
  pub error: Option<String>,
}

impl Default for BridgeStatus {
  fn default() -> Self {
    Self {
      connection: Connection::Connecting,
      playback: PlaybackStatus::Idle,
      account: Account::Unknown,
      jriver: None,
      error: None,
    }
  }
}

/// Windows cuts tray tooltips at 127 characters.
pub const TOOLTIP_MAX: usize = 120;

impl BridgeStatus {
  /// One line for the tray menu and tooltip.
  pub fn summary(&self) -> String {
    if let Some(error) = &self.error {
      return format!("Error: {error}");
    }
    match &self.account {
      Account::LoggedOut => return "Not logged in to Jellyfin (use Log in…)".to_owned(),
      Account::Rejected { user } => {
        return format!("Jellyfin rejected the login of {user} (use Log in…)")
      }
      Account::Unknown | Account::LoggedIn { .. } => {}
    }
    let playback = match &self.playback {
      PlaybackStatus::RestartingJRiver => "Restarting JRiver (it stopped responding)".to_owned(),
      PlaybackStatus::Idle => "Waiting for casts".to_owned(),
      PlaybackStatus::Starting => "Starting a cast".to_owned(),
      PlaybackStatus::Playing {
        name,
        paused: false,
      } => format!("Playing {name}"),
      PlaybackStatus::Playing { name, paused: true } => format!("Paused: {name}"),
    };
    match self.connection {
      Connection::Connected => playback,
      Connection::Connecting => format!("{playback} (connecting to Jellyfin)"),
      Connection::Lost => format!("{playback} (Jellyfin connection lost)"),
    }
  }

  /// The tooltip: the app name plus [`Self::summary`], cut to fit.
  pub fn tooltip(&self) -> String {
    truncate_chars(
      &format!("JRiver cast bridge: {}", self.summary()),
      TOOLTIP_MAX,
    )
  }
}

fn truncate_chars(text: &str, max: usize) -> String {
  if text.chars().count() <= max {
    return text.to_owned();
  }
  let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
  out.push('…');
  out
}

/// Side of the generated tray icon, in pixels.
pub const ICON_SIZE: u32 = 32;

/// The tray icon, as RGBA bytes (see [`crate::icon`]).
pub fn icon_rgba() -> Vec<u8> {
  crate::icon::badge_rgba(ICON_SIZE)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn summaries() {
    let mut status = BridgeStatus::default();
    assert_eq!(
      status.summary(),
      "Waiting for casts (connecting to Jellyfin)"
    );
    status.connection = Connection::Connected;
    assert_eq!(status.summary(), "Waiting for casts");
    status.playback = PlaybackStatus::Playing {
      name: "Avatar".to_owned(),
      paused: true,
    };
    assert_eq!(status.summary(), "Paused: Avatar");
    status.connection = Connection::Lost;
    assert_eq!(
      status.summary(),
      "Paused: Avatar (Jellyfin connection lost)"
    );
    status.error = Some("no saved login".to_owned());
    assert_eq!(status.summary(), "Error: no saved login");
  }

  #[test]
  fn tooltips_fit_windows() {
    let status = BridgeStatus {
      connection: Connection::Connected,
      playback: PlaybackStatus::Playing {
        name: "å".repeat(300),
        paused: false,
      },
      account: Account::Unknown,
      jriver: None,
      error: None,
    };
    let tooltip = status.tooltip();
    assert_eq!(tooltip.chars().count(), TOOLTIP_MAX);
    assert!(tooltip.ends_with('…'));
  }

  #[test]
  fn notices_round_trip_through_their_argument() {
    for notice in [JRiverNotice::Starting, JRiverNotice::Restarting] {
      assert_eq!(JRiverNotice::from_arg(notice.arg()), Some(notice));
      assert!(!notice.text().0.is_empty());
    }
    assert_eq!(JRiverNotice::from_arg("other"), None);
  }

  #[test]
  fn icon_is_a_round_badge() {
    let rgba = icon_rgba();
    assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
    let pixel = |x: usize, y: usize| {
      let i = (y * ICON_SIZE as usize + x) * 4;
      &rgba[i..i + 4]
    };
    assert_eq!(pixel(0, 0)[3], 0, "corners are transparent");
    assert_eq!(
      pixel(16, 16),
      [255, 255, 255, 255],
      "centre is the triangle"
    );
    assert_eq!(pixel(5, 16), [0x1f, 0x6f, 0xd6, 255], "badge colour");
  }
}
