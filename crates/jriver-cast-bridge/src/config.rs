//! `%APPDATA%\jriver-cast-bridge\config.toml`.
//!
//! The Jellyfin token is never here: the login window keeps it in Windows
//! Credential Manager ([`crate::credentials`]).

use std::{
  fs,
  path::{Path, PathBuf},
  time::Duration,
};

use jriver_mcws::{Credentials, McwsConfig};
use serde::Deserialize;

use crate::{
  controller::ControllerOptions,
  jellyfin::check_header_value,
  jriver::LaunchOptions,
  path_map::{MapError, PathMapper, PathRule},
};

pub const APP_DIR: &str = "jriver-cast-bridge";
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
  #[serde(default)]
  pub mcws: McwsSection,
  #[serde(default)]
  pub path_map: Vec<PathRule>,
  /// Needed by the bridge and the Jellyfin commands of `bridge-cli`.
  pub jellyfin: Option<JellyfinSection>,
  #[serde(default)]
  pub bridge: BridgeSection,
  #[serde(default)]
  pub watchdog: WatchdogSection,
  #[serde(default)]
  pub jellyfin_player: JellyfinPlayerSection,
  #[serde(default)]
  pub log: LogSection,
}

/// Starting Jellyfin Media Player on the HTPC once the bridge is a cast
/// target (SPEC.md "Jellyfin Media Player"): JMP's autocast looks for its remembered
/// cast target only when it connects, so it must start after the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JellyfinPlayerSection {
  pub start: bool,
  pub exe_path: PathBuf,
}

/// Where the Jellyfin Media Player installer puts it.
pub const DEFAULT_PLAYER_PATH: &str =
  r"C:\Program Files\Jellyfin\Jellyfin Media Player\JellyfinMediaPlayer.exe";

impl Default for JellyfinPlayerSection {
  fn default() -> Self {
    Self {
      start: true,
      exe_path: PathBuf::from(DEFAULT_PLAYER_PATH),
    }
  }
}

/// Restarting a hung MC (SPEC.md "JRiver hung").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WatchdogSection {
  /// Seconds Windows must say "Not responding" (while MCWS doesn't answer)
  /// before MC is restarted.
  pub hang_after_s: u64,
  /// The same while a cast is opening a file: the NAS disks may be
  /// spinning up.
  pub hang_while_starting_s: u64,
  /// Restart attempts per hour; 0 never restarts MC.
  pub max_restarts_per_hour: usize,
}

