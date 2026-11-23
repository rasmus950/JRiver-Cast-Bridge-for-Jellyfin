//! Logging: a daily file in `%APPDATA%\jriver-cast-bridge\logs` (SPEC:
//! rotating file next to the config), plus stderr when there is a console.

use std::path::Path;

use tracing_appender::{
  non_blocking::WorkerGuard,
  rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
  fmt::{self, time::ChronoLocal},
  layer::SubscriberExt,
  util::SubscriberInitExt,
  EnvFilter,
};

/// Log file names: `bridge.YYYY-MM-DD.log`.
pub const FILE_PREFIX: &str = "bridge";
pub const FILE_SUFFIX: &str = "log";

/// The filter for `level`: the bridge's own crates at `level`, everything
/// else at warn.
pub fn filter(level: log::LevelFilter) -> String {
  let level = level.as_str().to_ascii_lowercase();
  format!("warn,jriver_mcws={level},jriver_cast_bridge={level},bridge_cli={level}")
}

/// Start logging. Keep the returned guard alive until exit, or the last
/// lines may not reach the file. If the log folder can't be used, logs go to
/// stderr only and the reason is logged there.
pub fn init(level: log::LevelFilter, dir: Option<&Path>, keep_days: usize) -> Option<WorkerGuard> {
  let stderr = fmt::layer()
    .with_timer(ChronoLocal::rfc_3339())
    .with_writer(std::io::stderr)
    .with_target(level >= log::LevelFilter::Debug);
  let (file, guard, problem) = match dir.map(|dir| open_appender(dir, keep_days)) {
    Some(Ok(appender)) => {
      let (writer, guard) = tracing_appender::non_blocking(appender);
      let layer = fmt::layer()
        .with_timer(ChronoLocal::rfc_3339())
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true);
      (Some(layer), Some(guard), None)
    }
    Some(Err(err)) => (None, None, Some(err)),
    None => (None, None, None),
  };
  tracing_subscriber::registry()
    .with(EnvFilter::new(filter(level)))
    .with(stderr)
    .with(file)
    .init();
  if let Some(problem) = problem {
    log::warn!("logging to stderr only: {problem}");
  }
  guard
}

fn open_appender(dir: &Path, keep_days: usize) -> Result<RollingFileAppender, String> {
  std::fs::create_dir_all(dir).map_err(|err| format!("can't create {}: {err}", dir.display()))?;
  RollingFileAppender::builder()
    .rotation(Rotation::DAILY)
    .filename_prefix(FILE_PREFIX)
    .filename_suffix(FILE_SUFFIX)
    .max_log_files(keep_days.max(1))
    .build(dir)
    .map_err(|err| format!("can't open a log file in {}: {err}", dir.display()))
}

/// The newest log file in `dir`, for "Open log".
pub fn newest_log(dir: &Path) -> Option<std::path::PathBuf> {
  std::fs::read_dir(dir)
    .ok()?
    .filter_map(Result::ok)
    .filter(|entry| {
      let name = entry.file_name().to_string_lossy().to_string();
      name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)
    })
    .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
    .max_by_key(|(modified, _)| *modified)
    .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn filter_lifts_only_the_bridge_crates() {
    let f = filter(log::LevelFilter::Debug);
    assert!(f.starts_with("warn,"));
    assert!(f.contains("jriver_cast_bridge=debug"));
    EnvFilter::try_new(&f).unwrap();
  }

  #[test]
  fn newest_log_picks_the_latest_bridge_file() {
    let dir = std::env::temp_dir().join(format!(
      "jriver-cast-bridge-log-test-{}",
      uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(newest_log(&dir), None);
    std::fs::write(dir.join("bridge.2026-11-20.log"), "old").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(dir.join("bridge.2026-11-21.log"), "new").unwrap();
    std::fs::write(dir.join("other.txt"), "x").unwrap();
    assert_eq!(newest_log(&dir), Some(dir.join("bridge.2026-11-21.log")));
    let _ = std::fs::remove_dir_all(&dir);
  }
}
