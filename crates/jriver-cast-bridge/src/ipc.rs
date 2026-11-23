//! A local named pipe the running bridge listens on (SPEC.md: graceful quit
//! for the installer, and a new login picked up without a restart).
//!
//! One request per connection, one line each way:
//! - `quit`: as the tray's Quit. The bridge answers `ok`, then keeps the
//!   pipe open until it has stopped, so the client can wait for the close.
//! - `login <account>`: a login was saved for that account; a bridge using
//!   it reconnects with the new login.
//! - `reload`: the settings window saved the config; the bridge reloads it.
//!
//! The pipe is per Windows user and refuses remote clients.

use std::{io, time::Duration};

use log::{debug, info, warn};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  sync::{mpsc, watch},
  task::JoinHandle,
};

use crate::controller::Event;

/// How long either side waits for the other's line.
const LINE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
  Quit,
  /// A login was saved for `account` (`<profile>@<server URL>`). A bridge
  /// using another account (a test profile) ignores it.
  Login {
    account: String,
  },
  /// The config was saved: reload it.
  Reload,
}

impl Request {
  fn line(&self) -> String {
    match self {
      Self::Quit => "quit".to_owned(),
      Self::Login { account } => format!("login {account}"),
      Self::Reload => "reload".to_owned(),
    }
  }

  fn parse(line: &str) -> Option<Self> {
    let line = line.trim();
    if line == "quit" {
      return Some(Self::Quit);
    }
    if line == "reload" {
      return Some(Self::Reload);
    }
    let account = line.strip_prefix("login ")?.trim();
    (!account.is_empty()).then(|| Self::Login {
      account: account.to_owned(),
    })
  }
}

