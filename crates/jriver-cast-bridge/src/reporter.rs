//! Sends playback reports in the background, so a slow or unreachable
//! server never holds up JRiver control (SPEC.md "Network blip to Jellyfin:
//! keep playing, hold the latest progress, resend on reconnect").
//!
//! - Reports reach the server in the order they were made.
//! - Progress reports of one playback coalesce: only the newest one waits.
//! - A failure that may pass (network, HTTP 5xx) is retried with capped
//!   backoff, for at most `max_age`; then the report is dropped with an
//!   error. A failure that won't pass (401, 400) drops it at once.

use std::{collections::VecDeque, future::Future, sync::Arc, time::Duration};

use log::{error, info, warn};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};

use crate::jellyfin::{JellyfinClient, JellyfinError, Report};

/// Where reports go. [`JellyfinClient`] in the bridge, a fake in tests.
pub trait ReportApi: Send + Sync + 'static {
  fn send(&self, report: &Report) -> impl Future<Output = Result<(), JellyfinError>> + Send;
}

impl ReportApi for JellyfinClient {
  fn send(&self, report: &Report) -> impl Future<Output = Result<(), JellyfinError>> + Send {
    self.report(report)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReporterOptions {
  /// Give up on a report the server hasn't taken after this long.
  pub max_age: Duration,
  /// Most reports kept waiting; the oldest progress report goes first.
  pub max_queue: usize,
}

impl Default for ReporterOptions {
  fn default() -> Self {
    Self {
      max_age: Duration::from_secs(60 * 60),
      max_queue: 100,
    }
  }
}

/// Handle the controller sends reports through. Never blocks.
#[derive(Debug, Clone)]
pub struct Reporter {
  tx: mpsc::UnboundedSender<Report>,
}

impl Reporter {
  /// A reporter and the receiving end, for [`run`] (or a test).
  pub fn channel() -> (Self, mpsc::UnboundedReceiver<Report>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Self { tx }, rx)
  }

  pub fn send(&self, report: Report) {
    if self.tx.send(report).is_err() {
      error!("Jellyfin reporter has stopped; a playback report was lost");
    }
  }
}

/// Start the background sender. It ends once every [`Reporter`] is dropped
/// and the queue is empty (or every report in it has been given up on).
pub fn spawn<A: ReportApi>(api: Arc<A>, options: ReporterOptions) -> (Reporter, JoinHandle<()>) {
  let (reporter, rx) = Reporter::channel();
  let handle = tokio::spawn(run(api, rx, options));
  (reporter, handle)
}

struct Pending {
  report: Report,
  queued_at: Instant,
}

const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30, 60];

fn backoff(attempt: usize) -> Duration {
  Duration::from_secs(BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)])
}

pub async fn run<A: ReportApi>(
  api: Arc<A>,
  mut rx: mpsc::UnboundedReceiver<Report>,
  options: ReporterOptions,
) {
  let mut queue: VecDeque<Pending> = VecDeque::new();
  let mut attempt = 0;
  let mut closed = false;
  loop {
    if queue.is_empty() {
      if closed {
        return;
      }
      match rx.recv().await {
        Some(report) => push(&mut queue, report, options.max_queue),
        None => return,
      }
    }
    while let Ok(report) = rx.try_recv() {
      push(&mut queue, report, options.max_queue);
    }

    let front = queue.front().expect("queue is not empty here");
    if front.queued_at.elapsed() > options.max_age {
      error!(
        "Jellyfin: giving up on a {} after {:?} of failures; the server may show a stale state",
        describe(&front.report),
        options.max_age
      );
      queue.pop_front();
      attempt = 0;
      continue;
    }

    match api.send(&front.report).await {
      Ok(()) => {
        if attempt > 0 {
          info!(
            "Jellyfin: reachable again; sent the waiting {}",
            describe(&front.report)
          );
        }
        queue.pop_front();
        attempt = 0;
      }
      Err(err) if !err.is_retryable() => {
        error!("Jellyfin: {err}; dropping this {}", describe(&front.report));
        queue.pop_front();
        attempt = 0;
      }
      Err(err) => {
        let delay = backoff(attempt);
        attempt += 1;
        warn!(
          "Jellyfin: {err}; retrying in {delay:?} ({} report(s) waiting)",
          queue.len()
        );
        // Wait, but keep taking new reports so progress coalesces.
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
          tokio::select! {
            () = &mut sleep => break,
            report = rx.recv(), if !closed => match report {
              Some(report) => push(&mut queue, report, options.max_queue),
              None => closed = true,
            },
          }
        }
      }
    }
  }
}

fn push(queue: &mut VecDeque<Pending>, report: Report, max_queue: usize) {
  if let Report::Progress(new) = &report {
    if let Some(Pending {
      report: Report::Progress(old),
      ..
    }) = queue.back_mut()
    {
      if old.play_session_id == new.play_session_id {
        *old = new.clone();
        return;
      }
    }
  }
  queue.push_back(Pending {
    report,
    queued_at: Instant::now(),
  });
  if queue.len() > max_queue {
    let index = queue
      .iter()
      .position(|p| matches!(p.report, Report::Progress(_)))
      .unwrap_or(0);
    if let Some(dropped) = queue.remove(index) {
      warn!(
        "Jellyfin: too many reports waiting; dropped a {}",
        describe(&dropped.report)
      );
    }
  }
}

