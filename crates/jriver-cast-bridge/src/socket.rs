//! The Jellyfin WebSocket: the cast commands (Play, Playstate,
//! GeneralCommand) arrive here. From JellyPilot's `jellypilot-session`
//! (MIT, Copyright (c) 2025 JellyPilot Contributors; see THIRD_PARTY_NOTICES.md),
//! with its message types and error kept local.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
  connect_async,
  tungstenite::{client::IntoClientRequest, http::header, Message},
};
use tokio_util::sync::CancellationToken;

/// Why the socket couldn't connect (or a message couldn't be read).
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
  #[error("WebSocket error: {0}")]
  WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
  #[error("HTTP error: {0}")]
  Handshake(String),
  #[error("JSON error: {0}")]
  Json(#[from] serde_json::Error),
  #[error("Not connected to server")]
  NotConnected,
}

/// A message from the server's socket.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WsMessage {
  message_type: String,
  #[serde(default)]
  data: Option<serde_json::Value>,
}

/// A Play command (cast) from Jellyfin.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayRequest {
  pub item_ids: Vec<String>,
  pub start_position_ticks: Option<i64>,
  pub play_command: String,
  #[serde(default)]
  pub media_source_id: Option<String>,
  #[serde(default)]
  pub audio_stream_index: Option<i32>,
  #[serde(default)]
  pub subtitle_stream_index: Option<i32>,
  /// Index into `item_ids` of the item to play first (a queue sent from a
  /// list starts part-way through it).
  #[serde(default)]
  pub start_index: Option<i32>,
}

/// A Playstate command (pause, unpause, seek, stop) from Jellyfin.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaystateRequest {
  pub command: String,
  #[serde(default)]
  pub seek_position_ticks: Option<i64>,
}

/// A general command (track changes; volume and others are ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GeneralCommand {
  pub name: String,
  #[serde(default)]
  pub arguments: Option<serde_json::Value>,
}

/// Commands that can be received from Jellyfin.
#[derive(Debug, Clone)]
pub enum JellyfinCommand {
  /// Play media items.
  Play(PlayRequest),
  /// Playstate command (pause, unpause, seek, stop).
  Playstate(PlaystateRequest),
  /// General command (volume, mute, etc.).
  GeneralCommand(GeneralCommand),
}

/// Stream events emitted by the restartable Jellyfin WebSocket command stream.
#[derive(Debug, Clone)]
pub enum JellyfinWebSocketEvent {
  /// Initial socket connection has been established.
  Connected,
  /// The active socket was lost. A reconnect may follow unless shutdown was requested.
  ConnectionLost,
  /// A lost socket has reconnected successfully.
  Reconnected,
  /// A Jellyfin command received from the active socket.
  Command(JellyfinCommand),
}

/// Internal state for the command stream receiver.
struct ChannelState {
  event_tx: Option<mpsc::Sender<JellyfinWebSocketEvent>>,
  event_rx: Option<mpsc::Receiver<JellyfinWebSocketEvent>>,
}

/// WebSocket connection to Jellyfin server.
pub struct JellyfinWebSocket {
  channel: Arc<RwLock<ChannelState>>,
  connected: Arc<RwLock<bool>>,
  cancel_token: Arc<RwLock<Option<CancellationToken>>>,
  task_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
  session_updates: bool,
}

impl JellyfinWebSocket {
  /// Create a new WebSocket handler.
  pub fn new() -> Self {
    let (event_tx, event_rx) = mpsc::channel(32);
    Self {
      session_updates: true,
      channel: Arc::new(RwLock::new(ChannelState {
        event_tx: Some(event_tx),
        event_rx: Some(event_rx),
      })),
      connected: Arc::new(RwLock::new(false)),
      cancel_token: Arc::new(RwLock::new(None)),
      task_handle: Arc::new(RwLock::new(None)),
    }
  }

  /// A handler that does not subscribe to the server's session list
  /// (`SessionsStart`). A cast receiver that never shows other sessions
  /// would otherwise get the whole list pushed every second.
  pub fn without_session_updates() -> Self {
    Self {
      session_updates: false,
      ..Self::new()
    }
  }

  /// Connect to Jellyfin WebSocket and own reconnects until explicit shutdown.
  #[allow(dead_code)]
  pub async fn connect(&self, url: &str) -> Result<(), SocketError> {
    self.connect_with_user_agent(url, None).await
  }