impl Default for WatchdogSection {
  fn default() -> Self {
    let defaults = ControllerOptions::default();
    Self {
      hang_after_s: defaults.hang_confirm.as_secs(),
      hang_while_starting_s: defaults.hang_confirm_starting.as_secs(),
      max_restarts_per_hour: defaults.max_restarts_per_hour,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
  /// `error`, `warn`, `info` or `debug` (every MCWS and Jellyfin call).
  pub level: String,
  /// Daily log files kept in the `logs` folder next to the config.
  pub keep_days: usize,
}

impl Default for LogSection {
  fn default() -> Self {
    Self {
      level: "info".to_owned(),
      keep_days: 14,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JellyfinSection {
  /// Server base URL, e.g. `http://jellyfin.local:8096`.
  pub url: String,
  /// The cast-target name shown in Jellyfin's cast menu.
  #[serde(default = "default_device_name")]
  pub device_name: String,
  /// Names the saved login in Credential Manager, so a test login can live
  /// beside the real one.
  #[serde(default = "default_profile")]
  pub profile: String,
}

pub(crate) fn default_device_name() -> String {
  "JRiver (HTPC)".to_owned()
}

pub(crate) fn default_profile() -> String {
  "default".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BridgeSection {
  /// MCWS status poll while playing (SPEC: once a second).
  pub poll_interval_ms: u64,
  /// Progress report while playing (SPEC: every 10 s).
  pub progress_interval_s: u64,
  /// Minimize MC after a full-screen playback ends, so the Jellyfin app on
  /// the HTPC gets focus back.
  pub minimize_after_playback: bool,
  /// Play the audio and subtitle tracks picked in Jellyfin (and offer the
  /// pickers there). Off: JRiver decides, and the bridge sends no track
  /// commands.
  pub tracks_from_jellyfin: bool,
}

impl Default for BridgeSection {
  fn default() -> Self {
    let defaults = ControllerOptions::default();
    Self {
      poll_interval_ms: u64::try_from(defaults.poll_interval.as_millis()).unwrap_or(1_000),
      progress_interval_s: defaults.progress_interval.as_secs(),
      minimize_after_playback: defaults.minimize_after_playback,
      tracks_from_jellyfin: defaults.tracks_from_jellyfin,
    }
  }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McwsSection {
  /// MCWS base URL.
  pub url: String,
  /// Zone ID to drive. 0 is MC's local "Player" zone.
  pub zone_id: i32,
  /// Per-call timeout in milliseconds.
  pub timeout_ms: u64,
  /// Only when MC's Media Network authentication is on.
  pub username: Option<String>,
  pub password: Option<String>,
  /// `Media Center 36.exe`, to start MC when a cast arrives and MC isn't
  /// running. Unset: never start MC.
  pub exe_path: Option<PathBuf>,
}

impl Default for McwsSection {
  fn default() -> Self {
    let defaults = McwsConfig::default();
    Self {
      url: defaults.base_url,
      zone_id: defaults.zone_id,
      timeout_ms: u64::try_from(defaults.timeout.as_millis()).unwrap_or(5_000),
      username: None,
      password: None,
      exe_path: None,
    }
  }
}

impl std::fmt::Debug for McwsSection {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("McwsSection")
      .field("url", &self.url)
      .field("zone_id", &self.zone_id)
      .field("timeout_ms", &self.timeout_ms)
      .field("username", &self.username)
      .field("password", &self.password.as_ref().map(|_| "<redacted>"))
      .field("exe_path", &self.exe_path)
      .finish()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
  #[error("can't read config {path}: {source}")]
  Read {
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },
  #[error("config {path} is invalid: {message}")]
  Parse { path: PathBuf, message: String },
  #[error("config {path}: {message}")]
  Invalid { path: PathBuf, message: String },
  #[error("config {path}: {source}")]
  PathMap {
    path: PathBuf,
    #[source]
    source: MapError,
  },
  #[error("%APPDATA% is not set, so the default config location is unknown; pass --config")]
  NoAppData,
}

/// Where the config came from, for log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
  File(PathBuf),
  /// The default file doesn't exist yet; built-in defaults are in use.
  Defaults(PathBuf),
}

/// `%APPDATA%\jriver-cast-bridge`: config, logs, lock file.
pub fn app_dir() -> Result<PathBuf, ConfigError> {
  let appdata = std::env::var_os("APPDATA").ok_or(ConfigError::NoAppData)?;
  Ok(PathBuf::from(appdata).join(APP_DIR))
}

/// `%APPDATA%\jriver-cast-bridge\config.toml`.
pub fn default_path() -> Result<PathBuf, ConfigError> {
  Ok(app_dir()?.join(CONFIG_FILE))
}

/// `%APPDATA%\jriver-cast-bridge\logs`.
pub fn log_dir() -> Result<PathBuf, ConfigError> {
  Ok(app_dir()?.join("logs"))
}

/// The example config, for creating a missing one.
pub const EXAMPLE: &str = include_str!("../config.example.toml");

fn parse_level(level: &str) -> Option<log::LevelFilter> {
  match level.trim().to_ascii_lowercase().as_str() {
    "error" => Some(log::LevelFilter::Error),
    "warn" => Some(log::LevelFilter::Warn),
    "info" => Some(log::LevelFilter::Info),
    "debug" => Some(log::LevelFilter::Debug),
    _ => None,
  }
}

/// Load an explicit config file (must exist), or the default one (built-in
/// defaults when it doesn't exist yet).
pub fn load(explicit: Option<&Path>) -> Result<(Config, ConfigSource), ConfigError> {
  match explicit {
    Some(path) => Ok((load_file(path)?, ConfigSource::File(path.to_owned()))),
    None => {
      let path = default_path()?;
      if path.exists() {
        Ok((load_file(&path)?, ConfigSource::File(path)))
      } else {
        Ok((Config::default(), ConfigSource::Defaults(path)))
      }
    }
  }
}

pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
  let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
    path: path.to_owned(),
    source,
  })?;
  parse(&text, path)
}

/// The settings as written, without checking them: for the settings window,
/// which shows a config with an error so it can be fixed.
pub fn parse_unchecked(text: &str) -> Result<Config, String> {
  toml::from_str(text).map_err(|err| describe(&err, text))
}

/// A TOML error as "line L, column C: what's wrong", never quoting the
/// file: the quoted line could hold the Media Network password, and this
/// message is logged and shown in the tray.
fn describe(err: &toml::de::Error, text: &str) -> String {
  match err.span() {
    Some(span) => {
      let before = &text[..span.start.min(text.len())];
      let line = before.matches('\n').count() + 1;
      let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
      format!("line {line}, column {column}: {}", err.message())
    }
    None => err.message().to_owned(),
  }
}

pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
  let config: Config = toml::from_str(text).map_err(|err| ConfigError::Parse {
    path: path.to_owned(),
    message: describe(&err, text),
  })?;
  config.validate(path)?;
  Ok(config)
}

impl Config {
  fn validate(&self, path: &Path) -> Result<(), ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
      path: path.to_owned(),
      message,
    };
    if !self.mcws.url.starts_with("http://") {
      return Err(invalid(format!(
        "mcws.url must be an http:// URL, got {:?}",
        self.mcws.url
      )));
    }
    if self.mcws.zone_id < 0 {
      return Err(invalid(
        "mcws.zone_id must be a real zone ID (0 is \"Player\"); -1 would follow MC's current zone"
          .to_owned(),
      ));
    }
    if self.mcws.timeout_ms == 0 || self.mcws.timeout_ms > 60_000 {
      return Err(invalid(
        "mcws.timeout_ms must be between 1 and 60000".to_owned(),
      ));
    }
    // The password lives in Credential Manager; one still in the file (from
    // before 0.6.0) is moved there by the bridge.
    if self
      .mcws
      .username
      .as_deref()
      .is_some_and(|user| user.trim().is_empty())
    {
      return Err(invalid(
        "mcws.username is empty; remove it for no authentication".to_owned(),
      ));
    }
    if self.mcws.password.is_some() && self.mcws.username.is_none() {
      return Err(invalid(
        "mcws.password needs mcws.username (and belongs in Credential Manager: set it in Settings)"
          .to_owned(),
      ));
    }
    PathMapper::new(&self.path_map).map_err(|source| ConfigError::PathMap {
      path: path.to_owned(),
      source,
    })?;
    if let Some(jellyfin) = &self.jellyfin {
      let url = jellyfin.url.trim();
      if !(url.starts_with("http://") || url.starts_with("https://")) || url.contains(['?', '#']) {
        return Err(invalid(format!(
          "jellyfin.url must be the server's http:// or https:// base URL, got {url:?}"
        )));
      }
      check_header_value("jellyfin.device_name", &jellyfin.device_name)
        .map_err(|err| invalid(err.to_string()))?;
      let profile_ok = !jellyfin.profile.is_empty()
        && jellyfin.profile.len() <= 32
        && jellyfin
          .profile
          .bytes()
          .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
      if !profile_ok {
        return Err(invalid(
          "jellyfin.profile must be 1-32 letters, digits, - or _".to_owned(),
        ));
      }
    }
    if !(250..=5_000).contains(&self.bridge.poll_interval_ms) {
      return Err(invalid(
        "bridge.poll_interval_ms must be between 250 and 5000".to_owned(),
      ));
    }
    if !(1..=60).contains(&self.bridge.progress_interval_s) {
      return Err(invalid(
        "bridge.progress_interval_s must be between 1 and 60".to_owned(),
      ));
    }
    if let Some(exe) = &self.mcws.exe_path {
      let is_exe = exe
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
      if !exe.is_absolute() || !is_exe {
        return Err(invalid(format!(
          "mcws.exe_path must be the full path to Media Center's .exe, got {}",
          exe.display()
        )));
      }
    }
    let player = &self.jellyfin_player.exe_path;
    let player_is_exe = player
      .extension()
      .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if !player.is_absolute() || !player_is_exe {
      return Err(invalid(format!(
        "jellyfin_player.exe_path must be the full path to Jellyfin Media Player's .exe, got {}",
        player.display()
      )));
    }
    if !(1..=60).contains(&self.watchdog.hang_after_s) {
      return Err(invalid(
        "watchdog.hang_after_s must be between 1 and 60".to_owned(),
      ));
    }
    if !(1..=300).contains(&self.watchdog.hang_while_starting_s) {
      return Err(invalid(
        "watchdog.hang_while_starting_s must be between 1 and 300".to_owned(),
      ));
    }
    if self.watchdog.max_restarts_per_hour > 20 {
      return Err(invalid(
        "watchdog.max_restarts_per_hour must be between 0 and 20".to_owned(),
      ));
    }
    if parse_level(&self.log.level).is_none() {
      return Err(invalid(format!(
        "log.level must be error, warn, info or debug, got {:?}",
        self.log.level
      )));
    }
    if !(1..=365).contains(&self.log.keep_days) {
      return Err(invalid(
        "log.keep_days must be between 1 and 365".to_owned(),
      ));
    }
    Ok(())
  }

