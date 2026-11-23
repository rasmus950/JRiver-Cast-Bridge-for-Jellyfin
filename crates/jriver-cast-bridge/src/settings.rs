//! The settings window's form (SPEC.md "Settings"): every config setting as
//! editable text, read from a [`Config`] and written back into the config
//! file's TOML in place, so its comments and layout stay. What's written is
//! checked with the bridge's own [`config::parse`] first: the window can't
//! save a config the bridge would refuse.

use std::path::Path;

use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::{
  config::{self, Config},
  credentials::{CredentialError, McwsSecret, Secrets},
};

/// MCWS log levels offered by the window.
pub const LOG_LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];

/// One `[[path_map]]` rule as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathRow {
  pub from: String,
  pub to: String,
}

/// Every setting, as the window edits it.
#[derive(Clone, PartialEq, Eq)]
pub struct Form {
  pub jellyfin_url: String,
  pub device_name: String,
  pub profile: String,
  pub mcws_url: String,
  pub zone_id: String,
  pub timeout_ms: String,
  /// Empty: never start MC.
  pub exe_path: String,
  /// Empty: no Media Network authentication.
  pub username: String,
  /// A new Media Network password to save in Credential Manager; empty
  /// keeps the saved one. (Filled from the file only for a password still
  /// there from before 0.6.0, so saving moves it.)
  pub password: String,
  pub path_map: Vec<PathRow>,
  pub poll_interval_ms: String,
  pub progress_interval_s: String,
  pub minimize_after_playback: bool,
  pub tracks_from_jellyfin: bool,
  /// Start Jellyfin Media Player once the bridge is registered.
  pub start_player: bool,
  pub player_path: String,
  pub hang_after_s: String,
  pub hang_while_starting_s: String,
  pub max_restarts_per_hour: String,
  pub log_level: String,
  pub keep_days: String,
}

/// The password never reaches a log.
impl std::fmt::Debug for Form {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Form")
      .field("jellyfin_url", &self.jellyfin_url)
      .field("mcws_url", &self.mcws_url)
      .field("username", &self.username)
      .field(
        "password",
        &(!self.password.is_empty()).then_some("<redacted>"),
      )
      .field("path_map", &self.path_map)
      .finish_non_exhaustive()
  }
}

impl Form {
  pub fn from_config(config: &Config) -> Self {
    // Not configured yet: the defaults a `[jellyfin]` section would get.
    let jellyfin = config
      .jellyfin
      .clone()
      .unwrap_or_else(|| config::JellyfinSection {
        url: String::new(),
        device_name: config::default_device_name(),
        profile: config::default_profile(),
      });
    Self {
      jellyfin_url: jellyfin.url,
      device_name: jellyfin.device_name,
      profile: jellyfin.profile,
      mcws_url: config.mcws.url.clone(),
      zone_id: config.mcws.zone_id.to_string(),
      timeout_ms: config.mcws.timeout_ms.to_string(),
      exe_path: config
        .mcws
        .exe_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default(),
      username: config.mcws.username.clone().unwrap_or_default(),
      password: config.mcws.password.clone().unwrap_or_default(),
      path_map: config
        .path_map
        .iter()
        .map(|rule| PathRow {
          from: rule.from.clone(),
          to: rule.to.clone(),
        })
        .collect(),
      poll_interval_ms: config.bridge.poll_interval_ms.to_string(),
      progress_interval_s: config.bridge.progress_interval_s.to_string(),
      minimize_after_playback: config.bridge.minimize_after_playback,
      tracks_from_jellyfin: config.bridge.tracks_from_jellyfin,
      start_player: config.jellyfin_player.start,
      player_path: config.jellyfin_player.exe_path.display().to_string(),
      hang_after_s: config.watchdog.hang_after_s.to_string(),
      hang_while_starting_s: config.watchdog.hang_while_starting_s.to_string(),
      max_restarts_per_hour: config.watchdog.max_restarts_per_hour.to_string(),
      log_level: config.log.level.trim().to_ascii_lowercase(),
      keep_days: config.log.keep_days.to_string(),
    }
  }

