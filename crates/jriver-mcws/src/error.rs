use std::time::Duration;

use crate::Function;

/// A failed MCWS call. Every variant names the function, so a log line is
/// enough to tell what was attempted.
#[derive(Debug, thiserror::Error)]
pub enum McwsError {
  /// MC didn't answer in time (MC busy, hung, or not running).
  #[error("MCWS {function} timed out after {timeout:?}")]
  Timeout {
    function: Function,
    timeout: Duration,
  },
  /// No connection or another transport failure (MC not running, Media
  /// Network off, wrong port).
  #[error("MCWS {function} could not be reached: {source}")]
  Unreachable {
    function: Function,
    #[source]
    source: reqwest::Error,
  },
  /// MC answered with a non-success HTTP status. A 500 is what MC returns
  /// when it rejects a call, e.g. `PlayByFilename` on a bare folder.
  #[error("MCWS {function} returned HTTP {status}{}", detail_suffix(.detail))]
  Status {
    function: Function,
    status: u16,
    detail: Option<String>,
  },
  /// MC answered HTTP 200 but with `<Response Status="Failure">`.
  #[error("MCWS {function} failed: {message}")]
  Failure { function: Function, message: String },
  /// The response wasn't the XML MCWS documents.
  #[error("MCWS {function} response could not be parsed: {reason}")]
  Parse { function: Function, reason: String },
  /// The bridge refused to send the call, e.g. a filename containing `|`,
  /// which MCWS uses as the list delimiter.
  #[error("MCWS {function} not sent: {reason}")]
  InvalidArgument { function: Function, reason: String },
  /// Media Network authentication is on and the configured credentials were
  /// rejected (or none are configured).
  #[error("MCWS authentication failed for {function}: {reason}")]
  Auth { function: Function, reason: String },
}

fn detail_suffix(detail: &Option<String>) -> String {
  match detail {
    Some(detail) if !detail.is_empty() => format!(": {detail}"),
    _ => String::new(),
  }
}

impl McwsError {
  /// The function that failed.
  pub fn function(&self) -> Function {
    match self {
      Self::Timeout { function, .. }
      | Self::Unreachable { function, .. }
      | Self::Status { function, .. }
      | Self::Failure { function, .. }
      | Self::Parse { function, .. }
      | Self::InvalidArgument { function, .. }
      | Self::Auth { function, .. } => *function,
    }
  }

  /// True when MC itself is unreachable or not answering, as opposed to MC
  /// answering with an error. The session controller treats this as "MCWS
  /// stopped answering".
  pub fn is_unreachable(&self) -> bool {
    matches!(self, Self::Timeout { .. } | Self::Unreachable { .. })
  }
}
