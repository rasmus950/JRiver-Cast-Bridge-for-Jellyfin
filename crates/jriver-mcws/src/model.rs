//! Typed views of the MCWS responses the bridge uses.

use crate::{response::Items, Function, McwsError};

/// `UserInterface/Info` `Mode` while MC shows Display view (full screen).
/// Observed on MC 36.0.51: 0 before playback, 2 once a video starts.
pub const UI_MODE_DISPLAY: i32 = 2;

/// `Playback/Info` `State`. Values confirmed on MC 36.0.51.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
  Stopped,
  Paused,
  Playing,
  /// MC is opening the file. Seeks are accepted in this state.
  Opening,
  /// A value MCWS doesn't document; kept so callers can log it.
  Unknown(i32),
}

impl PlayState {
  pub fn from_code(code: i32) -> Self {
    match code {
      0 => Self::Stopped,
      1 => Self::Paused,
      2 => Self::Playing,
      3 => Self::Opening,
      other => Self::Unknown(other),
    }
  }
}

/// `Playback/Pause` `State` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
  Pause,
  Unpause,
  Toggle,
}

impl PauseAction {
  pub(crate) fn param(self) -> &'static str {
    match self {
      Self::Pause => "1",
      Self::Unpause => "0",
      Self::Toggle => "-1",
    }
  }
}

/// The subset of `Playback/Info` the bridge needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackInfo {
  pub state: PlayState,
  /// MC library key of the current file; -1 when there is none. Right after
  /// `PlayByFilename`, MC briefly reports `State=2` with key -1, so a key of
  /// -1 never counts as "our file is playing".
  pub file_key: i64,
  /// Position in milliseconds. MC resets it to 0 when playback stops, so the
  /// last value polled while playing is the one to report.
  pub position_ms: u64,
  pub duration_ms: u64,
  pub playing_now_tracks: u32,
  /// Full path of the current file (`Fields=Filename`). For a Blu-ray MC
  /// reports `...\BDMV\index.bluray;<title>`.
  pub filename: Option<String>,
  /// Display name of the current file.
  pub name: Option<String>,
}

impl PlaybackInfo {
  pub(crate) fn from_items(items: &Items) -> Result<Self, McwsError> {
    let function = Function::PlaybackInfo;
    let state = PlayState::from_code(required_int(items, function, "State")? as i32);
    Ok(Self {
      state,
      file_key: optional_int(items, function, "FileKey")?.unwrap_or(-1),
      position_ms: non_negative(optional_int(items, function, "PositionMS")?),
      duration_ms: non_negative(optional_int(items, function, "DurationMS")?),
      playing_now_tracks: optional_int(items, function, "PlayingNowTracks")?
        .map_or(0, |tracks| u32::try_from(tracks.max(0)).unwrap_or(u32::MAX)),
      filename: non_empty(items.get("Filename")),
      name: non_empty(items.get("Name")),
    })
  }

  /// MC has a real file loaded (not the stale `FileKey=-1` snapshot).
  pub fn has_file(&self) -> bool {
    self.file_key >= 0
  }
}

/// The subset of `Alive` the bridge logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliveInfo {
  pub program_name: String,
  pub program_version: String,
  pub friendly_name: String,
  pub platform: String,
}

impl AliveInfo {
  pub(crate) fn from_items(items: &Items) -> Self {
    let get = |name| items.get(name).unwrap_or_default().to_owned();
    Self {
      program_name: get("ProgramName"),
      program_version: get("ProgramVersion"),
      friendly_name: get("FriendlyName"),
      platform: get("Platform"),
    }
  }
}

/// `UserInterface/Info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiInfo {
  pub mode: i32,
}

impl UiInfo {
  pub(crate) fn from_items(items: &Items) -> Result<Self, McwsError> {
    let mode = required_int(items, Function::UserInterfaceInfo, "Mode")?;
    Ok(Self {
      mode: i32::try_from(mode).unwrap_or(i32::MIN),
    })
  }

  /// MC shows Display view, i.e. the video is full screen.
  pub fn is_display_view(self) -> bool {
    self.mode == UI_MODE_DISPLAY
  }
}

pub(crate) fn required_int(
  items: &Items,
  function: Function,
  name: &str,
) -> Result<i64, McwsError> {
  optional_int(items, function, name)?.ok_or_else(|| McwsError::Parse {
    function,
    reason: format!("missing item {name}"),
  })
}

fn optional_int(items: &Items, function: Function, name: &str) -> Result<Option<i64>, McwsError> {
  match items.get(name).map(str::trim) {
    None | Some("") => Ok(None),
    Some(raw) => raw.parse::<i64>().map(Some).map_err(|_| McwsError::Parse {
      function,
      reason: format!("item {name} is not an integer: {raw:?}"),
    }),
  }
}

fn non_negative(value: Option<i64>) -> u64 {
  value.map_or(0, |v| u64::try_from(v).unwrap_or(0))
}