  /// The config file's text with this form's settings written in. `original`
  /// is the file as it is (or the example config for a new file). The result
  /// is checked as the bridge would load it; the error says what's wrong.
  pub fn apply(&self, original: &str, path: &Path) -> Result<String, String> {
    let mut doc: DocumentMut = original
      .parse()
      .map_err(|err| format!("the config file can't be read as TOML: {err}"))?;

    let jellyfin = table(&mut doc, "jellyfin");
    jellyfin["url"] = value(self.jellyfin_url.trim());
    jellyfin["device_name"] = value(self.device_name.trim());
    jellyfin["profile"] = value(self.profile.trim());

    let mcws = table(&mut doc, "mcws");
    mcws["url"] = value(self.mcws_url.trim());
    mcws["zone_id"] = value(number(&self.zone_id, "JRiver zone ID")?);
    mcws["timeout_ms"] = value(number(&self.timeout_ms, "JRiver timeout")?);
    set_optional(mcws, "exe_path", &self.exe_path);
    set_optional(mcws, "username", &self.username);
    // The password goes to Credential Manager ([`store_password`]), never
    // into the file; one left there from before 0.6.0 is removed.
    mcws.remove("password");

    let bridge = table(&mut doc, "bridge");
    bridge["poll_interval_ms"] = value(number(&self.poll_interval_ms, "Poll interval")?);
    bridge["progress_interval_s"] = value(number(&self.progress_interval_s, "Progress report")?);
    bridge["minimize_after_playback"] = value(self.minimize_after_playback);
    bridge["tracks_from_jellyfin"] = value(self.tracks_from_jellyfin);

    let player = table(&mut doc, "jellyfin_player");
    player["start"] = value(self.start_player);
    player["exe_path"] = value(self.player_path.trim());

    let watchdog = table(&mut doc, "watchdog");
    watchdog["hang_after_s"] = value(number(&self.hang_after_s, "Hang time")?);
    watchdog["hang_while_starting_s"] = value(number(
      &self.hang_while_starting_s,
      "Hang time while a cast starts",
    )?);
    watchdog["max_restarts_per_hour"] =
      value(number(&self.max_restarts_per_hour, "Restarts per hour")?);

    let log = table(&mut doc, "log");
    log["level"] = value(self.log_level.trim());
    log["keep_days"] = value(number(&self.keep_days, "Days of logs kept")?);

    set_path_map(&mut doc, &self.path_map);

    let text = doc.to_string();
    config::parse(&text, path).map_err(message)?;
    Ok(text)
  }
}

/// Where the Media Network password of these settings is kept: `None`
/// without a user name (no authentication).
pub fn password_entry(form: &Form) -> Option<McwsSecret> {
  let user = form.username.trim();
  (!user.is_empty()).then(|| McwsSecret::new(&form.mcws_url, user))
}

/// What [`store_password`] did.
#[derive(Debug)]
pub struct PasswordStored {
  /// A password is now saved for the form's user name and MCWS URL.
  pub saved: bool,
  /// The previous entry, no longer used: delete it once the config is
  /// written ([`drop_stale_password`]).
  pub stale: Option<McwsSecret>,
}

/// Before the form is written: keep its Media Network password. A typed
/// password is saved; with none typed, one still in the file (from before
/// 0.6.0) is kept, and a saved one moves along if the user name or MCWS URL
/// changed since `previous` (the form as last saved). Nothing is deleted
/// here, so a failed config write loses nothing.
pub fn store_password(
  form: &Form,
  previous: &Form,
  secrets: &impl Secrets,
) -> Result<PasswordStored, String> {
  let fail = |err: CredentialError| err.to_string();
  let old = password_entry(previous);
  let Some(new) = password_entry(form) else {
    if !form.password.is_empty() {
      return Err("a Media Network password needs a user name".to_owned());
    }
    return Ok(PasswordStored {
      saved: false,
      stale: old,
    });
  };
  // "Empty keeps the saved one", also for one still in the file.
  let password = if form.password.is_empty() {
    &previous.password
  } else {
    &form.password
  };
  let mut wrote = false;
  if !password.is_empty() {
    secrets.save(&new, password).map_err(fail)?;
    wrote = true;
  } else if let Some(old) = old.as_ref().filter(|old| **old != new) {
    if let Some(password) = secrets.load(old).map_err(fail)? {
      secrets.save(&new, &password).map_err(fail)?;
      wrote = true;
    }
  }
  // Only for showing "saved": a failed read never fails the save.
  let saved = wrote
    || match secrets.load(&new) {
      Ok(found) => found.is_some(),
      Err(err) => {
        log::warn!(
          "can't read the Media Network password for {}: {err}",
          new.account()
        );
        false
      }
    };
  Ok(PasswordStored {
    saved,
    stale: old.filter(|old| *old != new),
  })
}