fn describe(report: &Report) -> &'static str {
  match report {
    Report::Start(_) => "start report",
    Report::Progress(_) => "progress report",
    Report::Stopped(_) => "stop report",
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use super::*;
  use crate::{
    jellyfin::{PlayMethod, ProgressReport, StopReport},
    units::Ticks,
  };

  /// Fails with the scripted errors first, then succeeds; records sends.
  struct FakeApi {
    failures: Mutex<VecDeque<JellyfinError>>,
    sent: Mutex<Vec<Report>>,
    attempts: Mutex<u32>,
  }

  impl FakeApi {
    fn new(failures: Vec<JellyfinError>) -> Arc<Self> {
      Arc::new(Self {
        failures: Mutex::new(failures.into()),
        sent: Mutex::new(Vec::new()),
        attempts: Mutex::new(0),
      })
    }

    fn sent(&self) -> Vec<Report> {
      self.sent.lock().unwrap().clone()
    }
  }

  impl ReportApi for FakeApi {
    async fn send(&self, report: &Report) -> Result<(), JellyfinError> {
      *self.attempts.lock().unwrap() += 1;
      if let Some(err) = self.failures.lock().unwrap().pop_front() {
        return Err(err);
      }
      self.sent.lock().unwrap().push(report.clone());
      Ok(())
    }
  }

  fn unavailable() -> JellyfinError {
    JellyfinError::Status {
      what: "report",
      status: 503,
      detail: String::new(),
    }
  }

  fn progress(session: &str, ms: i64) -> Report {
    Report::Progress(ProgressReport {
      item_id: "item".to_owned(),
      media_source_id: None,
      play_session_id: session.to_owned(),
      position_ticks: Ticks(ms * 10_000),
      is_paused: false,
      is_muted: false,
      can_seek: true,
      play_method: PlayMethod::DirectPlay,
      audio_stream_index: None,
      subtitle_stream_index: None,
    })
  }

  fn stopped(session: &str) -> Report {
    Report::Stopped(StopReport {
      item_id: "item".to_owned(),
      media_source_id: None,
      play_session_id: session.to_owned(),
      position_ticks: Ticks(0),
      failed: false,
    })
  }

  #[tokio::test(start_paused = true)]
  async fn sends_in_order() {
    let api = FakeApi::new(vec![]);
    let (reporter, handle) = spawn(api.clone(), ReporterOptions::default());
    reporter.send(progress("a", 1));
    reporter.send(stopped("a"));
    drop(reporter);
    handle.await.unwrap();
    assert_eq!(api.sent(), vec![progress("a", 1), stopped("a")]);
  }

  #[tokio::test(start_paused = true)]
  async fn holds_the_latest_progress_through_an_outage() {
    let api = FakeApi::new(vec![unavailable(), unavailable(), unavailable()]);
    let (reporter, handle) = spawn(api.clone(), ReporterOptions::default());
    reporter.send(progress("a", 1));
    tokio::time::sleep(Duration::from_millis(500)).await;
    reporter.send(progress("a", 2));
    reporter.send(progress("a", 3));
    reporter.send(stopped("a"));
    drop(reporter);
    handle.await.unwrap();
    assert_eq!(api.sent(), vec![progress("a", 3), stopped("a")]);
  }

  #[tokio::test(start_paused = true)]
  async fn drops_reports_the_server_refuses() {
    let api = FakeApi::new(vec![JellyfinError::Unauthorized { what: "report" }]);
    let (reporter, handle) = spawn(api.clone(), ReporterOptions::default());
    reporter.send(progress("a", 1));
    reporter.send(stopped("a"));
    drop(reporter);
    handle.await.unwrap();
    assert_eq!(api.sent(), vec![stopped("a")]);
    assert_eq!(*api.attempts.lock().unwrap(), 2);
  }

  #[tokio::test(start_paused = true)]
  async fn gives_up_after_max_age() {
    let failures = (0..200).map(|_| unavailable()).collect();
    let api = FakeApi::new(failures);
    let options = ReporterOptions {
      max_age: Duration::from_secs(300),
      max_queue: 100,
    };
    let (reporter, handle) = spawn(api.clone(), options);
    reporter.send(stopped("a"));
    drop(reporter);
    handle.await.unwrap();
    assert!(api.sent().is_empty());
    // 1+2+5+10+30+60+60+60+60 s ≈ 288 s, then one more try past 300 s.
    let attempts = *api.attempts.lock().unwrap();
    assert!((8..=12).contains(&attempts), "{attempts} attempts");
  }

  #[test]
  fn progress_of_different_playbacks_does_not_coalesce_and_queue_is_capped() {
    let mut queue = VecDeque::new();
    push(&mut queue, progress("a", 1), 3);
    push(&mut queue, progress("b", 1), 3);
    push(&mut queue, progress("b", 2), 3);
    assert_eq!(queue.len(), 2);
    push(&mut queue, stopped("b"), 3);
    push(&mut queue, stopped("c"), 3);
    // Over the cap: the oldest progress report (playback "a") went.
    assert_eq!(queue.len(), 3);
    assert!(matches!(&queue[0].report, Report::Progress(r) if r.play_session_id == "b"));
    assert!(matches!(&queue[2].report, Report::Stopped(r) if r.play_session_id == "c"));
  }
}
