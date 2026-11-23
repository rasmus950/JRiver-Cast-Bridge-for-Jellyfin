//! HTTP client for MCWS on localhost.

use std::{
  fmt,
  sync::Mutex,
  time::{Duration, Instant},
};

use log::{debug, warn};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::{
  model::{AliveInfo, PauseAction, PlaybackInfo, UiInfo},
  response::{self, error_detail, Items},
  Function, McwsError,
};

/// Default per-call timeout. MCWS answers locally in well under 100 ms; a
/// call that takes seconds means MC is hung or gone.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The MCC commands the bridge sends, each through its own method (SPEC.md,
/// "JRiver side": minimize after playback, and the two track commands).
const MCC_MINIMIZE_WINDOW: &str = "10014";
const MCC_SET_SUBTITLES: &str = "10052";
const MCC_SET_AUDIO_STREAM: &str = "10053";

/// Everything except RFC 3986 unreserved characters is percent-encoded, so
/// spaces become `%20` (not `+`) and `&`, `#`, `+`, `%`, `;` and non-ASCII
/// UTF-8 bytes survive the query string.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
  .remove(b'-')
  .remove(b'.')
  .remove(b'_')
  .remove(b'~');

/// Media Network credentials, needed only when MC's Media Network
/// authentication is on.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
  pub username: String,
  pub password: String,
}

impl fmt::Debug for Credentials {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Credentials")
      .field("username", &self.username)
      .field("password", &"<redacted>")
      .finish()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McwsConfig {
  /// MCWS base URL, e.g. `http://localhost:52199/MCWS/v1/`.
  pub base_url: String,
  /// Zone ID to target. 0 is MC's local "Player" zone. Never -1.
  pub zone_id: i32,
  /// Per-call timeout (connect + response).
  pub timeout: Duration,
  pub credentials: Option<Credentials>,
}

impl Default for McwsConfig {
  fn default() -> Self {
    Self {
      base_url: "http://localhost:52199/MCWS/v1/".to_owned(),
      zone_id: 0,
      timeout: DEFAULT_TIMEOUT,
      credentials: None,
    }
  }
}

/// MCWS client. Cheap to share behind an `Arc`; reuses one HTTP connection
/// pool for every call.
pub struct McwsClient {
  http: reqwest::Client,
  base: String,
  zone_id: String,
  timeout: Duration,
  credentials: Option<Credentials>,
  token: Mutex<Option<String>>,
}

impl fmt::Debug for McwsClient {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("McwsClient")
      .field("base", &self.base)
      .field("zone_id", &self.zone_id)
      .field("timeout", &self.timeout)
      .field("credentials", &self.credentials)
      .finish_non_exhaustive()
  }
}

impl McwsClient {
  pub fn new(config: McwsConfig) -> Result<Self, reqwest::Error> {
    let http = reqwest::Client::builder()
      .timeout(config.timeout)
      .connect_timeout(config.timeout)
      // MCWS is on localhost; never route it through a system proxy.
      .no_proxy()
      .build()?;
    let mut base = config.base_url.trim().to_owned();
    if !base.ends_with('/') {
      base.push('/');
    }
    Ok(Self {
      http,
      base,
      zone_id: config.zone_id.to_string(),
      timeout: config.timeout,
      credentials: config.credentials,
      token: Mutex::new(None),
    })
  }

  /// `Alive`: MC is up and answering.
  pub async fn alive(&self) -> Result<AliveInfo, McwsError> {
    let items = self.call(Function::Alive, &[]).await?;
    Ok(AliveInfo::from_items(&items))
  }

  /// `Playback/Info` for the configured zone, including the file path.
  pub async fn info(&self) -> Result<PlaybackInfo, McwsError> {
    let items = self
      .call(Function::PlaybackInfo, &[("Fields", "Filename")])
      .await?;
    PlaybackInfo::from_items(&items)
  }

  /// `UserInterface/Info` (read-only).
  pub async fn ui_info(&self) -> Result<UiInfo, McwsError> {
    let items = self.call(Function::UserInterfaceInfo, &[]).await?;
    UiInfo::from_items(&items)
  }

  /// `Playback/PlayByFilename` with one file. MC appends it to Playing Now
  /// and starts it; call [`Self::clear_playlist`] first to replace.
  pub async fn play_by_filename(&self, path: &str) -> Result<(), McwsError> {
    let function = Function::PlayByFilename;
    if path.trim().is_empty() {
      return Err(McwsError::InvalidArgument {
        function,
        reason: "empty filename".to_owned(),
      });
    }
    if path.contains('|') {
      return Err(McwsError::InvalidArgument {
        function,
        reason: format!("filename contains the MCWS list delimiter '|': {path}"),
      });
    }
    self.call(function, &[("Filenames", path)]).await.map(drop)
  }

