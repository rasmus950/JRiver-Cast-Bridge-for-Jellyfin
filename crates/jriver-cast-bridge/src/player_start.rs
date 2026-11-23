//! Starting Jellyfin Media Player (JMP) on the HTPC once the bridge is a
//! cast target (SPEC.md "Jellyfin Media Player").
//!
//! JMP's "autocast" selects its remembered cast target only when its own
//! connection to the server opens. If the bridge isn't registered yet, JMP
//! never selects it. So the bridge starts JMP itself, right after its first
//! registration, or after a while if Jellyfin can't be reached.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use log::{info, warn};
use tokio::sync::watch;

use crate::{config, process};

/// JMP is started this long after the bridge started even if it couldn't
/// register (Jellyfin unreachable): the HTPC still needs JMP.
pub const REGISTER_WAIT: Duration = Duration::from_secs(60);

/// Wait for the first registration (or [`REGISTER_WAIT`]), then start JMP
/// unless the settings turn that off or JMP already runs. Once per bridge
/// process: the settings are read at that moment.
pub async fn start_after_registration(
  mut registered: watch::Receiver<bool>,
  config_arg: Option<PathBuf>,
) {
  if let Ok((config, _)) = config::load(config_arg.as_deref()) {
    if config.jellyfin_player.start {
      for shortcut in startup_shortcuts() {
        warn!(
          "{} starts Jellyfin Media Player at logon, possibly before the bridge; remove it (the bridge starts JMP itself)",
          shortcut.display()
        );
      }
    }
  }
  // A bool at once: the watch guard mustn't live across the awaits below.
  let in_time = tokio::time::timeout(REGISTER_WAIT, registered.wait_for(|done| *done))
    .await
    .is_ok_and(|result| result.is_ok());
  if !in_time {
    warn!(
      "not registered with Jellyfin after {REGISTER_WAIT:?}; starting Jellyfin Media Player anyway"
    );
  }
  let config = match config::load(config_arg.as_deref()) {
    Ok((config, _)) => config,
    Err(err) => {
      warn!("can't read the settings to start Jellyfin Media Player: {err}");
      return;
    }
  };
  let player = &config.jellyfin_player;
  if !player.start {
    return;
  }
  start(&player.exe_path).await;
}

/// Start JMP at `exe` unless it already runs in this session.
async fn start(exe: &Path) {
  let Some(image) = exe.file_name().and_then(|name| name.to_str()) else {
    return;
  };
  if !exe.exists() {
    warn!(
      "Jellyfin Media Player isn't at {}; set its path in Settings, or turn starting it off",
      exe.display()
    );
    return;
  }
  match process::running(image).await {
    Ok(true) => {
      info!("Jellyfin Media Player is already running; not starting it");
      return;
    }
    Ok(false) => {}
    Err(err) => warn!("can't tell whether Jellyfin Media Player runs ({err}); starting it"),
  }
  let mut command = std::process::Command::new(exe);
  if let Some(dir) = exe.parent() {
    command.current_dir(dir);
  }
  match command.spawn() {
    Ok(_) => info!("started Jellyfin Media Player ({})", exe.display()),
    Err(err) => warn!("can't start Jellyfin Media Player: {err}"),
  }
}

/// The current user's Startup folder.
fn startup_dir() -> Option<PathBuf> {
  let appdata = std::env::var_os("APPDATA")?;
  Some(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

/// Shortcuts in the Startup folder that start JMP by themselves (named after
/// it), which would race the bridge.
pub fn startup_shortcuts() -> Vec<PathBuf> {
  let Some(dir) = startup_dir() else {
    return Vec::new();
  };
  let Ok(entries) = std::fs::read_dir(&dir) else {
    return Vec::new();
  };
  entries
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| is_player_shortcut(path))
    .collect()
}

fn is_player_shortcut(path: &Path) -> bool {
  let name = path
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or_default()
    .to_ascii_lowercase();
  name.ends_with(".lnk") && name.contains("jellyfin media player")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn jmp_shortcuts_are_told_by_name() {
    assert!(is_player_shortcut(Path::new(
      r"C:\x\Jellyfin Media Player - Shortcut.lnk"
    )));
    assert!(is_player_shortcut(Path::new(
      r"C:\x\jellyfin media player.LNK"
    )));
    assert!(!is_player_shortcut(Path::new(
      r"C:\x\JRiver Cast Bridge.lnk"
    )));
    assert!(!is_player_shortcut(Path::new(
      r"C:\x\Jellyfin Media Player.exe"
    )));
  }
}