  /// The configured log level.
  pub fn log_level(&self) -> log::LevelFilter {
    parse_level(&self.log.level).unwrap_or(log::LevelFilter::Info)
  }

  pub fn launch_options(&self) -> LaunchOptions {
    LaunchOptions {
      exe_path: self.mcws.exe_path.clone(),
      ..LaunchOptions::default()
    }
  }

  /// The `[jellyfin]` section, or an error saying it's needed.
  pub fn jellyfin(&self) -> Result<&JellyfinSection, String> {
    self.jellyfin.as_ref().ok_or_else(|| {
      "the config has no [jellyfin] section (url, device_name); see config.example.toml".to_owned()
    })
  }

  pub fn controller_options(&self) -> ControllerOptions {
    ControllerOptions {
      poll_interval: Duration::from_millis(self.bridge.poll_interval_ms),
      progress_interval: Duration::from_secs(self.bridge.progress_interval_s),
      minimize_after_playback: self.bridge.minimize_after_playback,
      tracks_from_jellyfin: self.bridge.tracks_from_jellyfin,
      hang_confirm: Duration::from_secs(self.watchdog.hang_after_s),
      hang_confirm_starting: Duration::from_secs(self.watchdog.hang_while_starting_s),
      max_restarts_per_hour: self.watchdog.max_restarts_per_hour,
      ..ControllerOptions::default()
    }
  }

