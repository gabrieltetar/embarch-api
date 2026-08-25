//! The one implementation of "how does an EmbArch process reach
//! embarch-core over HTTP+Bearer" — extracted 2026-08-24 out of
//! `embarch-api`'s own `core_client.rs`/`config.rs::CoreConfig`/
//! `token_discovery.rs` so `embarch-api` and `embarch-ui` depend on the same
//! crate instead of `embarch-ui` growing an independent, duplicated client.
//! See `embarch-doc/embarch-ui/design.md` §3 decision 5 and
//! `embarch-doc/embarch-ui/milestone-1.md` §4.1 for the full rationale.

use anyhow::Result;
use serde::Deserialize;

pub mod client;
pub mod token_discovery;

pub use client::*;

fn default_status_timeout_secs() -> u64 {
    10
}
fn default_reset_timeout_secs() -> u64 {
    10
}
fn default_flash_timeout_secs() -> u64 {
    120
}
fn default_serial_timeout_secs() -> u64 {
    15
}
fn default_study_timeout_secs() -> u64 {
    30
}

fn default_core_port() -> u16 {
    embarch_topology::software::DEFAULT_CORE_PORT
}

/// Config needed to reach embarch-core: its own dedicated table (`[core]`
/// in embarch-api's TOML today), not embarch-api-specific project/build
/// config, which stays out of this crate entirely.
#[derive(Debug, Deserialize)]
pub struct CoreConfig {
    /// Core's base URL, or the literal `"auto"` to resolve it at first use
    /// (`embarch-api/design.md` §3.11). `"auto"` exists because the WSL2
    /// host-gateway address changes on every WSL restart, so any literal IP
    /// written here is guaranteed to go stale — and did, before this field
    /// accepted it.
    pub base_url: String,
    /// Only consulted by `base_url = "auto"`, as its last candidate: a Core
    /// on a genuinely separate machine.
    #[serde(default)]
    pub host: Option<String>,
    /// Only consulted by `base_url = "auto"`, when building candidates.
    #[serde(default = "default_core_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_status_timeout_secs")]
    pub status_timeout_secs: u64,
    #[serde(default = "default_reset_timeout_secs")]
    pub reset_timeout_secs: u64,
    #[serde(default = "default_flash_timeout_secs")]
    pub flash_timeout_secs: u64,
    #[serde(default = "default_serial_timeout_secs")]
    pub serial_timeout_secs: u64,
    /// Shared by all four `/study` endpoints (`post_study`, `get_study_status`,
    /// `get_study_power_data`, `get_study_waveform_data`) — unlike
    /// build/flash/reset/serial-log, these don't warrant separate knobs:
    /// `POST /study` returns immediately (async, `embarch-study-designer`
    /// design.md §3 decision 9), status polling is a cheap read, and the
    /// power/waveform CSV downloads are bounded by the same
    /// `limits::MAX_STEPS_PER_STUDY`-sized study that produced them.
    #[serde(default = "default_study_timeout_secs")]
    pub study_timeout_secs: u64,
}

impl CoreConfig {
    /// Resolve the bearer token: `token_env` wins if set, then inline
    /// `token`, then the machine-wide token file embarch-core generates
    /// (see `token_discovery`) — so a secret never has to live in the
    /// config file at all, and a fresh embarch-core can be discovered with
    /// no config changes.
    pub fn resolve_token(&self) -> Result<String> {
        token_discovery::resolve_token(self.token.clone(), self.token_env.clone())
    }

    /// Is Core's address to be discovered rather than declared?
    pub fn is_auto(&self) -> bool {
        self.base_url.trim().eq_ignore_ascii_case("auto")
    }
}
