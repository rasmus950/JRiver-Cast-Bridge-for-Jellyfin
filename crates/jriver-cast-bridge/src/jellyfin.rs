//! The bridge's own Jellyfin client.
//!
//! It covers only what a cast receiver needs: password login, cast-target
//! registration, looking up an item's file path, and playback reports. It
//! never asks for a stream URL or `PlaybackInfo`, so it can't start a
//! transcode, and every report says `PlayMethod: DirectPlay`.
//!
//! Endpoints and field names were checked against Jellyfin 10.11.11's own
//! OpenAPI.

use std::{
  fmt,
  sync::atomic::{AtomicBool, Ordering},
  time::Duration,
};

use log::debug;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{header, Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::units::Ticks;

pub const CLIENT_NAME: &str = "jriver-cast-bridge";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// `GeneralCommandType` values the bridge acts on. Volume and mute are left
/// out so remotes don't offer controls that do nothing (the bridge never
/// sends volume commands). `PlayMediaSource` makes jellyfin-web keep the
/// version, audio and subtitle pickers on an item's page while casting here
/// (it hides them otherwise); the picks then come in the Play command's
/// `MediaSourceId`, `AudioStreamIndex` and `SubtitleStreamIndex`.
pub const SUPPORTED_COMMANDS: &[&str] = &[
  "Play",
  "PlayState",
  "PlayMediaSource",
  "SetAudioStreamIndex",
  "SetSubtitleStreamIndex",
];

/// The same when JRiver picks the tracks (setting `tracks_from_jellyfin`
/// off): no track pickers or track buttons in Jellyfin's apps.
pub const SUPPORTED_COMMANDS_WITHOUT_TRACKS: &[&str] = &["Play", "PlayState"];

/// Query-string encoding: everything but unreserved characters.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
  .remove(b'-')
  .remove(b'.')
  .remove(b'_')
  .remove(b'~');

/// Longest error body kept in an error message.
const MAX_DETAIL: usize = 200;

/// A Jellyfin access token. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
  pub fn new(token: impl Into<String>) -> Self {
    Self(token.into())
  }

  /// The token itself, for the `Authorization` header and the socket URL only.
  pub fn expose(&self) -> &str {
    &self.0
  }
}

impl fmt::Debug for AccessToken {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("AccessToken(<redacted>)")
  }
}

/// How the bridge shows up on the server: the cast-target name and a stable
/// device ID (a token belongs to one device ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
  pub device_name: String,
  pub device_id: String,
}

/// A logged-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
  pub user_id: String,
  pub user_name: String,
  pub token: AccessToken,
}

#[derive(Debug, thiserror::Error)]
pub enum JellyfinError {
  #[error("{what}: can't reach the Jellyfin server: {source}")]
  Transport {
    what: &'static str,
    #[source]
    source: reqwest::Error,
  },
  #[error("{what}: the user name or password is wrong")]
  LoginRejected { what: &'static str },
  #[error(
    "{what}: the server rejected the saved login (HTTP 401); log in again from the tray menu"
  )]
  Unauthorized { what: &'static str },
  #[error("{what}: HTTP {status}: {detail}")]
  Status {
    what: &'static str,
    status: u16,
    detail: String,
  },
  #[error("{what}: unexpected response: {message}")]
  Parse { what: &'static str, message: String },
  #[error("not logged in to Jellyfin; log in from the tray menu")]
  NotLoggedIn,
  #[error("{0}")]
  InvalidArgument(String),
}

impl JellyfinError {
  /// Worth retrying later: the server or network may come back. A rejected
  /// login or a bad request won't fix itself.
  pub fn is_retryable(&self) -> bool {
    match self {
      Self::Transport { .. } => true,
      Self::Status { status, .. } => *status >= 500 || *status == 408 || *status == 429,
      _ => false,
    }
  }
}

/// `GET /Items/{id}`: the fields the bridge needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
  pub id: String,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default, rename = "Type")]
  pub item_type: Option<String>,
  #[serde(default)]
  pub media_type: Option<String>,
  #[serde(default)]
  pub path: Option<String>,
  /// `VideoFile`, `Iso`, `Dvd` or `BluRay`.
  #[serde(default)]
  pub video_type: Option<String>,
  #[serde(default)]
  pub run_time_ticks: Option<i64>,
  #[serde(default)]
  pub media_sources: Option<Vec<MediaSourceInfo>>,
}