  /// The MCWS client's settings, with the Media Network password from
  /// Credential Manager (see [`crate::credentials::McwsSecret`]); there's
  /// authentication only with a user name and a password.
  pub fn mcws_config(&self, password: Option<String>) -> McwsConfig {
    McwsConfig {
      base_url: self.mcws.url.clone(),
      zone_id: self.mcws.zone_id,
      timeout: Duration::from_millis(self.mcws.timeout_ms),
      credentials: match (&self.mcws.username, password) {
        (Some(username), Some(password)) => Some(Credentials {
          username: username.clone(),
          password,
        }),
        _ => None,
      },
    }
  }

  pub fn path_mapper(&self) -> Result<PathMapper, MapError> {
    PathMapper::new(&self.path_map)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn p() -> &'static Path {
    Path::new("test.toml")
  }

  #[test]
  fn parse_errors_never_quote_the_file() {
    let text = "[mcws]\nusername = \"u\"\npassword = \"s3cret";
    let err = parse(text, p()).unwrap_err().to_string();
    assert!(!err.contains("s3cret"), "{err}");
    assert!(err.contains("line 3, column 19"), "{err}");
    let err = parse_unchecked("[mcws]\npassword = 'x' 'leak'").unwrap_err();
    assert!(!err.contains("leak"), "{err}");
    assert!(err.starts_with("line 2"), "{err}");
  }

  #[test]
  fn empty_file_gives_defaults() {
    let config = parse("", p()).unwrap();
    assert_eq!(config, Config::default());
    let mcws = config.mcws_config(None);
    assert_eq!(mcws.base_url, "http://localhost:52199/MCWS/v1/");
    assert_eq!(mcws.zone_id, 0);
    assert_eq!(mcws.timeout, Duration::from_secs(5));
    assert!(mcws.credentials.is_none());
    assert!(config.path_mapper().unwrap().is_empty());
  }

  #[test]
  fn example_config_parses() {
    let text = include_str!("../config.example.toml");
    let config = parse(text, p()).unwrap();
    assert_eq!(config.path_map.len(), 2);
    assert_eq!(config.path_map[0].from, "/media/movies");
    assert_eq!(config.path_map[0].to, r"\\nas\movies");
    config.path_mapper().unwrap();
    let jellyfin = config.jellyfin().unwrap();
    assert_eq!(jellyfin.url, "http://jellyfin.local:8096");
    assert_eq!(jellyfin.device_name, "JRiver (HTPC)");
    assert_eq!(jellyfin.profile, "default");
    assert_eq!(config.controller_options(), ControllerOptions::default());
    assert_eq!(
      config.launch_options().exe_path.as_deref(),
      Some(Path::new(
        r"C:\Program Files\J River\Media Center 36\Media Center 36.exe"
      ))
    );
    assert_eq!(config.log_level(), log::LevelFilter::Info);
    assert_eq!(config.log.keep_days, 14);
  }