/// `\\.\pipe\jriver-cast-bridge-<Windows user>`.
pub fn pipe_name() -> String {
  let user: String = std::env::var("USERNAME")
    .unwrap_or_default()
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    .collect();
  let user = if user.is_empty() {
    "user".to_owned()
  } else {
    user
  };
  format!(r"\\.\pipe\jriver-cast-bridge-{user}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
  /// No bridge is listening.
  NotRunning,
  /// The bridge took the request (and, for a quit with a wait, has stopped).
  Done,
}

/// The listening pipe. Dropping it stops listening and closes the pipes
/// held open for `quit` clients, which tells them the bridge has stopped.
#[derive(Debug)]
pub struct PipeServer {
  accept: JoinHandle<()>,
  _closing: watch::Sender<()>,
}

impl Drop for PipeServer {
  fn drop(&mut self) {
    self.accept.abort();
  }
}

/// What the server needs to answer requests.
#[derive(Debug, Clone)]
struct Context {
  events: mpsc::Sender<Event>,
  /// This bridge's saved-login account (it follows the config across
  /// reloads); logins for others are ignored.
  account: watch::Receiver<String>,
  closed: watch::Receiver<()>,
}

#[cfg(windows)]
mod imp {
  use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
    time::Instant,
  };

  use super::*;

  /// `ERROR_PIPE_BUSY`: every instance is taken for a moment.
  const PIPE_BUSY: i32 = 231;

  /// Listen for requests while the returned server lives. Fails if another
  /// bridge already owns the pipe. Made once per bridge: it outlives
  /// config reloads (`account` follows the config).
  pub fn serve(
    events: mpsc::Sender<Event>,
    account: watch::Receiver<String>,
  ) -> io::Result<PipeServer> {
    serve_on(pipe_name(), events, account)
  }

  pub(super) fn serve_on(
    name: String,
    events: mpsc::Sender<Event>,
    account: watch::Receiver<String>,
  ) -> io::Result<PipeServer> {
    let first = ServerOptions::new()
      .first_pipe_instance(true)
      .reject_remote_clients(true)
      .create(&name)?;
    debug!("listening on {name}");
    let (closing, closed) = watch::channel(());
    let context = Context {
      events,
      account,
      closed,
    };
    Ok(PipeServer {
      accept: tokio::spawn(accept_loop(name, first, context)),
      _closing: closing,
    })
  }

  async fn accept_loop(name: String, mut server: NamedPipeServer, context: Context) {
    loop {
      if let Err(err) = server.connect().await {
        warn!("local pipe: accepting failed: {err}; --quit won't work until restart");
        return;
      }
      let connected = server;
      server = match ServerOptions::new()
        .reject_remote_clients(true)
        .create(&name)
      {
        Ok(next) => next,
        Err(err) => {
          warn!("local pipe: can't listen again: {err}; --quit won't work until restart");
          return;
        }
      };
      tokio::spawn(handle(connected, context.clone()));
    }
  }

  async fn handle(mut pipe: NamedPipeServer, mut context: Context) {
    let mut buf = [0u8; MAX_LINE];
    let n = match tokio::time::timeout(LINE_TIMEOUT, pipe.read(&mut buf)).await {
      Ok(Ok(n)) => n,
      _ => return,
    };
    let line = String::from_utf8_lossy(&buf[..n]).into_owned();
    let Some(request) = Request::parse(&line) else {
      warn!("local pipe: unknown request {:?}", line.trim());
      let _ = pipe.write_all(b"error\n").await;
      return;
    };
    let ours = context.account.borrow().clone();
    let event = match &request {
      Request::Quit => {
        info!("quit requested over the local pipe");
        Some(Event::Shutdown)
      }
      Request::Login { account } if *account == ours => {
        info!("a new login was saved for {account}; reconnecting with it");
        Some(Event::LoginChanged)
      }
      Request::Reload => {
        info!("the settings were saved; reloading the config");
        Some(Event::Reload)
      }
      Request::Login { account } => {
        debug!("a login was saved for {account}; this bridge uses {ours}, so it's ignored");
        None
      }
    };
    let taken = match event {
      Some(event) => context.events.send(event).await.is_ok(),
      None => true,
    };
    let _ = pipe
      .write_all(if taken { b"ok\n" } else { b"error\n" })
      .await;
    if request == Request::Quit && taken {
      // Keep the pipe until the bridge has stopped (the server is dropped):
      // its close tells the client the bridge is gone.
      let _ = context.closed.changed().await;
    }
  }

  pub async fn send(request: &Request, wait_for_exit: Option<Duration>) -> io::Result<Reply> {
    send_on(&pipe_name(), request, wait_for_exit).await
  }

  pub(super) async fn send_on(
    name: &str,
    request: &Request,
    wait_for_exit: Option<Duration>,
  ) -> io::Result<Reply> {
    let busy_until = Instant::now() + LINE_TIMEOUT;
    let mut pipe = loop {
      match ClientOptions::new().open(name) {
        Ok(pipe) => break pipe,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Reply::NotRunning),
        Err(err) if err.raw_os_error() == Some(PIPE_BUSY) && Instant::now() < busy_until => {
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(err) => return Err(err),
      }
    };
    pipe
      .write_all(format!("{}\n", request.line()).as_bytes())
      .await?;
    let mut buf = [0u8; MAX_LINE];
    let n = tokio::time::timeout(LINE_TIMEOUT, pipe.read(&mut buf))
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the bridge didn't answer"))??;
    let reply = String::from_utf8_lossy(&buf[..n]);
    if reply.trim() != "ok" {
      return Err(io::Error::other(format!(
        "the bridge refused {:?}: {:?}",
        request.line(),
        reply.trim()
      )));
    }
    if let Some(wait) = wait_for_exit {
      // Read until the bridge closes the pipe (EOF or a broken pipe).
      let closed = tokio::time::timeout(wait, async {
        loop {
          match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
          }
        }
      })
      .await;
      if closed.is_err() {
        return Err(io::Error::new(
          io::ErrorKind::TimedOut,
          format!("the bridge didn't stop within {wait:?}"),
        ));
      }
    }
    Ok(Reply::Done)
  }
}

#[cfg(windows)]
pub use imp::{send, serve};

#[cfg(not(windows))]
pub fn serve(
  _events: mpsc::Sender<Event>,
  _account: watch::Receiver<String>,
) -> io::Result<PipeServer> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "the local pipe is Windows-only",
  ))
}

#[cfg(not(windows))]
pub async fn send(_request: &Request, _wait_for_exit: Option<Duration>) -> io::Result<Reply> {
  Ok(Reply::NotRunning)
}