  /// `Playback/ClearPlaylist`: stops and clears Playing Now. Asynchronous in
  /// MC; poll [`Self::info`] until `playing_now_tracks` is 0.
  pub async fn clear_playlist(&self) -> Result<(), McwsError> {
    self.call(Function::ClearPlaylist, &[]).await.map(drop)
  }

  /// `Playback/Position` without a value: the current position in ms.
  pub async fn position(&self) -> Result<u64, McwsError> {
    let items = self.call(Function::Position, &[]).await?;
    position_from(&items)
  }

  /// `Playback/Position?Position=<ms>`: absolute seek. Returns the position
  /// MC reports after the change.
  pub async fn seek(&self, position_ms: u64) -> Result<u64, McwsError> {
    let position = position_ms.to_string();
    let items = self
      .call(Function::Position, &[("Position", &position)])
      .await?;
    position_from(&items)
  }

  /// `Playback/Pause?State=<1|0|-1>`.
  pub async fn pause(&self, action: PauseAction) -> Result<(), McwsError> {
    self
      .call(Function::Pause, &[("State", action.param())])
      .await
      .map(drop)
  }

  /// `Playback/Stop`.
  pub async fn stop(&self) -> Result<(), McwsError> {
    self.call(Function::Stop, &[]).await.map(drop)
  }

  /// `Control/MCC?Command=10014&Parameter=1` (`MCC_MINIMIZE_WINDOW`).
  ///
  /// JRiver documents parameter 1 as *toggling* minimize and restore, so
  /// send it only when MC is known not to be minimized (the bridge sends it
  /// only right after MC left Display view).
  pub async fn minimize_window(&self) -> Result<(), McwsError> {
    self
      .call(
        Function::MinimizeWindow,
        &[("Command", MCC_MINIMIZE_WINDOW), ("Parameter", "1")],
      )
      .await
      .map(drop)
  }

  /// `Control/MCC?Command=10053&Parameter=<n>` (`MCC_SET_AUDIO_STREAM`):
  /// play audio track `n` of the playing file (-1 next, -2 previous).
  pub async fn select_audio_stream(&self, track: i32) -> Result<(), McwsError> {
    let track = track.to_string();
    self
      .call(
        Function::SelectAudioStream,
        &[("Command", MCC_SET_AUDIO_STREAM), ("Parameter", &track)],
      )
      .await
      .map(drop)
  }

  /// `Control/MCC?Command=10052&Parameter=<n>` (`MCC_SET_SUBTITLES`): show
  /// subtitle track `n` of the playing file (-1 next, -2 previous).
  pub async fn select_subtitles(&self, track: i32) -> Result<(), McwsError> {
    let track = track.to_string();
    self
      .call(
        Function::SelectSubtitles,
        &[("Command", MCC_SET_SUBTITLES), ("Parameter", &track)],
      )
      .await
      .map(drop)
  }

  async fn call(&self, function: Function, params: &[(&str, &str)]) -> Result<Items, McwsError> {
    let use_token = function.needs_token() && self.credentials.is_some();
    let token = if use_token {
      Some(self.token_or_authenticate(function).await?)
    } else {
      None
    };
    match self.send(function, params, token.as_deref()).await {
      Err(McwsError::Status { status: 401, .. }) if use_token => {
        // The token expired (MC restarted). Re-authenticate once.
        warn!("MCWS {function} rejected the token; authenticating again");
        self.set_token(None);
        let token = self.authenticate(function).await?;
        self.send(function, params, Some(&token)).await
      }
      Err(McwsError::Status { status: 401, .. }) => Err(McwsError::Auth {
        function,
        reason: "Media Network authentication is on but no MCWS credentials are configured"
          .to_owned(),
      }),
      other => other,
    }
  }

  async fn send(
    &self,
    function: Function,
    params: &[(&str, &str)],
    token: Option<&str>,
  ) -> Result<Items, McwsError> {
    let url = self.url(function, params, token);
    debug!("MCWS -> {function} {}", describe(params));
    let started = Instant::now();
    let response = self
      .http
      .get(url)
      .send()
      .await
      .map_err(|err| self.transport_error(function, err))?;
    let status = response.status();
    let body = response
      .text()
      .await
      .map_err(|err| self.transport_error(function, err))?;
    debug!(
      "MCWS <- {function} HTTP {} in {} ms",
      status.as_u16(),
      started.elapsed().as_millis()
    );
    if !status.is_success() {
      return Err(McwsError::Status {
        function,
        status: status.as_u16(),
        detail: error_detail(function, &body),
      });
    }
    response::parse(function, &body)
  }

