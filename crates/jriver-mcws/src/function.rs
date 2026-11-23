/// The MCWS functions this crate may call. This is the whole allowed set
/// (SPEC.md, "JRiver side"); there is deliberately no way to send any other
/// MCWS function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Function {
  /// Health check and version.
  Alive,
  /// Exchange Media Network credentials for a token (only when auth is on).
  Authenticate,
  /// Playback state, position and duration of the configured zone.
  PlaybackInfo,
  /// Start a file. Appends to Playing Now, so it always follows `ClearPlaylist`.
  PlayByFilename,
  /// Stop and clear Playing Now.
  ClearPlaylist,
  /// Get or set the position in milliseconds.
  Position,
  /// Set the pause state.
  Pause,
  /// Stop playback.
  Stop,
  /// Read-only: the UI mode, to confirm MC is in Display view.
  UserInterfaceInfo,
  /// `Control/MCC` with `MCC_MINIMIZE_WINDOW` (10014) and nothing else: once
  /// a playback the bridge started has ended, minimize MC so the Jellyfin app
  /// gets focus back. The command is fixed by [`crate::McwsClient::minimize_window`];
  /// besides it, only the two track commands below can be sent.
  MinimizeWindow,
  /// `Control/MCC` with `MCC_SET_AUDIO_STREAM` (10053): pick the audio track
  /// Jellyfin asked for. Fixed by
  /// [`crate::McwsClient::select_audio_stream`].
  SelectAudioStream,
  /// `Control/MCC` with `MCC_SET_SUBTITLES` (10052): pick the subtitle track
  /// Jellyfin asked for. Fixed by
  /// [`crate::McwsClient::select_subtitles`].
  SelectSubtitles,
}

impl Function {
  /// Path relative to the MCWS base URL (`.../MCWS/v1/`).
  pub const fn path(self) -> &'static str {
    match self {
      Self::Alive => "Alive",
      Self::Authenticate => "Authenticate",
      Self::PlaybackInfo => "Playback/Info",
      Self::PlayByFilename => "Playback/PlayByFilename",
      Self::ClearPlaylist => "Playback/ClearPlaylist",
      Self::Position => "Playback/Position",
      Self::Pause => "Playback/Pause",
      Self::Stop => "Playback/Stop",
      Self::UserInterfaceInfo => "UserInterface/Info",
      Self::MinimizeWindow | Self::SelectAudioStream | Self::SelectSubtitles => "Control/MCC",
    }
  }

  /// Whether the call targets a playback zone. Zoned calls always carry an
  /// explicit zone ID, never `-1` (MC's *current* zone, which can be a DLNA
  /// renderer).
  pub const fn zoned(self) -> bool {
    match self {
      Self::Alive | Self::Authenticate | Self::UserInterfaceInfo => false,
      Self::PlaybackInfo
      | Self::PlayByFilename
      | Self::ClearPlaylist
      | Self::Position
      | Self::Pause
      | Self::Stop
      // MCC takes a zone too; never let it default to -1.
      | Self::MinimizeWindow
      | Self::SelectAudioStream
      | Self::SelectSubtitles => true,
    }
  }

  /// Whether the call needs the Media Network token when auth is on.
  pub const fn needs_token(self) -> bool {
    !matches!(self, Self::Alive | Self::Authenticate)
  }
}

impl std::fmt::Display for Function {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.path())
  }
}
