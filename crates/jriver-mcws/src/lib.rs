//! MCWS client and player for JRiver Media Center.
//!
//! Only the calls SPEC.md allows towards MC exist here: see [`Function`].
//! Nothing in this crate changes MC settings, DSP, audio output or JRVR
//! profiles.

mod client;
mod error;
mod function;
mod media;
mod model;
mod player;
mod response;

pub use client::{Credentials, McwsClient, McwsConfig, DEFAULT_TIMEOUT};
pub use error::McwsError;
pub use function::Function;
pub use media::same_media;
pub use model::{AliveInfo, PauseAction, PlayState, PlaybackInfo, UiInfo, UI_MODE_DISPLAY};
pub use player::{JRiverPlayer, Mcws, PlayerOptions, StartError, StartedPlayback};