/// One version of an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSourceInfo {
  pub id: String,
  #[serde(default)]
  pub path: Option<String>,
  /// `File` for a file on the server's disk; anything else is a stream.
  #[serde(default)]
  pub protocol: Option<String>,
  #[serde(default)]
  pub video_type: Option<String>,
  /// The file's tracks, for mapping Jellyfin's track choice to JRiver's.
  #[serde(default)]
  pub media_streams: Vec<MediaStream>,
}

/// One track of a media source, as Jellyfin numbers it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStream {
  /// Jellyfin's index across all of the file's streams (video, audio,
  /// subtitles, then external subtitle files).
  pub index: i32,
  /// `Video`, `Audio`, `Subtitle`, …
  #[serde(rename = "Type", default)]
  pub kind: String,
  #[serde(default)]
  pub language: Option<String>,
  #[serde(default)]
  pub display_title: Option<String>,
  /// The file marks it as the default track.
  #[serde(default)]
  pub is_default: bool,
  /// A separate file beside the video (e.g. `.srt`), not inside it.
  #[serde(default)]
  pub is_external: bool,
}

/// The one play method the bridge ever reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PlayMethod {
  DirectPlay,
}

/// Body of `POST /Sessions/Playing` and `/Sessions/Playing/Progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProgressReport {
  pub item_id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub media_source_id: Option<String>,
  pub play_session_id: String,
  pub position_ticks: Ticks,
  pub is_paused: bool,
  pub is_muted: bool,
  pub can_seek: bool,
  pub play_method: PlayMethod,
  /// The audio and subtitle stream in use, as far as the bridge knows (MC
  /// can't be asked), so remotes show the right one picked.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub audio_stream_index: Option<i32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub subtitle_stream_index: Option<i32>,
}

/// Body of `POST /Sessions/Playing/Stopped`.
///
/// `PositionTicks` is always sent: a stop report without it makes the
/// server assume the item was played to the end. `Failed` tells the server
/// the item never really played, so it leaves the user's resume point and
/// played status alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct StopReport {
  pub item_id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub media_source_id: Option<String>,
  pub play_session_id: String,
  pub position_ticks: Ticks,
  pub failed: bool,
}

/// A playback report, in the order it must reach the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
  Start(ProgressReport),
  Progress(ProgressReport),
  Stopped(StopReport),
}

impl Report {
  pub fn play_session_id(&self) -> &str {
    match self {
      Self::Start(r) | Self::Progress(r) => &r.play_session_id,
      Self::Stopped(r) => &r.play_session_id,
    }
  }

  fn what(&self) -> &'static str {
    match self {
      Self::Start(_) => "report playback start",
      Self::Progress(_) => "report playback progress",
      Self::Stopped(_) => "report playback stop",
    }
  }

  fn path(&self) -> &'static str {
    match self {
      Self::Start(_) => "/Sessions/Playing",
      Self::Progress(_) => "/Sessions/Playing/Progress",
      Self::Stopped(_) => "/Sessions/Playing/Stopped",
    }
  }
}

/// The bridge's own session, as `GET /Sessions?deviceId=` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionSummary {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub user_name: Option<String>,
  #[serde(default)]
  pub client: Option<String>,
  #[serde(default)]
  pub device_name: Option<String>,
  #[serde(default)]
  pub device_id: Option<String>,
  #[serde(default)]
  pub supports_remote_control: Option<bool>,
  #[serde(default)]
  pub supports_media_control: Option<bool>,
  #[serde(default)]
  pub playable_media_types: Option<Vec<String>>,
  #[serde(default)]
  pub supported_commands: Option<Vec<String>>,
  /// What the server thinks this device is playing. Stale after the bridge
  /// was killed mid-film.
  #[serde(default)]
  pub now_playing_item: Option<NowPlayingItem>,
  #[serde(default)]
  pub play_state: Option<SessionPlayState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NowPlayingItem {
  pub id: String,
  #[serde(default)]
  pub name: Option<String>,
  /// The tracks the server lists for the playing source (clients build
  /// their track menus from these).
  #[serde(default)]
  pub media_streams: Option<Vec<MediaStream>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionPlayState {
  #[serde(default)]
  pub position_ticks: Option<i64>,
  #[serde(default)]
  pub media_source_id: Option<String>,
  #[serde(default)]
  pub audio_stream_index: Option<i32>,
  #[serde(default)]
  pub subtitle_stream_index: Option<i32>,
}

/// A film the server shows as playing on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
  pub item_id: String,
  pub media_source_id: Option<String>,
  pub position_ticks: i64,
}