  /// Connect to Jellyfin WebSocket with an optional User-Agent override.
  pub async fn connect_with_user_agent(
    &self,
    url: &str,
    user_agent: Option<&str>,
  ) -> Result<(), SocketError> {
    self.stop_task(false).await;

    let Some(event_tx) = self.channel.read().event_tx.clone() else {
      return Err(SocketError::NotConnected);
    };

    let cancel_token = CancellationToken::new();
    *self.cancel_token.write() = Some(cancel_token.clone());

    let connected = self.connected.clone();
    let url = url.to_string();
    let user_agent = user_agent.map(str::to_string);
    let session_updates = self.session_updates;
    let (initial_tx, initial_rx) = oneshot::channel();

    let handle = tokio::spawn(async move {
      Self::run_command_stream(
        url,
        user_agent,
        session_updates,
        event_tx,
        connected,
        cancel_token,
        Some(initial_tx),
      )
      .await;
    });
    *self.task_handle.write() = Some(handle);

    initial_rx.await.unwrap_or(Err(SocketError::NotConnected))
  }

  async fn run_command_stream(
    url: String,
    user_agent: Option<String>,
    session_updates: bool,
    event_tx: mpsc::Sender<JellyfinWebSocketEvent>,
    connected: Arc<RwLock<bool>>,
    cancel_token: CancellationToken,
    mut initial_tx: Option<oneshot::Sender<Result<(), SocketError>>>,
  ) {
    let mut reconnect_attempt = 0usize;
    let mut has_connected = false;

    loop {
      if cancel_token.is_cancelled() {
        break;
      }

      let request = match Self::connection_request(&url, user_agent.as_deref()) {
        Ok(request) => request,
        Err(error) => {
          *connected.write() = false;
          if let Some(initial_tx) = initial_tx.take() {
            let _ = initial_tx.send(Err(error));
            break;
          }
          log::error!("WebSocket request build failed: {}", error);
          let delay = reconnect_delay(reconnect_attempt);
          reconnect_attempt = reconnect_attempt.saturating_add(1);
          if wait_for_reconnect_delay(delay, &cancel_token).await {
            break;
          }
          continue;
        }
      };

      // A server that accepts the connection but never answers the
      // upgrade must not hang the stream: treat it as a failed attempt.
      let connection = tokio::select! {
        _ = cancel_token.cancelled() => break,
        connection = tokio::time::timeout(handshake_timeout(), connect_async(request)) => {
          connection.map_err(|_| {
              SocketError::Handshake(format!(
                  "WebSocket handshake timed out after {:?}",
                  handshake_timeout()
              ))
          })
          .and_then(|result| result.map_err(SocketError::from))
        }
      };

      let (ws_stream, _) = match connection {
        Ok(connection) => connection,
        Err(error) => {
          *connected.write() = false;
          if let Some(initial_tx) = initial_tx.take() {
            let _ = initial_tx.send(Err(error));
            break;
          }
          log::error!("WebSocket reconnection failed: {}", error);
          let delay = reconnect_delay(reconnect_attempt);
          reconnect_attempt = reconnect_attempt.saturating_add(1);
          if wait_for_reconnect_delay(delay, &cancel_token).await {
            break;
          }
          continue;
        }
      };

      *connected.write() = true;
      reconnect_attempt = 0;
      if has_connected {
        if Self::send_event(
          &event_tx,
          JellyfinWebSocketEvent::Reconnected,
          &cancel_token,
        )
        .await
        {
          break;
        }
      } else {
        has_connected = true;
        if let Some(initial_tx) = initial_tx.take() {
          let _ = initial_tx.send(Ok(()));
        }
        if Self::send_event(&event_tx, JellyfinWebSocketEvent::Connected, &cancel_token).await {
          break;
        }
      }

      let lost = Self::run_socket(ws_stream, session_updates, &event_tx, &cancel_token).await;
      *connected.write() = false;

      if !lost || cancel_token.is_cancelled() {
        break;
      }

      if Self::send_event(
        &event_tx,
        JellyfinWebSocketEvent::ConnectionLost,
        &cancel_token,
      )
      .await
      {
        break;
      }
      let delay = reconnect_delay(reconnect_attempt);
      reconnect_attempt = reconnect_attempt.saturating_add(1);
      log::info!(
        "Attempting WebSocket reconnection in {} seconds (attempt {})",
        delay.as_secs(),
        reconnect_attempt
      );
      if wait_for_reconnect_delay(delay, &cancel_token).await {
        break;
      }
    }

    *connected.write() = false;
  }