  #[test]
  fn exe_path_log_and_minimize_settings() {
    let config = parse(
      "[log]\nlevel = \"DEBUG\"\n[bridge]\nminimize_after_playback = false",
      p(),
    )
    .unwrap();
    assert_eq!(config.log_level(), log::LevelFilter::Debug);
    assert!(!config.controller_options().minimize_after_playback);
    assert!(config.launch_options().exe_path.is_none());
    for (text, needle) in [
      ("[mcws]\nexe_path = 'mc.exe'", "exe_path"),
      ("[mcws]\nexe_path = 'C:\\MC\\mc.bat'", "exe_path"),
      ("[log]\nlevel = \"loud\"", "log.level"),
      ("[log]\nkeep_days = 0", "keep_days"),
    ] {
      let err = parse(text, p()).unwrap_err().to_string();
      assert!(err.contains(needle), "{text:?}: {err}");
    }
  }

  #[test]
  fn jellyfin_section_defaults_and_checks() {
    let config = parse("[jellyfin]\nurl = \"http://jf:8096\"", p()).unwrap();
    assert_eq!(config.jellyfin().unwrap().device_name, "JRiver (HTPC)");
    assert!(parse("", p()).unwrap().jellyfin().is_err());
    for (text, needle) in [
      ("[jellyfin]\nurl = \"jf:8096\"", "jellyfin.url"),
      (
        "[jellyfin]\nurl = \"http://jf\"\ndevice_name = \"a,b\"",
        "device_name",
      ),
      (
        "[jellyfin]\nurl = \"http://jf\"\nprofile = \"a b\"",
        "profile",
      ),
      (
        "[jellyfin]\nurl = \"http://jf\"\ntoken = \"x\"",
        "unknown field",
      ),
      ("[bridge]\npoll_interval_ms = 10", "poll_interval_ms"),
      ("[bridge]\nprogress_interval_s = 0", "progress_interval_s"),
    ] {
      let err = parse(text, p()).unwrap_err().to_string();
      assert!(err.contains(needle), "{text:?}: {err}");
    }
  }

  #[test]
  fn full_mcws_section() {
    let config = parse(
      r#"
[mcws]
url = "http://127.0.0.1:52199/MCWS/v1/"
zone_id = 10001
timeout_ms = 2500
username = "mc"
password = "secret"
"#,
      p(),
    )
    .unwrap();
    let mcws = config.mcws_config(Some("secret".to_owned()));
    assert_eq!(mcws.zone_id, 10001);
    assert_eq!(mcws.timeout, Duration::from_millis(2500));
    assert_eq!(mcws.credentials.unwrap().username, "mc");
    assert!(!format!("{config:?}").contains("secret"));
  }

  #[test]
  fn rejects_bad_values() {
    for (text, needle) in [
      ("[mcws]\nzone_id = -1", "zone_id"),
      (
        "[mcws]\nurl = \"https://localhost:52199/MCWS/v1/\"",
        "http://",
      ),
      ("[mcws]\ntimeout_ms = 0", "timeout_ms"),
      ("[mcws]\npassword = \"pw\"", "needs mcws.username"),
      ("[mcws]\nusername = \" \"", "mcws.username is empty"),
      ("[[path_map]]\nfrom = '/media/movies'\nto = 'M:\\'", "UNC"),
      ("[mcws]\nzone = 0", "unknown field"),
      ("[[path_map]]\nfrom = '/a'", "missing field"),
      ("nonsense", "invalid"),
    ] {
      let err = parse(text, p()).unwrap_err().to_string();
      assert!(err.contains(needle), "{text:?}: {err}");
    }
  }

  #[test]
  fn explicit_missing_file_is_an_error() {
    let missing = std::env::temp_dir().join("bridge-no-such-config.toml");
    assert!(matches!(
      load(Some(&missing)),
      Err(ConfigError::Read { .. })
    ));
  }
}
