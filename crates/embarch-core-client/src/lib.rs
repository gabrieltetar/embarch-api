//! The one implementation of "how does an EmbArch process reach
//! embarch-core over HTTP+Bearer" — extracted 2026-08-24 out of
//! `embarch-api`'s own `core_client.rs`/`config.rs::CoreConfig`/
//! `token_discovery.rs` so `embarch-api` and `embarch-ui` depend on the same
//! crate instead of `embarch-ui` growing an independent, duplicated client.
//! See `embarch-ui` decision 5 for the full rationale.

use anyhow::Result;
use serde::Deserialize;

pub mod api_log;
pub mod attest;
pub mod call_trace;
pub mod desk_discovery;
pub mod client;
pub mod config_discovery;
pub mod lease_gate;
pub mod sse;
pub mod study_events;
pub mod token_discovery;
pub use token_discovery::TokenScope;
pub mod user_dirs;
pub mod version;

pub use client::*;
// The SSE half is re-exported at the crate root alongside `client::*` so a
// caller writes `embarch_core_client::FollowItem` next to
// `embarch_core_client::CoreClient`, rather than having to know which of the
// two modules a type happens to live in. `sse` itself is deliberately *not*
// re-exported: it is the wire-format layer, and a caller consuming study
// events should never need to name a raw `SseFrame`.
pub use study_events::{
    is_terminal_status, FollowItem, FollowMode, FollowOptions, FollowOutcome, StreamEnd,
    StudyEvent, StudyEventStream, StudyStreamItem, StudyStreamNext, LAGGED_NOTE,
};

fn default_status_timeout_secs() -> u64 {
    10
}
fn default_reset_timeout_secs() -> u64 {
    10
}
fn default_flash_timeout_secs() -> u64 {
    120
}

/// Longer than a flash: a bootload waits on the DUT re-enumerating twice
/// and, at the default buffer size, sends a request per 169 bytes. Core's
/// own phase timeouts are placeholders until measured (`embarch-core`
/// decision 77), so this is too.
fn default_bootload_timeout_secs() -> u64 {
    300
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
    /// (decision 11). `"auto"` exists because the WSL2
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
    /// The person's token, inline. Only a client built with
    /// [`TokenScope::Person`] reads it; the MCP server never does.
    #[serde(default)]
    pub person_token: Option<String>,
    /// Env var holding the person's token; wins over `person_token`.
    #[serde(default)]
    pub person_token_env: Option<String>,
    #[serde(default = "default_status_timeout_secs")]
    pub status_timeout_secs: u64,
    #[serde(default = "default_reset_timeout_secs")]
    pub reset_timeout_secs: u64,
    #[serde(default = "default_flash_timeout_secs")]
    pub flash_timeout_secs: u64,
    /// `POST /bootload` alone.
    #[serde(default = "default_bootload_timeout_secs")]
    pub bootload_timeout_secs: u64,
    #[serde(default = "default_serial_timeout_secs")]
    pub serial_timeout_secs: u64,
    /// Shared by `post_study`, `get_study_status` and `get_study_stream` —
    /// unlike build/flash/reset/serial-log, these don't warrant separate
    /// knobs: `POST /study` returns immediately (async,
    /// `embarch-study-designer` decision 9), status polling is a cheap read,
    /// and a capture download is bounded by the same
    /// `limits::MAX_STEPS_PER_STUDY`-sized study that produced it.
    /// **Not** `study_streams`, which reuses `status_timeout` instead (a
    /// small on-disk index read, not a capture download).
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

    /// [`CoreConfig::resolve_token`] for either scope. The person's falls
    /// back to `EMBARCH_PERSON_TOKEN`, then `person-token` beside the
    /// agent's file, and never to the agent's token.
    pub fn resolve_token_for(&self, scope: TokenScope) -> Result<String> {
        match scope {
            TokenScope::Agent => self.resolve_token(),
            TokenScope::Person => token_discovery::resolve_token_for(
                TokenScope::Person,
                self.person_token.clone(),
                self.person_token_env.clone(),
            ),
        }
    }

    /// Whether the token for `scope` is one the caller named (config or its
    /// named env var) rather than discovered from the machine-wide file.
    pub fn token_is_explicit(&self, scope: TokenScope) -> bool {
        let (token, env) = match scope {
            TokenScope::Agent => (&self.token, &self.token_env),
            TokenScope::Person => (&self.person_token, &self.person_token_env),
        };
        token.is_some() || env.as_ref().is_some_and(|v| std::env::var(v).is_ok()) || (scope == TokenScope::Person && token_discovery::person_env_set())
    }

    /// Is Core's address to be discovered rather than declared?
    pub fn is_auto(&self) -> bool {
        self.base_url.trim().eq_ignore_ascii_case("auto")
    }
}