  fn connection_request(
    url: &str,
    user_agent: Option<&str>,
  ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SocketError> {
    let mut request = url.into_client_request()?;
    if let Some(user_agent) = user_agent {
      let value = header::HeaderValue::from_str(user_agent)
        .map_err(tokio_tungstenite::tungstenite::Error::from)?;
      request.headers_mut().insert(header::USER_AGENT, value);
    }
    Ok(request)
  }

  async fn run_socket<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    session_updates: bool,
    event_tx: &mpsc::Sender<JellyfinWebSocketEvent>,
    cancel_token: &CancellationToken,
  ) -> bool
  where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
  {
    let (mut write, mut read) = ws_stream.split();

    if session_updates {
      // "1000,1000" tells Jellyfin we are an active remote-control client.
      let session_start = serde_json::json!({
        "MessageType": "SessionsStart",
        "Data": "1000,1000"
      });
      if let Err(e) = write
        .send(Message::Text(session_start.to_string().into()))
        .await
      {
        log::error!("Failed to send SessionsStart: {}", e);
        return true;
      }
    }

    let mut keepalive_interval = tokio::time::interval(Duration::from_secs(30));

    loop {
      tokio::select! {
        _ = cancel_token.cancelled() => {
          log::info!("WebSocket shutdown requested via cancellation");
          let _ = write.close().await;
          return false;
        }
        msg = read.next() => {
          match msg {
            Some(Ok(Message::Text(text))) => {
              if let Err(e) = Self::handle_socket_message(&text, event_tx, cancel_token).await {
                log::error!("Failed to handle WebSocket message: {}", e);
              }
            }
            Some(Ok(Message::Close(_))) => {
              log::info!("WebSocket closed by server");
              return true;
            }
            Some(Err(e)) => {
              log::error!("WebSocket error: {}", e);
              return true;
            }
            None => {
              log::info!("WebSocket stream ended");
              return true;
            }
            _ => {}
          }
        }
        _ = keepalive_interval.tick() => {
          let keepalive = serde_json::json!({
            "MessageType": "KeepAlive"
          });
          if let Err(e) = write.send(Message::Text(keepalive.to_string().into())).await {
            log::error!("Failed to send keepalive: {}", e);
            return true;
          }
        }
      }
    }
  }

  async fn send_event(
    event_tx: &mpsc::Sender<JellyfinWebSocketEvent>,
    event: JellyfinWebSocketEvent,
    cancel_token: &CancellationToken,
  ) -> bool {
    if cancel_token.is_cancelled() {
      return true;
    }

    tokio::select! {
      biased;
      _ = cancel_token.cancelled() => true,
      result = event_tx.send(event) => result.is_err(),
    }
  }

  async fn handle_socket_message(
    text: &str,
    event_tx: &mpsc::Sender<JellyfinWebSocketEvent>,
    cancel_token: &CancellationToken,
  ) -> Result<(), SocketError> {
    if let Some(command) = Self::parse_message(text)? {
      let _ = Self::send_event(
        event_tx,
        JellyfinWebSocketEvent::Command(command),
        cancel_token,
      )
      .await;
    }

    Ok(())
  }

  #[cfg(test)]
  /// Handle incoming WebSocket message.
  async fn handle_message(
    text: &str,
    event_tx: &mpsc::Sender<JellyfinWebSocketEvent>,
  ) -> Result<(), SocketError> {
    if let Some(command) = Self::parse_message(text)? {
      let _ = event_tx
        .send(JellyfinWebSocketEvent::Command(command))
        .await;
    }

    Ok(())
  }

  fn parse_message(text: &str) -> Result<Option<JellyfinCommand>, SocketError> {
    let msg: WsMessage = serde_json::from_str(text)?;

    match msg.message_type.as_str() {
      "Play" => {
        if let Some(data) = msg.data {
          let play_request: PlayRequest = serde_json::from_value(data)?;
          log::info!("Received Play command: {:?}", play_request);
          Ok(Some(JellyfinCommand::Play(play_request)))
        } else {
          Ok(None)
        }
      }
      "Playstate" => {
        if let Some(data) = msg.data {
          let playstate: PlaystateRequest = serde_json::from_value(data)?;
          log::info!("Received Playstate command: {:?}", playstate);
          Ok(Some(JellyfinCommand::Playstate(playstate)))
        } else {
          Ok(None)
        }
      }
      "GeneralCommand" => {
        if let Some(data) = msg.data {
          let command: GeneralCommand = serde_json::from_value(data)?;
          log::info!("Received GeneralCommand: {:?}", command);
          Ok(Some(JellyfinCommand::GeneralCommand(command)))
        } else {
          Ok(None)
        }
      }
      "ForceKeepAlive" | "KeepAlive" => Ok(None),
      _ => {
        log::debug!("Unhandled WebSocket message type: {}", msg.message_type);
        Ok(None)
      }
    }
  }