/// Ask the running bridge to quit and wait until its process has let go of
/// the single-instance lock in `app_dir`, all within `timeout`.
pub async fn quit_running_bridge(
  app_dir: &std::path::Path,
  timeout: Duration,
) -> io::Result<Reply> {
  let deadline = tokio::time::Instant::now() + timeout;
  let reply = send(&Request::Quit, Some(timeout)).await?;
  if reply == Reply::NotRunning {
    // No pipe, but the lock may still be held: a tray whose service stopped
    // with an error, or an older bridge without the pipe.
    return match crate::instance::acquire(app_dir) {
      Ok(_) => Ok(reply),
      Err(crate::instance::InstanceError::AlreadyRunning(_)) => Err(io::Error::other(
        "a bridge is running but doesn't answer on the local pipe; quit it from the tray",
      )),
      Err(err) => Err(io::Error::other(err.to_string())),
    };
  }
  // The pipe closes when the service has stopped; the program exits right
  // after, releasing the lock.
  loop {
    match crate::instance::acquire(app_dir) {
      Ok(lock) => {
        drop(lock);
        return Ok(Reply::Done);
      }
      Err(crate::instance::InstanceError::AlreadyRunning(_))
        if tokio::time::Instant::now() < deadline =>
      {
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
      Err(err) => return Err(io::Error::other(err.to_string())),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn requests_round_trip_and_junk_is_refused() {
    for request in [
      Request::Quit,
      Request::Login {
        account: "default@http://jellyfin.local:8096".to_owned(),
      },
      Request::Reload,
    ] {
      assert_eq!(
        Request::parse(&format!("{}\n", request.line())),
        Some(request)
      );
    }
    assert_eq!(Request::parse("login "), None);
    assert_eq!(Request::parse("stop"), None);
    assert_eq!(Request::parse(""), None);
  }

  #[cfg(windows)]
  mod pipe {
    use super::*;

    const ACCOUNT: &str = "default@http://jellyfin";

    fn account() -> watch::Receiver<String> {
      watch::channel(ACCOUNT.to_owned()).1
    }

    fn test_pipe() -> String {
      format!(
        r"\\.\pipe\jriver-cast-bridge-test-{}",
        uuid::Uuid::new_v4().simple()
      )
    }

    #[tokio::test]
    async fn nobody_listening_is_not_running() {
      let reply = imp::send_on(&test_pipe(), &Request::Quit, None)
        .await
        .unwrap();
      assert_eq!(reply, Reply::NotRunning);
    }

    #[tokio::test]
    async fn quit_becomes_shutdown_and_the_client_waits_until_the_bridge_stopped() {
      let name = test_pipe();
      let (tx, mut rx) = mpsc::channel(4);
      let server = imp::serve_on(name.clone(), tx, account()).unwrap();
      // A second bridge can't take the same pipe.
      let (tx2, _rx2) = mpsc::channel(4);
      assert!(imp::serve_on(name.clone(), tx2, account()).is_err());

      let client = tokio::spawn({
        let name = name.clone();
        async move { imp::send_on(&name, &Request::Quit, Some(Duration::from_secs(5))).await }
      });
      assert!(matches!(rx.recv().await, Some(Event::Shutdown)));
      // The client is still waiting: the bridge hasn't stopped yet.
      tokio::time::sleep(Duration::from_millis(300)).await;
      assert!(!client.is_finished());
      // The bridge has stopped: dropping the server closes the held pipe.
      drop(server);
      let reply = tokio::time::timeout(Duration::from_secs(2), client)
        .await
        .expect("the client sees the close")
        .unwrap();
      assert_eq!(reply.unwrap(), Reply::Done);
    }

    #[tokio::test]
    async fn a_quit_that_never_finishes_times_out_for_the_client() {
      let name = test_pipe();
      let (tx, mut rx) = mpsc::channel(4);
      let _server = imp::serve_on(name.clone(), tx, account()).unwrap();
      let client = imp::send_on(&name, &Request::Quit, Some(Duration::from_millis(300)));
      let (reply, event) = tokio::join!(client, rx.recv());
      assert!(matches!(event, Some(Event::Shutdown)));
      assert_eq!(reply.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn only_a_login_for_this_bridges_account_reconnects_it() {
      let name = test_pipe();
      let (tx, mut rx) = mpsc::channel(4);
      let _server = imp::serve_on(name.clone(), tx, account()).unwrap();
      let other = Request::Login {
        account: "verifier@http://jellyfin".to_owned(),
      };
      assert_eq!(
        imp::send_on(&name, &other, None).await.unwrap(),
        Reply::Done
      );
      let ours = Request::Login {
        account: ACCOUNT.to_owned(),
      };
      assert_eq!(imp::send_on(&name, &ours, None).await.unwrap(), Reply::Done);
      // Only the second one reaches the bridge.
      assert!(matches!(rx.recv().await, Some(Event::LoginChanged)));
      assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_account_follows_a_reloaded_config() {
      let name = test_pipe();
      let (tx, mut rx) = mpsc::channel(4);
      let (account_tx, account) = watch::channel(ACCOUNT.to_owned());
      let _server = imp::serve_on(name.clone(), tx, account).unwrap();
      account_tx.send_replace("verifier@http://jellyfin".to_owned());
      let login = Request::Login {
        account: "verifier@http://jellyfin".to_owned(),
      };
      assert_eq!(
        imp::send_on(&name, &login, None).await.unwrap(),
        Reply::Done
      );
      assert!(matches!(rx.recv().await, Some(Event::LoginChanged)));
    }
  }
}