impl SessionSummary {
  pub fn now_playing(&self) -> Option<NowPlaying> {
    let item = self.now_playing_item.as_ref()?;
    if item.id.is_empty() {
      return None;
    }
    let state = self.play_state.clone().unwrap_or_default();
    Some(NowPlaying {
      item_id: item.id.clone(),
      media_source_id: state.media_source_id.filter(|id| !id.is_empty()),
      position_ticks: state.position_ticks.unwrap_or(0).max(0),
    })
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticationResult {
  user: UserDto,
  access_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserDto {
  id: String,
  name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Capabilities<'a> {
  playable_media_types: &'a [&'a str],
  supported_commands: &'a [&'a str],
  supports_media_control: bool,
  supports_persistent_identifier: bool,
}

pub struct JellyfinClient {
  http: reqwest::Client,
  server_url: String,
  identity: Identity,
  login: Option<Login>,
  /// Register with the track commands (see [`Self::set_track_commands`]).
  track_commands: AtomicBool,
}

impl fmt::Debug for JellyfinClient {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("JellyfinClient")
      .field("server_url", &self.server_url)
      .field("identity", &self.identity)
      .field("login", &self.login)
      .finish()
  }
}

impl JellyfinClient {
  /// `server_url` is the server's base URL (`http://host:8096`, plus any
  /// reverse-proxy prefix). `login` is `None` only for logging in.
  pub fn new(
    server_url: &str,
    identity: Identity,
    login: Option<Login>,
    timeout: Duration,
  ) -> Result<Self, JellyfinError> {
    let server_url = server_url.trim().trim_end_matches('/').to_owned();
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
      return Err(JellyfinError::InvalidArgument(format!(
        "the Jellyfin URL must start with http:// or https://, got {server_url:?}"
      )));
    }
    check_header_value("device name", &identity.device_name)?;
    check_header_value("device ID", &identity.device_id)?;
    let http = reqwest::Client::builder()
      .timeout(timeout)
      .redirect(reqwest::redirect::Policy::none())
      .no_proxy()
      .user_agent(format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
      .build()
      .map_err(|source| JellyfinError::Transport {
        what: "create HTTP client",
        source: source.without_url(),
      })?;
    Ok(Self {
      http,
      server_url,
      identity,
      login,
      track_commands: AtomicBool::new(true),
    })
  }

  /// Whether the next registration offers the track commands (and so
  /// Jellyfin's track pickers). A reload that keeps the Jellyfin session
  /// sets it before registering again.
  pub fn set_track_commands(&self, on: bool) {
    self.track_commands.store(on, Ordering::Relaxed);
  }

  pub fn server_url(&self) -> &str {
    &self.server_url
  }

  pub fn identity(&self) -> &Identity {
    &self.identity
  }

  pub fn login_info(&self) -> Option<&Login> {
    self.login.as_ref()
  }