  async fn stop_task(&self, close_delivery: bool) {
    if let Some(token) = self.cancel_token.write().take() {
      token.cancel();
    }

    let handle = self.task_handle.write().take();
    if let Some(handle) = handle {
      let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    *self.connected.write() = false;

    if close_delivery {
      self.channel.write().event_tx.take();
    }
  }

  /// Disconnect from WebSocket and close command delivery for this stream.
  pub async fn disconnect(&self) {
    self.stop_task(true).await;
  }

  /// Check if connected.
  #[allow(dead_code)]
  pub fn is_connected(&self) -> bool {
    *self.connected.read()
  }

  /// Take the restartable command stream receiver.
  pub fn take_event_receiver(&self) -> Option<mpsc::Receiver<JellyfinWebSocketEvent>> {
    self.channel.write().event_rx.take()
  }
}

fn reconnect_delay(attempt: usize) -> Duration {
  #[cfg(not(test))]
  const RECONNECT_DELAYS: &[u64] = &[1, 2, 5, 10, 30, 60];
  #[cfg(test)]
  const TEST_RECONNECT_DELAYS: &[u64] = &[0, 0, 0, 0, 0, 0];

  #[cfg(test)]
  let delays = TEST_RECONNECT_DELAYS;
  #[cfg(not(test))]
  let delays = RECONNECT_DELAYS;

  Duration::from_secs(delays[attempt.min(delays.len() - 1)])
}

/// Longest wait for the server to answer the WebSocket upgrade.
fn handshake_timeout() -> Duration {
  #[cfg(not(test))]
  const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
  #[cfg(test)]
  const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(300);
  HANDSHAKE_TIMEOUT
}

async fn wait_for_reconnect_delay(delay: Duration, cancel_token: &CancellationToken) -> bool {
  tokio::select! {
    _ = cancel_token.cancelled() => true,
    _ = tokio::time::sleep(delay) => false,
  }
}

impl Default for JellyfinWebSocket {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
  };
  use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

  async fn send_text(stream: &mut WebSocketStream<TcpStream>, value: serde_json::Value) {
    stream
      .send(Message::Text(value.to_string().into()))
      .await
      .expect("send websocket message");
  }