  async fn token_or_authenticate(&self, function: Function) -> Result<String, McwsError> {
    if let Some(token) = self.current_token() {
      return Ok(token);
    }
    self.authenticate(function).await
  }

  async fn authenticate(&self, for_function: Function) -> Result<String, McwsError> {
    let function = Function::Authenticate;
    let Some(credentials) = &self.credentials else {
      return Err(McwsError::Auth {
        function: for_function,
        reason: "no MCWS credentials configured".to_owned(),
      });
    };
    debug!("MCWS -> {function} (user {})", credentials.username);
    let response = self
      .http
      .get(self.url(function, &[], None))
      .basic_auth(&credentials.username, Some(&credentials.password))
      .send()
      .await
      .map_err(|err| self.transport_error(function, err))?;
    let status = response.status();
    let body = response
      .text()
      .await
      .map_err(|err| self.transport_error(function, err))?;
    debug!("MCWS <- {function} HTTP {}", status.as_u16());
    if status.as_u16() == 401 {
      return Err(McwsError::Auth {
        function: for_function,
        reason: format!(
          "MC rejected the credentials for user {}",
          credentials.username
        ),
      });
    }
    if !status.is_success() {
      return Err(McwsError::Status {
        function,
        status: status.as_u16(),
        detail: error_detail(function, &body),
      });
    }
    let items = response::parse(function, &body)?;
    let token = items
      .get("Token")
      .filter(|token| !token.is_empty())
      .ok_or_else(|| McwsError::Parse {
        function,
        reason: "Authenticate returned no Token".to_owned(),
      })?
      .to_owned();
    self.set_token(Some(token.clone()));
    Ok(token)
  }

  fn current_token(&self) -> Option<String> {
    self
      .token
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .clone()
  }

  fn set_token(&self, token: Option<String>) {
    *self
      .token
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner) = token;
  }

  /// Full request URL. Holds the token, so it is never logged; see
  /// [`describe`] for the loggable form.
  fn url(&self, function: Function, params: &[(&str, &str)], token: Option<&str>) -> String {
    build_url(&self.base, function, &self.zone_id, params, token)
  }

  fn transport_error(&self, function: Function, err: reqwest::Error) -> McwsError {
    // The URL can carry the token: drop it from the error before it can be
    // logged.
    let err = err.without_url();
    if err.is_timeout() {
      McwsError::Timeout {
        function,
        timeout: self.timeout,
      }
    } else {
      McwsError::Unreachable {
        function,
        source: err,
      }
    }
  }
}

fn position_from(items: &Items) -> Result<u64, McwsError> {
  let position = crate::model::required_int(items, Function::Position, "Position")?;
  Ok(u64::try_from(position).unwrap_or(0))
}

pub(crate) fn build_url(
  base: &str,
  function: Function,
  zone_id: &str,
  params: &[(&str, &str)],
  token: Option<&str>,
) -> String {
  let mut url = format!("{base}{}", function.path());
  let mut pairs: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 3);
  if function.zoned() {
    pairs.push(("Zone", zone_id));
    pairs.push(("ZoneType", "ID"));
  }
  pairs.extend_from_slice(params);
  if let Some(token) = token {
    pairs.push(("Token", token));
  }
  for (index, (key, value)) in pairs.iter().enumerate() {
    url.push(if index == 0 { '?' } else { '&' });
    url.push_str(key);
    url.push('=');
    url.extend(utf8_percent_encode(value, COMPONENT));
  }
  url
}