/// After the config was written: delete the entry it no longer names. A
/// failure leaves an unused entry, nothing worse.
pub fn drop_stale_password(
  stale: Option<McwsSecret>,
  secrets: &impl Secrets,
) -> Result<(), String> {
  match stale {
    Some(entry) => secrets
      .delete(&entry)
      .map(drop)
      .map_err(|err| format!("{} ({})", err, entry.account())),
    None => Ok(()),
  }
}

/// The config file's text without a Media Network password (for moving one
/// left from before 0.6.0 into Credential Manager). `None` if there's none.
pub fn without_password(text: &str) -> Option<String> {
  let mut doc: DocumentMut = text.parse().ok()?;
  let mcws = doc.get_mut("mcws")?.as_table_mut()?;
  mcws.remove("password")?;
  Some(doc.to_string())
}

/// Move a Media Network password still in the config file (from before
/// 0.6.0) into Credential Manager and take it out of the file. The file is
/// changed only once the password is saved. `Ok(true)` only when the file
/// was changed.
pub fn move_password_to_credentials(
  path: &Path,
  config: &Config,
  secrets: &impl Secrets,
) -> Result<bool, String> {
  let (Some(user), Some(password)) = (&config.mcws.username, &config.mcws.password) else {
    return Ok(false);
  };
  secrets
    .save(&McwsSecret::new(&config.mcws.url, user), password)
    .map_err(|err| err.to_string())?;
  let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
  match without_password(&text) {
    Some(text) => {
      write_config(path, &text).map_err(|err| err.to_string())?;
      Ok(true)
    }
    None => Ok(false),
  }
}

/// Write the config file: beside it, synced, then renamed over it, so a
/// failure leaves the old file.
pub fn write_config(path: &Path, text: &str) -> std::io::Result<()> {
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir)?;
  }
  // Per process: the window and the bridge may both write.
  let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
  let written = (|| {
    let mut file = std::fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
  })();
  if written.is_err() {
    let _ = std::fs::remove_file(&tmp);
  }
  written
}

/// What the window starts from. (No `Debug`: `text` holds the file, with
/// any Media Network password.)
pub struct Loaded {
  pub form: Form,
  /// The file's text as read (`None`: no file yet), to tell on Save whether
  /// it was changed outside the window meanwhile.
  pub text: Option<String>,
  /// Something to tell the user about the file.
  pub problem: Option<String>,
}

/// Read the config file for the window. A file with an error is shown as
/// written (so saving doesn't lose its rules); only text that isn't TOML at
/// all falls back to defaults.
pub fn load(path: &Path) -> Loaded {
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      let example = config::parse(config::EXAMPLE, path).unwrap_or_default();
      return Loaded {
        form: Form::from_config(&example),
        text: None,
        problem: Some(
          "There's no config file yet: these are the example settings. Saving creates it."
            .to_owned(),
        ),
      };
    }
    Err(err) => {
      return Loaded {
        form: Form::from_config(&Config::default()),
        text: None,
        problem: Some(format!("Can't read {}: {err}", path.display())),
      };
    }
  };
  match config::parse(&text, path) {
    Ok(config) => Loaded {
      form: Form::from_config(&config),
      text: Some(text),
      problem: None,
    },
    Err(err) => match config::parse_unchecked(&text) {
      Ok(config) => Loaded {
        form: Form::from_config(&config),
        text: Some(text),
        problem: Some(format!(
          "The config file has an error (shown as written): {}. Fix it here and save.",
          message(err)
        )),
      },
      Err(_) => Loaded {
        form: Form::from_config(&Config::default()),
        text: Some(text),
        problem: Some(format!(
          "The config file can't be read: {}. Fix it with \"Open config file\", then open Settings again.",
          message(err)
        )),
      },
    },
  }
}

