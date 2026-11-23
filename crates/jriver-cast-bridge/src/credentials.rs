//! The saved Jellyfin login, kept in Windows Credential Manager, never in
//! the config file.
//!
//! One entry per server and profile (service `jriver-cast-bridge`, account
//! `<profile>@<server URL>`), so a test login never replaces the real one.
//! The entry also holds the device ID: a token belongs to one device ID, and
//! keeping it stable keeps the cast target the same across logins.

use std::fmt;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::{
  config::{JellyfinSection, McwsSection},
  jellyfin::{AccessToken, Identity, JellyfinClient, JellyfinError, Login, DEFAULT_TIMEOUT},
};

pub const SERVICE: &str = "jriver-cast-bridge";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLogin {
  pub user_id: String,
  pub user_name: String,
  pub device_id: String,
  pub access_token: String,
}

impl StoredLogin {
  pub fn new(login: &Login, device_id: &str) -> Self {
    Self {
      user_id: login.user_id.clone(),
      user_name: login.user_name.clone(),
      device_id: device_id.to_owned(),
      access_token: login.token.expose().to_owned(),
    }
  }

  pub fn login(&self) -> Login {
    Login {
      user_id: self.user_id.clone(),
      user_name: self.user_name.clone(),
      token: AccessToken::new(self.access_token.clone()),
    }
  }

  fn valid(&self) -> bool {
    !self.user_id.is_empty() && !self.device_id.is_empty() && !self.access_token.is_empty()
  }
}

impl fmt::Debug for StoredLogin {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("StoredLogin")
      .field("user_id", &self.user_id)
      .field("user_name", &self.user_name)
      .field("device_id", &self.device_id)
      .field("access_token", &"<redacted>")
      .finish()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
  #[error("Windows Credential Manager: {0}")]
  Store(String),
  #[error("the saved login for {account} is damaged; log in again")]
  Corrupt { account: String },
}

