//! Only one bridge at a time: two would register two cast targets and fight
//! over JRiver. A lock on a file in the app folder enforces it; Windows drops
//! the lock when the process ends, even after a crash.

use std::{
  fs::{File, OpenOptions, TryLockError},
  io,
  path::{Path, PathBuf},
};

pub const LOCK_FILE: &str = "bridge.lock";
/// Held while a login window is open, so a second one isn't opened.
pub const LOGIN_LOCK_FILE: &str = "login.lock";

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
  #[error("another jriver-cast-bridge is already running (it holds {0})")]
  AlreadyRunning(PathBuf),
  #[error("can't create the lock file {path}: {source}")]
  Io {
    path: PathBuf,
    #[source]
    source: io::Error,
  },
}

/// Holds the lock while alive.
#[derive(Debug)]
pub struct InstanceLock {
  _file: File,
}

/// Take the bridge lock in `dir`, creating the folder if needed.
pub fn acquire(dir: &Path) -> Result<InstanceLock, InstanceError> {
  acquire_file(dir, LOCK_FILE)
}

/// Take the lock file `name` in `dir`, creating the folder if needed.
pub fn acquire_file(dir: &Path, name: &str) -> Result<InstanceLock, InstanceError> {
  let path = dir.join(name);
  let io_err = |source| InstanceError::Io {
    path: path.clone(),
    source,
  };
  std::fs::create_dir_all(dir).map_err(io_err)?;
  let file = OpenOptions::new()
    .create(true)
    .truncate(false)
    .write(true)
    .open(&path)
    .map_err(io_err)?;
  match file.try_lock() {
    Ok(()) => Ok(InstanceLock { _file: file }),
    Err(TryLockError::WouldBlock) => Err(InstanceError::AlreadyRunning(path)),
    Err(TryLockError::Error(source)) => Err(io_err(source)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_second_lock_is_refused_until_the_first_is_dropped() {
    let dir = std::env::temp_dir().join(format!(
      "jriver-cast-bridge-lock-test-{}",
      uuid::Uuid::new_v4().simple()
    ));
    let first = acquire(&dir).unwrap();
    assert!(matches!(
      acquire(&dir),
      Err(InstanceError::AlreadyRunning(_))
    ));
    drop(first);
    let again = acquire(&dir).unwrap();
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