  /// `MediaBrowser Client=…, Device=…, DeviceId=…, Version=…[, Token=…]`.
  fn authorization(&self, token: Option<&AccessToken>) -> String {
    let mut value = format!(
      r#"MediaBrowser Client="{CLIENT_NAME}", Device="{}", DeviceId="{}", Version="{CLIENT_VERSION}""#,
      self.identity.device_name, self.identity.device_id
    );
    if let Some(token) = token {
      value.push_str(&format!(r#", Token="{}""#, token.expose()));
    }
    value
  }

  fn logged_in(&self) -> Result<&Login, JellyfinError> {
    self.login.as_ref().ok_or(JellyfinError::NotLoggedIn)
  }

  async fn send(
    &self,
    what: &'static str,
    method: Method,
    path_and_query: &str,
    body: Option<String>,
    token: Option<&AccessToken>,
  ) -> Result<reqwest::Response, JellyfinError> {
    let url = format!("{}{path_and_query}", self.server_url);
    debug!("Jellyfin: {method} {path_and_query}");
    let mut request = self
      .http
      .request(method, &url)
      .header(header::AUTHORIZATION, self.authorization(token))
      .header(header::ACCEPT, "application/json");
    if let Some(body) = body {
      request = request
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    }
    let response = request
      .send()
      .await
      .map_err(|source| JellyfinError::Transport {
        what,
        source: source.without_url(),
      })?;
    let status = response.status();
    debug!("Jellyfin: {what}: HTTP {}", status.as_u16());
    if status.is_success() {
      return Ok(response);
    }
    if status == StatusCode::UNAUTHORIZED {
      return Err(if token.is_some() {
        JellyfinError::Unauthorized { what }
      } else {
        JellyfinError::LoginRejected { what }
      });
    }
    let detail = response.text().await.unwrap_or_default();
    Err(JellyfinError::Status {
      what,
      status: status.as_u16(),
      detail: truncate(detail.trim(), MAX_DETAIL),
    })
  }

  async fn json<T: DeserializeOwned>(
    what: &'static str,
    response: reqwest::Response,
  ) -> Result<T, JellyfinError> {
    let bytes = response
      .bytes()
      .await
      .map_err(|source| JellyfinError::Transport {
        what,
        source: source.without_url(),
      })?;
    serde_json::from_slice(&bytes).map_err(|err| JellyfinError::Parse {
      what,
      message: err.to_string(),
    })
  }

  fn to_body(what: &'static str, value: &impl Serialize) -> Result<String, JellyfinError> {
    serde_json::to_string(value).map_err(|err| JellyfinError::Parse {
      what,
      message: err.to_string(),
    })
  }

  /// `POST /Users/AuthenticateByName`. The password is only sent, never kept.
  pub async fn login(&self, username: &str, password: &str) -> Result<Login, JellyfinError> {
    const WHAT: &str = "log in";
    if username.trim().is_empty() {
      return Err(JellyfinError::InvalidArgument(
        "the user name is empty".to_owned(),
      ));
    }
    let body = serde_json::json!({ "Username": username, "Pw": password }).to_string();
    let response = self
      .send(
        WHAT,
        Method::POST,
        "/Users/AuthenticateByName",
        Some(body),
        None,
      )
      .await?;
    let result: AuthenticationResult = Self::json(WHAT, response).await?;
    if result.access_token.is_empty() || result.user.id.is_empty() {
      return Err(JellyfinError::Parse {
        what: WHAT,
        message: "no access token or user ID in the reply".to_owned(),
      });
    }
    Ok(Login {
      user_id: result.user.id,
      user_name: result.user.name,
      token: AccessToken::new(result.access_token),
    })
  }

  /// `POST /Sessions/Logout`: revokes the token on the server.
  pub async fn logout(&self) -> Result<(), JellyfinError> {
    let login = self.logged_in()?;
    self
      .send(
        "log out",
        Method::POST,
        "/Sessions/Logout",
        None,
        Some(&login.token),
      )
      .await?;
    Ok(())
  }

  /// `POST /Sessions/Capabilities/Full`: makes this device a cast target
  /// that plays video and takes remote control.
  pub async fn register(&self) -> Result<(), JellyfinError> {
    const WHAT: &str = "register as a cast target";
    let login = self.logged_in()?;
    let body = Self::to_body(
      WHAT,
      &Capabilities {
        playable_media_types: &["Video"],
        supported_commands: if self.track_commands.load(Ordering::Relaxed) {
          SUPPORTED_COMMANDS
        } else {
          SUPPORTED_COMMANDS_WITHOUT_TRACKS
        },
        supports_media_control: true,
        supports_persistent_identifier: true,
      },
    )?;
    self
      .send(
        WHAT,
        Method::POST,
        "/Sessions/Capabilities/Full",
        Some(body),
        Some(&login.token),
      )
      .await?;
    Ok(())
  }

  /// `GET /Sessions?deviceId=`: this device's session(s), to check that the
  /// registration took.
  pub async fn own_sessions(&self) -> Result<Vec<SessionSummary>, JellyfinError> {
    const WHAT: &str = "list sessions";
    let login = self.logged_in()?;
    let path = format!("/Sessions?deviceId={}", encode(&self.identity.device_id));
    let response = self
      .send(WHAT, Method::GET, &path, None, Some(&login.token))
      .await?;
    Self::json(WHAT, response).await
  }

  /// The film the server shows as playing on this device, if any.
  pub async fn now_playing(&self) -> Result<Option<NowPlaying>, JellyfinError> {
    let sessions = self.own_sessions().await?;
    Ok(sessions.iter().find_map(SessionSummary::now_playing))
  }

  /// `GET /Items/{id}?userId=` (the older `/Users/{userId}/Items/{id}` is
  /// no longer documented in 10.11).
  pub async fn item(&self, item_id: &str) -> Result<Item, JellyfinError> {
    const WHAT: &str = "look up the item";
    let login = self.logged_in()?;
    check_id("item ID", item_id)?;
    let path = format!("/Items/{item_id}?userId={}", encode(&login.user_id));
    let response = self
      .send(WHAT, Method::GET, &path, None, Some(&login.token))
      .await?;
    Self::json(WHAT, response).await
  }

  /// Send one playback report.
  pub async fn report(&self, report: &Report) -> Result<(), JellyfinError> {
    let what = report.what();
    let login = self.logged_in()?;
    let body = match report {
      Report::Start(r) | Report::Progress(r) => Self::to_body(what, r)?,
      Report::Stopped(r) => Self::to_body(what, r)?,
    };
    self
      .send(
        what,
        Method::POST,
        report.path(),
        Some(body),
        Some(&login.token),
      )
      .await?;
    Ok(())
  }

  /// `ws(s)://…/socket?ApiKey=…&deviceId=…`. Contains the token: never log it.
  pub fn websocket_url(&self) -> Result<String, JellyfinError> {
    let login = self.logged_in()?;
    let base = if let Some(rest) = self.server_url.strip_prefix("https://") {
      format!("wss://{rest}")
    } else if let Some(rest) = self.server_url.strip_prefix("http://") {
      format!("ws://{rest}")
    } else {
      unreachable!("checked in new()")
    };
    Ok(format!(
      "{base}/socket?ApiKey={}&deviceId={}",
      encode(login.token.expose()),
      encode(&self.identity.device_id)
    ))
  }

  pub fn user_agent() -> String {
    format!("{CLIENT_NAME}/{CLIENT_VERSION}")
  }
}

fn encode(value: &str) -> String {
  utf8_percent_encode(value, QUERY).to_string()
}

/// Item IDs are GUIDs, with or without dashes.
fn check_id(what: &str, id: &str) -> Result<(), JellyfinError> {
  if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
    return Err(JellyfinError::InvalidArgument(format!(
      "{what} {id:?} is not a Jellyfin ID"
    )));
  }
  Ok(())
}

/// Values inside the `Authorization` header can't contain quotes, commas,
/// `=` or control characters, or the server would split them wrongly.
pub fn check_header_value(what: &str, value: &str) -> Result<(), JellyfinError> {
  if value.trim().is_empty()
    || value.len() > 100
    || value
      .chars()
      .any(|c| c.is_control() || matches!(c, '"' | ',' | '='))
  {
    return Err(JellyfinError::InvalidArgument(format!(
      "the {what} {value:?} must be 1-100 characters without quotes, commas, '=' or control characters"
    )));
  }
  Ok(())
}

fn truncate(text: &str, max: usize) -> String {
  if text.len() <= max {
    return text.to_owned();
  }
  let mut end = max;
  while !text.is_char_boundary(end) {
    end -= 1;
  }
  format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
  };