/// A config error without the file path (the window names the file).
fn message(err: config::ConfigError) -> String {
  match err {
    config::ConfigError::Invalid { message, .. } | config::ConfigError::Parse { message, .. } => {
      message
    }
    other => other.to_string(),
  }
}

/// The `[name]` table, made if missing (or replaced if it isn't a table).
fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
  let item = doc.as_table_mut().entry(name).or_insert(Item::None);
  if !item.is_table() {
    *item = Item::Table(Table::new());
  }
  match item {
    Item::Table(table) => table,
    _ => unreachable!("made a table above"),
  }
}

/// Set `key`, or remove it when the text is empty.
fn set_optional(table: &mut Table, key: &str, text: &str) {
  let text = text.trim();
  if text.is_empty() {
    table.remove(key);
  } else {
    table[key] = value(text);
  }
}

/// A whole number from a field, or a message naming the field.
fn number(text: &str, field: &str) -> Result<i64, String> {
  text
    .trim()
    .parse()
    .map_err(|_| format!("{field} must be a whole number, got {:?}", text.trim()))
}

/// Replace the `[[path_map]]` rules, keeping the comment above the first.
fn set_path_map(doc: &mut DocumentMut, rows: &[PathRow]) {
  let comment = doc
    .get("path_map")
    .and_then(Item::as_array_of_tables)
    .and_then(|rules| rules.get(0))
    .and_then(|first| first.decor().prefix().cloned());
  let mut rules = ArrayOfTables::new();
  for (i, row) in rows
    .iter()
    .filter(|row| !(row.from.trim().is_empty() && row.to.trim().is_empty()))
    .enumerate()
  {
    let mut rule = Table::new();
    rule["from"] = value(row.from.trim());
    rule["to"] = value(row.to.trim());
    if i == 0 {
      if let Some(comment) = &comment {
        rule.decor_mut().set_prefix(comment.clone());
      }
    }
    rules.push(rule);
  }
  doc.as_table_mut().remove("path_map");
  if !rules.is_empty() {
    doc.as_table_mut()["path_map"] = Item::ArrayOfTables(rules);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn p() -> &'static Path {
    Path::new("test.toml")
  }

  fn example() -> Config {
    config::parse(config::EXAMPLE, p()).unwrap()
  }

  #[test]
  fn an_unchanged_form_keeps_the_config_and_its_comments() {
    let form = Form::from_config(&example());
    let text = form.apply(config::EXAMPLE, p()).unwrap();
    assert_eq!(config::parse(&text, p()).unwrap(), example());
    assert!(text.contains("# The name in Jellyfin's cast menu."));
    assert!(text.contains("# Server path (as Jellyfin reports it)"));
  }

  #[test]
  fn changes_are_written_and_empty_optional_fields_removed() {
    let mut form = Form::from_config(&example());
    form.device_name = "Living room".to_owned();
    form.exe_path = String::new();
    form.hang_while_starting_s = "45".to_owned();
    form.max_restarts_per_hour = "0".to_owned();
    form.minimize_after_playback = false;
    form.start_player = false;
    form.player_path = r"D:\JMP\JellyfinMediaPlayer.exe".to_owned();
    form.path_map.push(PathRow {
      from: "/media/music".to_owned(),
      to: r"\\nas\music".to_owned(),
    });
    // A blank row is ignored.
    form.path_map.push(PathRow::default());
    let text = form.apply(config::EXAMPLE, p()).unwrap();
    let config = config::parse(&text, p()).unwrap();
    assert_eq!(config.jellyfin.unwrap().device_name, "Living room");
    assert_eq!(config.mcws.exe_path, None);
    assert_eq!(config.watchdog.hang_while_starting_s, 45);
    assert_eq!(config.watchdog.max_restarts_per_hour, 0);
    assert!(!config.bridge.minimize_after_playback);
    assert!(!config.jellyfin_player.start);
    assert_eq!(
      config.jellyfin_player.exe_path,
      std::path::PathBuf::from(r"D:\JMP\JellyfinMediaPlayer.exe")
    );
    assert_eq!(config.path_map.len(), 3);
    assert_eq!(config.path_map[2].to, r"\\nas\music");
    assert!(text.contains("# Server path (as Jellyfin reports it)"));
  }

  #[test]
  fn what_the_bridge_would_refuse_is_not_written() {
    let mut form = Form::from_config(&example());
    form.zone_id = "-1".to_owned();
    let err = form.apply(config::EXAMPLE, p()).unwrap_err();
    assert!(err.contains("zone_id"), "{err}");
    form.zone_id = "zero".to_owned();
    let err = form.apply(config::EXAMPLE, p()).unwrap_err();
    assert!(err.contains("JRiver zone ID"), "{err}");
    let mut form = Form::from_config(&example());
    form.hang_after_s = "0".to_owned();
    assert!(form.apply(config::EXAMPLE, p()).is_err());
  }

  fn temp_file(text: Option<&str>) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("bridge-settings-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    if let Some(text) = text {
      std::fs::write(&path, text).unwrap();
    }
    (dir, path)
  }

  #[test]
  fn a_config_with_an_error_is_shown_as_written_so_saving_keeps_its_rules() {
    let broken = config::EXAMPLE.replace("keep_days = 14", "keep_days = 0");
    let (dir, path) = temp_file(Some(&broken));
    let loaded = load(&path);
    assert!(loaded.problem.unwrap().contains("keep_days"));
    assert_eq!(loaded.form.path_map.len(), 2, "the rules are kept");
    assert!(!loaded.form.exe_path.is_empty());
    let mut form = loaded.form;
    form.keep_days = "14".to_owned();
    let text = form.apply(&broken, &path).unwrap();
    assert_eq!(config::parse(&text, &path).unwrap(), example());
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn loading_tells_a_missing_or_unreadable_file() {
    let (dir, path) = temp_file(None);
    let loaded = load(&path);
    assert!(loaded.text.is_none());
    assert!(loaded.problem.unwrap().contains("no config file yet"));
    assert_eq!(loaded.form, Form::from_config(&example()));
    std::fs::write(&path, "[jellyfin\nurl =").unwrap();
    let loaded = load(&path);
    assert!(loaded.problem.unwrap().contains("can't be read"));
    assert!(loaded
      .form
      .apply(loaded.text.as_deref().unwrap(), &path)
      .is_err());
    std::fs::write(&path, config::EXAMPLE).unwrap();
    let loaded = load(&path);
    assert!(loaded.problem.is_none());
    assert_eq!(loaded.text.as_deref(), Some(config::EXAMPLE));
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn the_password_never_goes_into_the_file() {
    let mut form = Form::from_config(&example());
    form.username = "mc".to_owned();
    form.password = " secret ".to_owned();
    let text = form.apply(config::EXAMPLE, p()).unwrap();
    assert!(!text.contains("secret"));
    let config = config::parse(&text, p()).unwrap();
    assert_eq!(config.mcws.username.as_deref(), Some("mc"));
    assert_eq!(config.mcws.password, None);
    assert!(!format!("{form:?}").contains("secret"), "and never logged");
  }

  #[test]
  fn a_password_left_in_the_file_is_offered_for_moving_and_removed() {
    let old = config::EXAMPLE.replace(
      "# username = \"...\"",
      "username = \"mc\"\npassword = \"pw\"",
    );
    let config = config::parse(&old, p()).unwrap();
    // The form carries it, so saving moves it to Credential Manager.
    assert_eq!(Form::from_config(&config).password, "pw");
    let text = without_password(&old).unwrap();
    assert!(!text.contains("\"pw\""));
    assert!(text.contains("username = \"mc\""));
    assert!(text.contains("# MC → Options → Media Network must be enabled."));
    assert_eq!(without_password(&text), None, "nothing more to move");
  }

  #[test]
  fn the_password_entry_follows_the_user_name_and_mcws_url() {
    let mut form = Form::from_config(&example());
    assert_eq!(password_entry(&form), None, "no user name: no password");
    form.username = " mc ".to_owned();
    form.mcws_url = "http://localhost:52199/MCWS/v1/".to_owned();
    assert_eq!(
      password_entry(&form).unwrap().account(),
      "mcws:mc@http://localhost:52199/mcws/v1"
    );
  }

  /// Credential Manager stand-in: names compared without regard to case,
  /// as Windows does.
  #[derive(Default)]
  struct MemorySecrets {
    saved: std::sync::Mutex<std::collections::HashMap<String, String>>,
    fail_save: bool,
    fail_load: bool,
  }

  impl Secrets for MemorySecrets {
    fn load(&self, entry: &McwsSecret) -> Result<Option<String>, CredentialError> {
      if self.fail_load {
        return Err(CredentialError::Store("unreadable".to_owned()));
      }
      let key = entry.account().to_lowercase();
      Ok(self.saved.lock().unwrap().get(&key).cloned())
    }
    fn save(&self, entry: &McwsSecret, password: &str) -> Result<(), CredentialError> {
      if self.fail_save {
        return Err(CredentialError::Store("refused".to_owned()));
      }
      let key = entry.account().to_lowercase();
      self.saved.lock().unwrap().insert(key, password.to_owned());
      Ok(())
    }
    fn delete(&self, entry: &McwsSecret) -> Result<bool, CredentialError> {
      let key = entry.account().to_lowercase();
      Ok(self.saved.lock().unwrap().remove(&key).is_some())
    }
  }

  fn with_user(user: &str, password: &str) -> Form {
    let mut form = Form::from_config(&example());
    form.username = user.to_owned();
    form.password = password.to_owned();
    form
  }

  #[test]
  fn a_typed_password_is_stored_and_an_empty_field_keeps_it() {
    let secrets = MemorySecrets::default();
    let none = Form::from_config(&example());
    let stored = store_password(&with_user("mc", "pw"), &none, &secrets).unwrap();
    assert!(stored.saved);
    assert!(stored.stale.is_none());
    // Later saves with an empty field keep it.
    let saved = with_user("mc", "");
    let stored = store_password(&saved, &saved, &secrets).unwrap();
    assert!(stored.saved);
    let entry = password_entry(&saved).unwrap();
    assert_eq!(secrets.load(&entry).unwrap().as_deref(), Some("pw"));
  }

  #[test]
  fn a_rename_moves_the_password_and_the_old_entry_goes_only_after_the_write() {
    let secrets = MemorySecrets::default();
    let previous = with_user("mc", "");
    let old = password_entry(&previous).unwrap();
    secrets.save(&old, "pw").unwrap();
    let renamed = with_user("mc2", "");
    let stored = store_password(&renamed, &previous, &secrets).unwrap();
    let new = password_entry(&renamed).unwrap();
    assert_eq!(secrets.load(&new).unwrap().as_deref(), Some("pw"));
    // Not deleted yet: the config write may still fail.
    assert_eq!(secrets.load(&old).unwrap().as_deref(), Some("pw"));
    assert_eq!(stored.stale.as_ref(), Some(&old));
    drop_stale_password(stored.stale, &secrets).unwrap();
    assert_eq!(secrets.load(&old).unwrap(), None);
    assert_eq!(secrets.load(&new).unwrap().as_deref(), Some("pw"));
  }

  #[test]
  fn a_rename_that_only_changes_case_keeps_the_password() {
    let secrets = MemorySecrets::default();
    let previous = with_user("MC", "");
    secrets
      .save(&password_entry(&previous).unwrap(), "pw")
      .unwrap();
    let renamed = with_user("mc", "");
    let stored = store_password(&renamed, &previous, &secrets).unwrap();
    assert!(stored.stale.is_none(), "one entry, never deleted");
    drop_stale_password(stored.stale, &secrets).unwrap();
    assert_eq!(
      secrets
        .load(&password_entry(&renamed).unwrap())
        .unwrap()
        .as_deref(),
      Some("pw")
    );
  }

  #[test]
  fn clearing_the_user_name_removes_the_password_after_the_write() {
    let secrets = MemorySecrets::default();
    let previous = with_user("mc", "");
    let old = password_entry(&previous).unwrap();
    secrets.save(&old, "pw").unwrap();
    let stored = store_password(&with_user("", ""), &previous, &secrets).unwrap();
    assert!(!stored.saved);
    drop_stale_password(stored.stale, &secrets).unwrap();
    assert_eq!(secrets.load(&old).unwrap(), None);
  }

  #[test]
  fn an_unreadable_entry_never_fails_the_save() {
    let secrets = MemorySecrets {
      fail_load: true,
      ..MemorySecrets::default()
    };
    let saved = with_user("mc", "");
    // A typed password is saved, so it counts without reading it back.
    let stored = store_password(&with_user("mc", "pw"), &saved, &secrets).unwrap();
    assert!(stored.saved);
    // An unrelated setting saved with the password untouched: still fine.
    let stored = store_password(&saved, &saved, &secrets).unwrap();
    assert!(!stored.saved, "can't tell, so not claimed");
  }

  #[test]
  fn a_password_needs_a_user_name_and_saved_means_saved() {
    let secrets = MemorySecrets::default();
    let none = Form::from_config(&example());
    let err = store_password(&with_user("", "pw"), &none, &secrets).unwrap_err();
    assert!(err.contains("needs a user name"), "{err}");
    // A user name but no password ever saved: not "saved".
    let stored = store_password(&with_user("mc", ""), &none, &secrets).unwrap();
    assert!(!stored.saved);
  }

  #[test]
  fn a_password_from_the_file_is_kept_when_the_field_is_cleared() {
    let secrets = MemorySecrets::default();
    // As loaded from a pre-0.6.0 file: the field holds the file's password.
    let previous = with_user("mc", "pw");
    let stored = store_password(&with_user("mc", ""), &previous, &secrets).unwrap();
    assert!(stored.saved);
    assert_eq!(
      secrets
        .load(&password_entry(&previous).unwrap())
        .unwrap()
        .as_deref(),
      Some("pw")
    );
  }

  #[test]
  fn the_move_leaves_the_file_alone_unless_the_password_is_saved() {
    let old = config::EXAMPLE.replace(
      "# username = \"...\"",
      "username = \"mc\"\npassword = \"pw\"",
    );
    let (dir, path) = temp_file(Some(&old));
    let config = config::parse(&old, &path).unwrap();
    let failing = MemorySecrets {
      fail_save: true,
      ..MemorySecrets::default()
    };
    assert!(move_password_to_credentials(&path, &config, &failing).is_err());
    assert_eq!(
      std::fs::read_to_string(&path).unwrap(),
      old,
      "file untouched"
    );
    let secrets = MemorySecrets::default();
    assert!(move_password_to_credentials(&path, &config, &secrets).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("\"pw\""));
    let moved = config::parse(&text, &path).unwrap();
    assert_eq!(moved.mcws.username.as_deref(), Some("mc"));
    assert_eq!(moved.mcws.password, None);
    assert_eq!(
      secrets
        .load(&McwsSecret::new(&moved.mcws.url, "mc"))
        .unwrap()
        .as_deref(),
      Some("pw")
    );
    assert!(
      moved.mcws_config(None).credentials.is_none(),
      "user name only: no auth"
    );
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn a_new_config_starts_from_an_empty_one() {
    let form = Form::from_config(&Config::default());
    assert_eq!(form.device_name, "JRiver (HTPC)");
    let mut form = form;
    form.jellyfin_url = "http://jf:8096".to_owned();
    let text = form.apply("", p()).unwrap();
    let config = config::parse(&text, p()).unwrap();
    assert_eq!(config.jellyfin.unwrap().url, "http://jf:8096");
  }
}