/// A new device ID for a first login.
pub fn new_device_id() -> String {
  format!("{SERVICE}-{}", uuid::Uuid::new_v4().simple())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStore {
  account: String,
}

impl CredentialStore {
  pub fn new(server_url: &str, profile: &str) -> Self {
    Self {
      account: format!("{profile}@{}", server_url.trim().trim_end_matches('/')),
    }
  }

  /// The Credential Manager account name, for messages.
  pub fn account(&self) -> &str {
    &self.account
  }

  fn entry(&self) -> Result<keyring::Entry, CredentialError> {
    keyring::Entry::new(SERVICE, &self.account)
      .map_err(|err| CredentialError::Store(err.to_string()))
  }

  pub fn load(&self) -> Result<Option<StoredLogin>, CredentialError> {
    let secret = match self.entry()?.get_password() {
      Ok(secret) => secret,
      Err(keyring::Error::NoEntry) => return Ok(None),
      Err(err) => return Err(CredentialError::Store(err.to_string())),
    };
    decode(&secret)
      .ok_or_else(|| CredentialError::Corrupt {
        account: self.account.clone(),
      })
      .map(Some)
  }

  pub fn save(&self, login: &StoredLogin) -> Result<(), CredentialError> {
    let secret =
      serde_json::to_string(login).map_err(|err| CredentialError::Store(err.to_string()))?;
    self
      .entry()?
      .set_password(&secret)
      .map_err(|err| CredentialError::Store(err.to_string()))
  }

  /// `Ok(false)` if nothing was saved.
  pub fn delete(&self) -> Result<bool, CredentialError> {
    match self.entry()?.delete_credential() {
      Ok(()) => Ok(true),
      Err(keyring::Error::NoEntry) => Ok(false),
      Err(err) => Err(CredentialError::Store(err.to_string())),
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum LogInError {
  #[error(transparent)]
  Jellyfin(#[from] JellyfinError),
  #[error(transparent)]
  Store(#[from] CredentialError),
}

/// Log in to the configured server and save the login. The device ID of an
/// earlier login is kept, so the cast target stays the same.
pub async fn log_in(
  jellyfin: &JellyfinSection,
  user: &str,
  password: &str,
) -> Result<(StoredLogin, CredentialStore), LogInError> {
  let store = CredentialStore::new(&jellyfin.url, &jellyfin.profile);
  let previous = match store.load() {
    Ok(previous) => previous,
    Err(err) => {
      warn!("{err}; starting over with a new device ID");
      None
    }
  };
  let device_id = previous
    .as_ref()
    .map_or_else(new_device_id, |previous| previous.device_id.clone());
  let client = JellyfinClient::new(
    &jellyfin.url,
    Identity {
      device_name: jellyfin.device_name.clone(),
      device_id: device_id.clone(),
    },
    None,
    DEFAULT_TIMEOUT,
  )?;
  let login = client.login(user, password).await?;
  let stored = StoredLogin::new(&login, &device_id);
  store.save(&stored)?;
  // The server revokes the old token itself only for the same user (and
  // device); another user's would be left valid with nothing to use it.
  if let Some(previous) = previous.filter(|p| must_revoke(p, &stored)) {
    revoke(jellyfin, &previous).await;
  }
  Ok((stored, store))
}

/// After a new login: the previous token is left valid by the server only
/// when it belonged to another user, and then it's revoked here.
fn must_revoke(previous: &StoredLogin, new: &StoredLogin) -> bool {
  previous.user_id != new.user_id
}

/// Log out an old login on the server; a failure is only logged.
async fn revoke(jellyfin: &JellyfinSection, previous: &StoredLogin) {
  let identity = Identity {
    device_name: jellyfin.device_name.clone(),
    device_id: previous.device_id.clone(),
  };
  let result = match JellyfinClient::new(
    &jellyfin.url,
    identity,
    Some(previous.login()),
    DEFAULT_TIMEOUT,
  ) {
    Ok(client) => client.logout().await,
    Err(err) => Err(err),
  };
  match result {
    Ok(()) => info!(
      "revoked the previous login of {} on the server",
      previous.user_name
    ),
    Err(err) => warn!(
      "couldn't revoke the previous login of {} on the server: {err}",
      previous.user_name
    ),
  }
}

/// The Media Network password (MC's MCWS authentication), kept in Credential
/// Manager like the Jellyfin login, never in the config file. One entry per
/// user name and MCWS URL: account `mcws:<user>@<MCWS URL>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McwsSecret {
  account: String,
}

impl McwsSecret {
  /// Lower case: Windows matches credential names without regard to case,
  /// so `MC` and `mc` must be one entry here too (or renaming one to the
  /// other would delete the password it just saved).
  pub fn new(mcws_url: &str, username: &str) -> Self {
    Self {
      account: format!(
        "mcws:{}@{}",
        username.trim(),
        mcws_url.trim().trim_end_matches('/')
      )
      .to_lowercase(),
    }
  }

  /// The Credential Manager account name, for messages.
  pub fn account(&self) -> &str {
    &self.account
  }

  fn entry(&self) -> Result<keyring::Entry, CredentialError> {
    keyring::Entry::new(SERVICE, &self.account)
      .map_err(|err| CredentialError::Store(err.to_string()))
  }

  pub fn load(&self) -> Result<Option<String>, CredentialError> {
    match self.entry()?.get_password() {
      Ok(password) => Ok(Some(password)),
      Err(keyring::Error::NoEntry) => Ok(None),
      Err(err) => Err(CredentialError::Store(err.to_string())),
    }
  }

  pub fn save(&self, password: &str) -> Result<(), CredentialError> {
    self
      .entry()?
      .set_password(password)
      .map_err(|err| CredentialError::Store(err.to_string()))
  }

  /// `Ok(false)` if nothing was saved.
  pub fn delete(&self) -> Result<bool, CredentialError> {
    match self.entry()?.delete_credential() {
      Ok(()) => Ok(true),
      Err(keyring::Error::NoEntry) => Ok(false),
      Err(err) => Err(CredentialError::Store(err.to_string())),
    }
  }
}

/// Where Media Network passwords are kept: Credential Manager in the bridge
/// ([`CredentialManager`]), in memory in tests.
pub trait Secrets {
  fn load(&self, entry: &McwsSecret) -> Result<Option<String>, CredentialError>;
  fn save(&self, entry: &McwsSecret, password: &str) -> Result<(), CredentialError>;
  /// `Ok(false)` if nothing was saved.
  fn delete(&self, entry: &McwsSecret) -> Result<bool, CredentialError>;
}

/// Windows Credential Manager.
#[derive(Debug, Clone, Copy, Default)]
pub struct CredentialManager;

impl Secrets for CredentialManager {
  fn load(&self, entry: &McwsSecret) -> Result<Option<String>, CredentialError> {
    entry.load()
  }
  fn save(&self, entry: &McwsSecret, password: &str) -> Result<(), CredentialError> {
    entry.save(password)
  }
  fn delete(&self, entry: &McwsSecret) -> Result<bool, CredentialError> {
    entry.delete()
  }
}

/// The Media Network password for `mcws`: from Credential Manager, or one
/// still in the config file (from before 0.6.0, until it's moved). `None`
/// without a user name. A missing or unreadable password is logged.
pub fn mcws_password(mcws: &McwsSection) -> Option<String> {
  let user = mcws.username.as_deref()?;
  if let Some(password) = &mcws.password {
    return Some(password.clone());
  }
  let secret = McwsSecret::new(&mcws.url, user);
  match secret.load() {
    Ok(Some(password)) => Some(password),
    Ok(None) => {
      warn!(
        "no Media Network password saved for {} in Credential Manager; set it in Settings",
        secret.account()
      );
      None
    }
    Err(err) => {
      warn!(
        "can't read the Media Network password for {}: {err}",
        secret.account()
      );
      None
    }
  }
}

fn decode(secret: &str) -> Option<StoredLogin> {
  serde_json::from_str::<StoredLogin>(secret)
    .ok()
    .filter(StoredLogin::valid)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn stored() -> StoredLogin {
    StoredLogin {
      user_id: "u1".to_owned(),
      user_name: "testuser".to_owned(),
      device_id: "jriver-cast-bridge-1".to_owned(),
      access_token: "secret-token".to_owned(),
    }
  }

  #[test]
  fn only_another_user_s_previous_login_is_revoked() {
    let previous = stored();
    let same_user = StoredLogin {
      access_token: "new-token".to_owned(),
      ..stored()
    };
    assert!(!must_revoke(&previous, &same_user));
    let other_user = StoredLogin {
      user_id: "u2".to_owned(),
      ..stored()
    };
    assert!(must_revoke(&previous, &other_user));
  }

  #[test]
  fn account_is_per_profile_and_server() {
    assert_eq!(
      CredentialStore::new("http://jellyfin.local:8096/", "default").account(),
      "default@http://jellyfin.local:8096"
    );
    assert_ne!(
      CredentialStore::new("http://x", "default"),
      CredentialStore::new("http://x", "verifier")
    );
  }

  #[test]
  fn round_trips_and_rejects_damaged_entries() {
    let text = serde_json::to_string(&stored()).unwrap();
    assert_eq!(decode(&text), Some(stored()));
    assert_eq!(decode("not json"), None);
    let empty_token = StoredLogin {
      access_token: String::new(),
      ..stored()
    };
    assert_eq!(decode(&serde_json::to_string(&empty_token).unwrap()), None);
  }

  #[test]
  fn debug_hides_the_token() {
    assert!(!format!("{:?}", stored()).contains("secret-token"));
    assert!(!format!("{:?}", stored().login()).contains("secret-token"));
  }

  #[test]
  fn device_ids_are_unique_and_header_safe() {
    let a = new_device_id();
    assert_ne!(a, new_device_id());
    assert!(a.starts_with("jriver-cast-bridge-"));
    crate::jellyfin::check_header_value("device ID", &a).unwrap();
  }
}