/// Loggable description of the call parameters (never includes the token).
fn describe(params: &[(&str, &str)]) -> String {
  params
    .iter()
    .map(|(key, value)| format!("{key}={value}"))
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex as AsyncMutex,
  };

  use super::*;
  use crate::PlayState;

  const BASE: &str = "http://localhost:52199/MCWS/v1/";

  #[test]
  fn zoned_calls_pin_the_zone_and_encode_paths() {
    let url = build_url(
      BASE,
      Function::PlayByFilename,
      "0",
      &[(
        "Filenames",
        r"\\nas\movies\Avatar - Fire and Ash (2025)\A&B #1 + 50% [Ben The Men];x é.mkv",
      )],
      None,
    );
    assert_eq!(
      url,
      "http://localhost:52199/MCWS/v1/Playback/PlayByFilename?Zone=0&ZoneType=ID&Filenames=\
       %5C%5Cnas%5Cmovies%5CAvatar%20-%20Fire%20and%20Ash%20%282025%29%5C\
       A%26B%20%231%20%2B%2050%25%20%5BBen%20The%20Men%5D%3Bx%20%C3%A9.mkv"
    );
  }

  #[test]
  fn unzoned_calls_have_no_zone_and_token_goes_last() {
    assert_eq!(
      build_url(BASE, Function::Alive, "0", &[], None),
      "http://localhost:52199/MCWS/v1/Alive"
    );
    assert_eq!(
      build_url(BASE, Function::Stop, "10001", &[], Some("ab+c")),
      "http://localhost:52199/MCWS/v1/Playback/Stop?Zone=10001&ZoneType=ID&Token=ab%2Bc"
    );
    assert_eq!(
      build_url(BASE, Function::UserInterfaceInfo, "0", &[], None),
      "http://localhost:52199/MCWS/v1/UserInterface/Info"
    );
  }

  #[tokio::test]
  async fn minimize_sends_only_mcc_10014_to_the_configured_zone() {
    let fake = fake_mcws(vec![(200, r#"<Response Status="OK"></Response>"#)]).await;
    client(&fake.base, None).minimize_window().await.unwrap();
    let requests = fake.requests.lock().await;
    assert!(
      requests[0].starts_with(
        "GET /MCWS/v1/Control/MCC?Zone=0&ZoneType=ID&Command=10014&Parameter=1 HTTP/1.1"
      ),
      "{}",
      requests[0]
    );
  }

  #[tokio::test]
  async fn track_selection_sends_only_mcc_10053_and_10052_to_the_zone() {
    let ok = r#"<Response Status="OK"></Response>"#;
    let fake = fake_mcws(vec![(200, ok), (200, ok)]).await;
    let mcws = client(&fake.base, None);
    mcws.select_audio_stream(2).await.unwrap();
    mcws.select_subtitles(-1).await.unwrap();
    let requests = fake.requests.lock().await;
    assert!(
      requests[0].starts_with(
        "GET /MCWS/v1/Control/MCC?Zone=0&ZoneType=ID&Command=10053&Parameter=2 HTTP/1.1"
      ),
      "{}",
      requests[0]
    );
    assert!(
      requests[1].starts_with(
        "GET /MCWS/v1/Control/MCC?Zone=0&ZoneType=ID&Command=10052&Parameter=-1 HTTP/1.1"
      ),
      "{}",
      requests[1]
    );
  }

  #[test]
  fn describe_never_contains_token() {
    assert_eq!(describe(&[("Position", "1000")]), "Position=1000");
  }

  #[test]
  fn credentials_debug_is_redacted() {
    let creds = Credentials {
      username: "u".into(),
      password: "hunter2".into(),
    };
    assert!(!format!("{creds:?}").contains("hunter2"));
  }

  /// Minimal scripted MCWS: answers each request with the next canned
  /// response and records the request lines.
  struct FakeMcws {
    base: String,
    requests: Arc<AsyncMutex<Vec<String>>>,
  }

  async fn fake_mcws(responses: Vec<(u16, &'static str)>) -> FakeMcws {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AsyncMutex::new(Vec::new()));
    let log = requests.clone();
    tokio::spawn(async move {
      for (status, body) in responses {
        let Ok((mut socket, _)) = listener.accept().await else {
          return;
        };
        let mut buf = vec![0u8; 8192];
        let mut read = 0;
        loop {
          let n = socket.read(&mut buf[read..]).await.unwrap_or(0);
          read += n;
          if n == 0 || buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
          }
        }
        let request = String::from_utf8_lossy(&buf[..read]).to_string();
        log.lock().await.push(request);
        let reply = format!(
          "HTTP/1.1 {status} X\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
        let _ = socket.shutdown().await;
      }
    });
    FakeMcws {
      base: format!("http://{addr}/MCWS/v1/"),
      requests,
    }
  }

  fn client(base: &str, credentials: Option<Credentials>) -> McwsClient {
    McwsClient::new(McwsConfig {
      base_url: base.to_owned(),
      zone_id: 0,
      timeout: Duration::from_secs(2),
      credentials,
    })
    .unwrap()
  }

  #[tokio::test]
  async fn info_round_trip() {
    let fake = fake_mcws(vec![(
      200,
      r#"<Response Status="OK"><Item Name="State">1</Item><Item Name="FileKey">4</Item><Item Name="PositionMS">600000</Item><Item Name="DurationMS">7139465</Item><Item Name="PlayingNowTracks">1</Item><Item Name="Filename">\\nas\movies\a.mkv</Item></Response>"#,
    )])
    .await;
    let info = client(&fake.base, None).info().await.unwrap();
    assert_eq!(info.state, PlayState::Paused);
    assert_eq!(info.position_ms, 600_000);
    let requests = fake.requests.lock().await;
    assert!(
      requests[0].starts_with("GET /MCWS/v1/Playback/Info?Zone=0&ZoneType=ID&Fields=Filename "),
      "{}",
      requests[0]
    );
  }

  #[tokio::test]
  async fn http_500_becomes_status_error() {
    // MC answers PlayByFilename on a bare folder with HTTP 500.
    let fake = fake_mcws(vec![(500, "")]).await;
    let err = client(&fake.base, None)
      .play_by_filename(r"\\nas\movies\Disc")
      .await
      .unwrap_err();
    assert!(
      matches!(
        err,
        McwsError::Status {
          function: Function::PlayByFilename,
          status: 500,
          detail: None
        }
      ),
      "{err:?}"
    );
  }

  #[tokio::test]
  async fn failure_body_becomes_failure_error() {
    let fake = fake_mcws(vec![(
      200,
      r#"<Response Status="Failure"><Item Name="Information">nope</Item></Response>"#,
    )])
    .await;
    let err = client(&fake.base, None).stop().await.unwrap_err();
    assert!(matches!(err, McwsError::Failure { .. }), "{err:?}");
  }

  #[tokio::test]
  async fn unauthenticated_401_explains_missing_credentials() {
    let fake = fake_mcws(vec![(401, "")]).await;
    let err = client(&fake.base, None).info().await.unwrap_err();
    assert!(matches!(err, McwsError::Auth { .. }), "{err:?}");
  }

  #[tokio::test]
  async fn authenticates_then_sends_token_and_reauthenticates_on_401() {
    let fake = fake_mcws(vec![
      (
        200,
        r#"<Response Status="OK"><Item Name="Token">tok1</Item></Response>"#,
      ),
      (200, r#"<Response Status="OK"/>"#),
      (401, ""),
      (
        200,
        r#"<Response Status="OK"><Item Name="Token">tok2</Item></Response>"#,
      ),
      (200, r#"<Response Status="OK"/>"#),
    ])
    .await;
    let client = client(
      &fake.base,
      Some(Credentials {
        username: "user".into(),
        password: "pw".into(),
      }),
    );
    client.stop().await.unwrap();
    client.stop().await.unwrap();
    let requests = fake.requests.lock().await;
    assert!(requests[0].starts_with("GET /MCWS/v1/Authenticate "));
    assert!(requests[0]
      .to_ascii_lowercase()
      .contains("authorization: basic "));
    assert!(requests[1].contains("&Token=tok1 "), "{}", requests[1]);
    assert!(requests[2].contains("&Token=tok1 "));
    assert!(requests[3].starts_with("GET /MCWS/v1/Authenticate "));
    assert!(requests[4].contains("&Token=tok2 "), "{}", requests[4]);
  }

  #[tokio::test]
  async fn refuses_pipe_and_empty_filenames_without_calling_mc() {
    let client = client("http://127.0.0.1:9/MCWS/v1/", None);
    assert!(matches!(
      client.play_by_filename(r"\\nas\a|b.mkv").await,
      Err(McwsError::InvalidArgument { .. })
    ));
    assert!(matches!(
      client.play_by_filename("  ").await,
      Err(McwsError::InvalidArgument { .. })
    ));
  }

  #[tokio::test]
  async fn unreachable_mc_is_reported_as_unreachable() {
    // Bind then drop to get a port nothing listens on.
    let port = {
      let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      listener.local_addr().unwrap().port()
    };
    let err = client(&format!("http://127.0.0.1:{port}/MCWS/v1/"), None)
      .alive()
      .await
      .unwrap_err();
    assert!(err.is_unreachable(), "{err:?}");
  }

  #[tokio::test]
  async fn hung_mc_times_out() {
    // Accepts the connection but never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let held = tokio::spawn(async move {
      let (socket, _) = listener.accept().await.unwrap();
      tokio::time::sleep(Duration::from_secs(10)).await;
      drop(socket);
    });
    let client = McwsClient::new(McwsConfig {
      base_url: format!("http://{addr}/MCWS/v1/"),
      zone_id: 0,
      timeout: Duration::from_millis(300),
      credentials: None,
    })
    .unwrap();
    let err = client.info().await.unwrap_err();
    assert!(matches!(err, McwsError::Timeout { .. }), "{err:?}");
    assert!(err.is_unreachable());
    held.abort();
  }
}