  async fn expect_sessions_start(stream: &mut WebSocketStream<TcpStream>) {
    let Some(Ok(Message::Text(text))) = stream.next().await else {
      panic!("expected SessionsStart");
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(value["MessageType"], "SessionsStart");
    assert_eq!(value["Data"], "1000,1000");
  }

  async fn next_event(rx: &mut mpsc::Receiver<JellyfinWebSocketEvent>) -> JellyfinWebSocketEvent {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("event before timeout")
      .expect("stream still open")
  }

  #[tokio::test]
  async fn connect_with_user_agent_sends_custom_handshake_header_before_http_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    let expected_user_agent =
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 jriver-cast-bridge/test";
    let (request_tx, request_rx) = oneshot::channel();

    let server = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.expect("accept");
      let mut bytes = Vec::new();
      let mut buffer = [0_u8; 1024];
      loop {
        let count = socket.read(&mut buffer).await.expect("read request");
        if count == 0 {
          break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
          break;
        }
      }
      let request = String::from_utf8_lossy(&bytes).into_owned();
      request_tx.send(request).expect("send captured request");
      socket
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
        .await
        .expect("write response");
    });

    let websocket = JellyfinWebSocket::new();
    let error = websocket
      .connect_with_user_agent(&url, Some(expected_user_agent))
      .await
      .expect_err("server rejects websocket upgrade");
    assert!(
      error.to_string().contains("403"),
      "expected HTTP 403 error, got: {error}"
    );
    let request = tokio::time::timeout(Duration::from_secs(2), request_rx)
      .await
      .expect("captured request before timeout")
      .expect("captured request");
    server.await.expect("server done");
    let user_agent = request.lines().find_map(|line| {
      line
        .split_once(':')
        .filter(|(name, _)| name.eq_ignore_ascii_case(header::USER_AGENT.as_str()))
        .map(|(_, value)| value.trim())
    });
    assert_eq!(user_agent, Some(expected_user_agent));
  }

  #[tokio::test]
  async fn command_delivery_preserves_supported_messages_and_skips_bad_messages() {
    let (event_tx, mut event_rx) = mpsc::channel(8);

    JellyfinWebSocket::handle_message(
      r#"{"MessageType":"Play","Data":{"ItemIds":["item-1"],"PlayCommand":"PlayNow"}}"#,
      &event_tx,
    )
    .await
    .expect("play message handled");
    JellyfinWebSocket::handle_message(
      r#"{"MessageType":"Unsupported","Data":{"Name":"ignored"}}"#,
      &event_tx,
    )
    .await
    .expect("unsupported message skipped");
    assert!(JellyfinWebSocket::handle_message(
      r#"{"MessageType":"Playstate","Data":{"Command":42}}"#,
      &event_tx,
    )
    .await
    .is_err());
    JellyfinWebSocket::handle_message(
      r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#,
      &event_tx,
    )
    .await
    .expect("playstate message handled");
    JellyfinWebSocket::handle_message(
      r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"50"}}}"#,
      &event_tx,
    )
    .await
    .expect("general command handled");

    match next_event(&mut event_rx).await {
      JellyfinWebSocketEvent::Command(JellyfinCommand::Play(request)) => {
        assert_eq!(request.item_ids, ["item-1"]);
        assert_eq!(request.play_command, "PlayNow");
      }
      event => panic!("unexpected event: {event:?}"),
    }
    match next_event(&mut event_rx).await {
      JellyfinWebSocketEvent::Command(JellyfinCommand::Playstate(request)) => {
        assert_eq!(request.command, "Pause");
      }
      event => panic!("unexpected event: {event:?}"),
    }
    match next_event(&mut event_rx).await {
      JellyfinWebSocketEvent::Command(JellyfinCommand::GeneralCommand(command)) => {
        assert_eq!(command.name, "SetVolume");
      }
      event => panic!("unexpected event: {event:?}"),
    }
    assert!(event_rx.try_recv().is_err());
  }

  #[tokio::test]
  async fn emby_remote_control_commands_decode_to_provider_neutral_commands() {
    let (event_tx, mut event_rx) = mpsc::channel(8);

    for message in [
      r#"{"MessageType":"Play","Data":{"ItemIds":["movie-1"],"PlayCommand":"PlayNow","StartPositionTicks":50000000,"AudioStreamIndex":1,"SubtitleStreamIndex":2}}"#,
      r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#,
      r#"{"MessageType":"Playstate","Data":{"Command":"Seek","SeekPositionTicks":1200000000}}"#,
      r#"{"MessageType":"Playstate","Data":{"Command":"Stop"}}"#,
      r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"65"}}}"#,
      r#"{"MessageType":"GeneralCommand","Data":{"Name":"ToggleMute"}}"#,
      r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetAudioStreamIndex","Arguments":{"Index":"1"}}}"#,
      r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetSubtitleStreamIndex","Arguments":{"Index":"2"}}}"#,
    ] {
      JellyfinWebSocket::handle_message(message, &event_tx)
        .await
        .expect("Emby command should decode");
    }

    match next_event(&mut event_rx).await {
      JellyfinWebSocketEvent::Command(JellyfinCommand::Play(request)) => {
        assert_eq!(request.item_ids, ["movie-1"]);
        assert_eq!(request.play_command, "PlayNow");
        assert_eq!(request.start_position_ticks, Some(50_000_000));
        assert_eq!(request.audio_stream_index, Some(1));
        assert_eq!(request.subtitle_stream_index, Some(2));
      }
      event => panic!("unexpected event: {event:?}"),
    }

    for expected in ["Pause", "Seek", "Stop"] {
      match next_event(&mut event_rx).await {
        JellyfinWebSocketEvent::Command(JellyfinCommand::Playstate(request)) => {
          assert_eq!(request.command, expected);
        }
        event => panic!("unexpected event: {event:?}"),
      }
    }

    for expected in [
      "SetVolume",
      "ToggleMute",
      "SetAudioStreamIndex",
      "SetSubtitleStreamIndex",
    ] {
      match next_event(&mut event_rx).await {
        JellyfinWebSocketEvent::Command(JellyfinCommand::GeneralCommand(command)) => {
          assert_eq!(command.name, expected);
        }
        event => panic!("unexpected event: {event:?}"),
      }
    }

    assert!(event_rx.try_recv().is_err());
  }

  #[tokio::test]
  async fn command_stream_reconnects_and_delivers_lifecycle_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));

    let server = tokio::spawn(async move {
      let (first_socket, _) = listener.accept().await.expect("first accept");
      let mut first = accept_async(first_socket).await.expect("first websocket");
      expect_sessions_start(&mut first).await;
      send_text(
        &mut first,
        serde_json::json!({
          "MessageType": "Play",
          "Data": { "ItemIds": ["item-1"], "PlayCommand": "PlayNow" }
        }),
      )
      .await;
      first.close(None).await.expect("close first");

      let (second_socket, _) = listener.accept().await.expect("second accept");
      let mut second = accept_async(second_socket).await.expect("second websocket");
      expect_sessions_start(&mut second).await;
      send_text(
        &mut second,
        serde_json::json!({
          "MessageType": "Playstate",
          "Data": { "Command": "Pause" }
        }),
      )
      .await;
      send_text(
        &mut second,
        serde_json::json!({
          "MessageType": "GeneralCommand",
          "Data": { "Name": "Mute" }
        }),
      )
      .await;
      second.next().await;
    });

    let websocket = JellyfinWebSocket::new();
    let mut rx = websocket.take_event_receiver().expect("event receiver");
    websocket.connect(&url).await.expect("initial connect");

    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Connected
    ));
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Command(JellyfinCommand::Play(_))
    ));
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::ConnectionLost
    ));
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Reconnected
    ));
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Command(JellyfinCommand::Playstate(_))
    ));
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Command(JellyfinCommand::GeneralCommand(_))
    ));

    websocket.disconnect().await;
    server.await.expect("server done");
    assert!(!websocket.is_connected());
  }

  #[tokio::test]
  async fn a_server_that_never_answers_the_upgrade_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
      // Accept the connection, read the request, never answer.
      let (mut socket, _) = listener.accept().await.expect("accept");
      let mut buffer = [0_u8; 1024];
      let _ = socket.read(&mut buffer).await;
      tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let websocket = JellyfinWebSocket::without_session_updates();
    let error = tokio::time::timeout(Duration::from_secs(5), websocket.connect(&url))
      .await
      .expect("connect gives up on its own")
      .expect_err("handshake never completes");
    assert!(error.to_string().contains("timed out"), "{error}");
    server.abort();
  }

  #[tokio::test]
  async fn without_session_updates_skips_sessions_start() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));

    let server = tokio::spawn(async move {
      let (socket, _) = listener.accept().await.expect("accept");
      let mut stream = accept_async(socket).await.expect("websocket");
      let Some(Ok(Message::Text(text))) = stream.next().await else {
        panic!("expected a text message");
      };
      let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
      value["MessageType"].as_str().map(str::to_owned)
    });

    let websocket = JellyfinWebSocket::without_session_updates();
    let mut rx = websocket.take_event_receiver().expect("event receiver");
    websocket.connect(&url).await.expect("connect");
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Connected
    ));

    // The first message is the keep-alive, not SessionsStart.
    let first = tokio::time::timeout(Duration::from_secs(2), server)
      .await
      .expect("server saw a message in time")
      .expect("server done");
    assert_eq!(first.as_deref(), Some("KeepAlive"));
    websocket.disconnect().await;
  }

  #[tokio::test]
  async fn explicit_shutdown_does_not_schedule_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));

    let server = tokio::spawn(async move {
      let (socket, _) = listener.accept().await.expect("accept");
      let mut stream = accept_async(socket).await.expect("websocket");
      expect_sessions_start(&mut stream).await;
      stream.next().await;
    });

    let websocket = JellyfinWebSocket::new();
    let mut rx = websocket.take_event_receiver().expect("event receiver");
    websocket.connect(&url).await.expect("connect");
    assert!(matches!(
      next_event(&mut rx).await,
      JellyfinWebSocketEvent::Connected
    ));

    websocket.disconnect().await;
    server.await.expect("server done");
    assert!(!websocket.is_connected());
    assert!(rx.recv().await.is_none());
  }
}