  use super::*;

  /// One captured request: request line, headers and body.
  #[derive(Debug, Clone)]
  struct Captured {
    head: String,
    body: String,
  }

  impl Captured {
    fn line(&self) -> &str {
      self.head.lines().next().unwrap_or_default()
    }

    fn header(&self, name: &str) -> Option<&str> {
      self.head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
      })
    }
  }

  struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
  }

  /// Answers each connection with the next `(status, body)`.
  async fn fake(responses: Vec<(u16, &'static str)>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    tokio::spawn(async move {
      for (status, body) in responses {
        let Ok((mut socket, _)) = listener.accept().await else {
          return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
          let n = socket.read(&mut chunk).await.unwrap_or(0);
          if n == 0 {
            break buf.len();
          }
          buf.extend_from_slice(&chunk[..n]);
          if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
          }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let length = head
          .lines()
          .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
              .then(|| v.trim().parse::<usize>().ok())?
          })
          .unwrap_or(0);
        while buf.len() < head_end + length {
          let n = socket.read(&mut chunk).await.unwrap_or(0);
          if n == 0 {
            break;
          }
          buf.extend_from_slice(&chunk[..n]);
        }
        let body_text = String::from_utf8_lossy(&buf[head_end..]).to_string();
        log.lock().await.push(Captured {
          head,
          body: body_text,
        });
        let reply = format!(
          "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
        let _ = socket.shutdown().await;
      }
    });
    Fake {
      url: format!("http://{addr}"),
      requests,
    }
  }

  fn identity() -> Identity {
    Identity {
      device_name: "JRiver (HTPC)".to_owned(),
      device_id: "jriver-cast-bridge-0001".to_owned(),
    }
  }

  fn login() -> Login {
    Login {
      user_id: "e0a04044ee2d48ce8091f8edf96bfa12".to_owned(),
      user_name: "testuser".to_owned(),
      token: AccessToken::new("secret-token"),
    }
  }

  fn client(url: &str, login: Option<Login>) -> JellyfinClient {
    JellyfinClient::new(url, identity(), login, Duration::from_secs(2)).unwrap()
  }

  fn report() -> ProgressReport {
    ProgressReport {
      item_id: "fc6b41643b6a10ebf8c0ef947a2d06c8".to_owned(),
      media_source_id: Some("fc6b41643b6a10ebf8c0ef947a2d06c8".to_owned()),
      play_session_id: "session-1".to_owned(),
      position_ticks: Ticks(12_000_000_000),
      is_paused: false,
      is_muted: false,
      can_seek: true,
      play_method: PlayMethod::DirectPlay,
      audio_stream_index: Some(2),
      subtitle_stream_index: Some(-1),
    }
  }

  #[tokio::test]
  async fn login_sends_device_identity_and_returns_the_token() {
    let fake = fake(vec![(
      200,
      r#"{"User":{"Id":"e0a04044ee2d48ce8091f8edf96bfa12","Name":"testuser"},"AccessToken":"abc123","ServerId":"s"}"#,
    )])
    .await;
    let result = client(&fake.url, None)
      .login("testuser", "pw\"x")
      .await
      .unwrap();
    assert_eq!(result.user_name, "testuser");
    assert_eq!(result.token.expose(), "abc123");
    let requests = fake.requests.lock().await;
    let request = &requests[0];
    assert_eq!(request.line(), "POST /Users/AuthenticateByName HTTP/1.1");
    let auth = request.header("authorization").unwrap();
    assert!(auth.starts_with(r#"MediaBrowser Client="jriver-cast-bridge", Device="JRiver (HTPC)", DeviceId="jriver-cast-bridge-0001""#));
    assert!(!auth.contains("Token="));
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["Username"], "testuser");
    assert_eq!(body["Pw"], "pw\"x");
  }

  #[tokio::test]
  async fn wrong_password_is_reported_as_such() {
    let fake = fake(vec![(401, "")]).await;
    let err = client(&fake.url, None)
      .login("testuser", "nope")
      .await
      .unwrap_err();
    assert!(matches!(err, JellyfinError::LoginRejected { .. }), "{err}");
    assert!(!err.is_retryable());
  }

  #[tokio::test]
  async fn register_posts_video_only_capabilities_with_the_token() {
    let fake = fake(vec![(204, "")]).await;
    client(&fake.url, Some(login())).register().await.unwrap();
    let requests = fake.requests.lock().await;
    let request = &requests[0];
    assert_eq!(request.line(), "POST /Sessions/Capabilities/Full HTTP/1.1");
    assert!(request
      .header("authorization")
      .unwrap()
      .ends_with(r#"Token="secret-token""#));
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["PlayableMediaTypes"], serde_json::json!(["Video"]));
    assert_eq!(
      body["SupportedCommands"],
      serde_json::json!([
        "Play",
        "PlayState",
        "PlayMediaSource",
        "SetAudioStreamIndex",
        "SetSubtitleStreamIndex"
      ])
    );
    assert_eq!(body["SupportsMediaControl"], true);
    assert!(body.get("DeviceProfile").is_none());
  }

  #[tokio::test]
  async fn with_jriver_picking_the_tracks_no_track_commands_are_offered() {
    let fake = fake(vec![(204, "")]).await;
    let c = client(&fake.url, Some(login()));
    c.set_track_commands(false);
    c.register().await.unwrap();
    let requests = fake.requests.lock().await;
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
      body["SupportedCommands"],
      serde_json::json!(["Play", "PlayState"])
    );
  }

  #[tokio::test]
  async fn item_lookup_uses_items_with_user_id() {
    let fake = fake(vec![(
      200,
      r#"{"Id":"a05ea8c6650fb5db2c14d3511440ff5a","Name":"Star Trek Into Darkness","Type":"Movie","MediaType":"Video",
         "Path":"/media/movies/Star Trek/X","VideoType":"BluRay","RunTimeTicks":79259840000,
         "MediaSources":[{"Id":"a05ea8c6650fb5db2c14d3511440ff5a","Path":"/media/movies/Star Trek/X","Protocol":"File","VideoType":"BluRay"}],
         "UserData":{"PlaybackPositionTicks":0}}"#,
    )])
    .await;
    let item = client(&fake.url, Some(login()))
      .item("a05ea8c6650fb5db2c14d3511440ff5a")
      .await
      .unwrap();
    assert_eq!(item.video_type.as_deref(), Some("BluRay"));
    assert_eq!(
      item.media_sources.unwrap()[0].protocol.as_deref(),
      Some("File")
    );
    let requests = fake.requests.lock().await;
    assert_eq!(
      requests[0].line(),
      "GET /Items/a05ea8c6650fb5db2c14d3511440ff5a?userId=e0a04044ee2d48ce8091f8edf96bfa12 HTTP/1.1"
    );
  }

  #[tokio::test]
  async fn null_media_sources_parse() {
    let fake = fake(vec![(
      200,
      r#"{"Id":"ab","MediaSources":null,"Path":null}"#,
    )])
    .await;
    let item = client(&fake.url, Some(login())).item("ab").await.unwrap();
    assert!(item.media_sources.is_none());
    assert!(item.path.is_none());
  }

  #[tokio::test]
  async fn rejects_ids_that_are_not_ids() {
    let c = client("http://127.0.0.1:1", Some(login()));
    for bad in ["", "../Users", "ab?x=1", "a b"] {
      assert!(matches!(
        c.item(bad).await,
        Err(JellyfinError::InvalidArgument(_))
      ));
    }
  }

  #[tokio::test]
  async fn reports_go_to_their_endpoints_as_direct_play() {
    let fake = fake(vec![(204, ""), (204, ""), (204, "")]).await;
    let c = client(&fake.url, Some(login()));
    c.report(&Report::Start(report())).await.unwrap();
    c.report(&Report::Progress(ProgressReport {
      is_paused: true,
      audio_stream_index: None,
      ..report()
    }))
    .await
    .unwrap();
    c.report(&Report::Stopped(StopReport {
      item_id: "fc6b41643b6a10ebf8c0ef947a2d06c8".to_owned(),
      media_source_id: None,
      play_session_id: "session-1".to_owned(),
      position_ticks: Ticks(0),
      failed: true,
    }))
    .await
    .unwrap();
    let requests = fake.requests.lock().await;
    assert_eq!(requests[0].line(), "POST /Sessions/Playing HTTP/1.1");
    assert_eq!(
      requests[1].line(),
      "POST /Sessions/Playing/Progress HTTP/1.1"
    );
    assert_eq!(
      requests[2].line(),
      "POST /Sessions/Playing/Stopped HTTP/1.1"
    );
    let start: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(start["PlayMethod"], "DirectPlay");
    assert_eq!(start["PositionTicks"], 12_000_000_000_i64);
    assert_eq!(start["CanSeek"], true);
    assert_eq!(start["IsPaused"], false);
    assert_eq!(start["AudioStreamIndex"], 2);
    assert_eq!(start["SubtitleStreamIndex"], -1);
    let progress: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(progress["IsPaused"], true);
    assert!(progress.get("AudioStreamIndex").is_none());
    let stop: serde_json::Value = serde_json::from_str(&requests[2].body).unwrap();
    assert_eq!(stop["Failed"], true);
    assert_eq!(stop["PositionTicks"], 0);
    assert!(stop.get("MediaSourceId").is_none());
  }

  #[tokio::test]
  async fn expired_token_and_server_errors_are_classified() {
    let fake = fake(vec![(401, ""), (503, "maintenance"), (400, "bad")]).await;
    let c = client(&fake.url, Some(login()));
    let unauthorized = c.register().await.unwrap_err();
    assert!(matches!(unauthorized, JellyfinError::Unauthorized { .. }));
    assert!(!unauthorized.is_retryable());
    let unavailable = c.report(&Report::Progress(report())).await.unwrap_err();
    assert!(unavailable.is_retryable(), "{unavailable}");
    assert!(unavailable.to_string().contains("maintenance"));
    let bad = c.report(&Report::Progress(report())).await.unwrap_err();
    assert!(!bad.is_retryable());
  }

  #[tokio::test]
  async fn unreachable_server_is_retryable_and_hides_the_url() {
    let c = client("http://127.0.0.1:1", Some(login()));
    let err = c.report(&Report::Progress(report())).await.unwrap_err();
    assert!(err.is_retryable());
    assert!(!err.to_string().contains("secret-token"));
  }

  #[test]
  fn websocket_url_carries_token_and_device() {
    let c = client("http://jellyfin.local:8096/", Some(login()));
    assert_eq!(
      c.websocket_url().unwrap(),
      "ws://jellyfin.local:8096/socket?ApiKey=secret-token&deviceId=jriver-cast-bridge-0001"
    );
    let tls = client("https://jf.example/jellyfin", Some(login()));
    assert!(tls
      .websocket_url()
      .unwrap()
      .starts_with("wss://jf.example/jellyfin/socket?"));
  }

  #[test]
  fn debug_never_shows_the_token() {
    let c = client("http://jellyfin.local:8096", Some(login()));
    assert!(!format!("{c:?}").contains("secret-token"));
  }

  #[test]
  fn header_values_are_checked() {
    assert!(check_header_value("device name", "JRiver (HTPC)").is_ok());
    for bad in ["", "  ", "a\"b", "a,b", "a=b", "a\nb"] {
      assert!(check_header_value("device name", bad).is_err(), "{bad:?}");
    }
    assert!(JellyfinClient::new("ftp://x", identity(), None, Duration::from_secs(1)).is_err());
  }

  #[test]
  fn truncates_on_char_boundaries() {
    assert_eq!(truncate("abc", 5), "abc");
    assert_eq!(truncate("åååå", 3), "å…");
  }

  /// The parts of a 10.11 `GET /Sessions` reply the takeover reads (trimmed
  /// from a real one; other fields are ignored).
  #[test]
  fn the_server_s_now_playing_film_is_read_from_its_session() {
    let body = r#"[{
      "PlayState": {"PositionTicks": 17844870000, "CanSeek": true, "IsPaused": false,
                    "MediaSourceId": "78b591b83f6a989a8209aa76e94d0131", "PlayMethod": "DirectPlay"},
      "Id": "2b5c", "UserName": "testuser", "Client": "jriver-cast-bridge",
      "DeviceName": "JRiver (HTPC)", "DeviceId": "jriver-cast-bridge-1",
      "NowPlayingItem": {"Name": "Noble Rot", "Id": "78b591b83f6a989a8209aa76e94d0131",
                         "RunTimeTicks": 27340000000, "Type": "Episode"},
      "SupportsRemoteControl": true
    }]"#;
    let sessions: Vec<SessionSummary> = serde_json::from_str(body).unwrap();
    assert_eq!(
      sessions[0].now_playing(),
      Some(NowPlaying {
        item_id: "78b591b83f6a989a8209aa76e94d0131".to_owned(),
        media_source_id: Some("78b591b83f6a989a8209aa76e94d0131".to_owned()),
        position_ticks: 17_844_870_000,
      })
    );
    // An idle session: no film, and no PlayState position either.
    let idle: Vec<SessionSummary> =
      serde_json::from_str(r#"[{"Id": "2b5c", "PlayState": {"CanSeek": false}}]"#).unwrap();
    assert_eq!(idle[0].now_playing(), None);
    // A film without a position still counts (from 0).
    let no_state: Vec<SessionSummary> =
      serde_json::from_str(r#"[{"NowPlayingItem": {"Id": "abc"}}]"#).unwrap();
    assert_eq!(no_state[0].now_playing().unwrap().position_ticks, 0);
  }
}
