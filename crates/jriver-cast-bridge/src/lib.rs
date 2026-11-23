//! jriver-cast-bridge: makes JRiver Media Center a Jellyfin cast target.
//!
//! - [`jellyfin`]: the bridge's own small Jellyfin client (login,
//!   registration, item lookup, reports). It never requests streams.
//! - [`credentials`]: the login, kept in Windows Credential Manager.
//! - [`resolve`] and [`path_map`]: from a cast item to the UNC path JRiver
//!   opens.
//! - [`controller`]: the playback state machine (SPEC.md "Playback flow").
//! - [`reporter`]: playback reports, sent in order in the background.
//! - [`jriver`]: JRiver as the player, including starting MC.
//! - [`status`], [`logging`], [`instance`]: tray status, log files and the
//!   single-instance lock for the `jriver-cast-bridge` program.
//!
//! JRiver is driven through [`jriver_mcws`]; Jellyfin's commands
//! arrive over the WebSocket in [`socket`].

pub mod config;
pub mod controller;
pub mod credentials;
pub mod icon;
pub mod instance;
pub mod ipc;
pub mod jellyfin;
pub mod jriver;
pub mod logging;
pub mod path_map;
pub mod player_start;
pub mod process;
pub mod record;
pub mod reporter;
pub mod resolve;
pub mod service;
pub mod settings;
pub mod socket;
pub mod status;
pub mod tracks;
pub mod units;