fn non_empty(value: Option<&str>) -> Option<String> {
  value.filter(|v| !v.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::response::parse;

  // Captured on MC 36.0.51 while a film was playing.
  const PLAYING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<Response Status="OK">
<Item Name="ZoneID">0</Item>
<Item Name="State">2</Item>
<Item Name="FileKey">1</Item>
<Item Name="NextFileKey">-1</Item>
<Item Name="PositionMS">1018500</Item>
<Item Name="DurationMS">11840703</Item>
<Item Name="PlayingNowPosition">0</Item>
<Item Name="PlayingNowTracks">1</Item>
<Item Name="Name">Avatar Fire And Ash</Item>
<Item Name="Filename">M:\Avatar - Fire and Ash (2025)\Avatar.mkv</Item>
</Response>"#;

  // Captured right after PlayByFilename: stale State=2 with no file.
  const STALE: &str = r#"<Response Status="OK">
<Item Name="ZoneID">0</Item>
<Item Name="State">2</Item>
<Item Name="FileKey">-1</Item>
<Item Name="PositionMS">0</Item>
<Item Name="DurationMS">7925000</Item>
<Item Name="PlayingNowTracks">1</Item>
<Item Name="Status"></Item>
</Response>"#;

  #[test]
  fn parses_playing_info() {
    let info = PlaybackInfo::from_items(&parse(Function::PlaybackInfo, PLAYING).unwrap()).unwrap();
    assert_eq!(info.state, PlayState::Playing);
    assert_eq!(info.file_key, 1);
    assert_eq!(info.position_ms, 1_018_500);
    assert_eq!(info.duration_ms, 11_840_703);
    assert_eq!(info.playing_now_tracks, 1);
    assert_eq!(info.name.as_deref(), Some("Avatar Fire And Ash"));
    assert_eq!(
      info.filename.as_deref(),
      Some(r"M:\Avatar - Fire and Ash (2025)\Avatar.mkv")
    );
    assert!(info.has_file());
  }

  #[test]
  fn stale_snapshot_has_no_file() {
    let info = PlaybackInfo::from_items(&parse(Function::PlaybackInfo, STALE).unwrap()).unwrap();
    assert_eq!(info.state, PlayState::Playing);
    assert!(!info.has_file());
    assert_eq!(info.filename, None);
  }

  #[test]
  fn state_codes() {
    assert_eq!(PlayState::from_code(0), PlayState::Stopped);
    assert_eq!(PlayState::from_code(1), PlayState::Paused);
    assert_eq!(PlayState::from_code(2), PlayState::Playing);
    assert_eq!(PlayState::from_code(3), PlayState::Opening);
    assert_eq!(PlayState::from_code(7), PlayState::Unknown(7));
  }

  #[test]
  fn missing_state_is_an_error_but_missing_optional_fields_default() {
    let items = parse(Function::PlaybackInfo, r#"<Response Status="OK"/>"#).unwrap();
    assert!(matches!(
      PlaybackInfo::from_items(&items),
      Err(McwsError::Parse { .. })
    ));
    let items = parse(
      Function::PlaybackInfo,
      r#"<Response Status="OK"><Item Name="State">0</Item></Response>"#,
    )
    .unwrap();
    let info = PlaybackInfo::from_items(&items).unwrap();
    assert_eq!(info.state, PlayState::Stopped);
    assert_eq!(info.file_key, -1);
    assert_eq!(info.position_ms, 0);
    assert_eq!(info.playing_now_tracks, 0);
  }

  #[test]
  fn garbage_number_is_a_parse_error_and_negative_position_clamps() {
    let items = parse(
      Function::PlaybackInfo,
      r#"<Response Status="OK"><Item Name="State">2</Item><Item Name="PositionMS">abc</Item></Response>"#,
    )
    .unwrap();
    assert!(matches!(
      PlaybackInfo::from_items(&items),
      Err(McwsError::Parse { .. })
    ));
    let items = parse(
      Function::PlaybackInfo,
      r#"<Response Status="OK"><Item Name="State">2</Item><Item Name="PositionMS">-5</Item></Response>"#,
    )
    .unwrap();
    assert_eq!(PlaybackInfo::from_items(&items).unwrap().position_ms, 0);
  }

  #[test]
  fn ui_mode_display() {
    let items = parse(
      Function::UserInterfaceInfo,
      r#"<Response Status="OK"><Item Name="Mode">2</Item><Item Name="InternalMode">-999</Item></Response>"#,
    )
    .unwrap();
    assert!(UiInfo::from_items(&items).unwrap().is_display_view());
    let items = parse(
      Function::UserInterfaceInfo,
      r#"<Response Status="OK"><Item Name="Mode">0</Item></Response>"#,
    )
    .unwrap();
    assert!(!UiInfo::from_items(&items).unwrap().is_display_view());
  }

  #[test]
  fn pause_params() {
    assert_eq!(PauseAction::Pause.param(), "1");
    assert_eq!(PauseAction::Unpause.param(), "0");
    assert_eq!(PauseAction::Toggle.param(), "-1");
  }
}
