//! The per-user data directory this suite's non-service processes keep state
//! in — one definition, several callers.
//!
//! Split out of [`crate::api_log`], which established the directory and its
//! reasoning (`embarch-api/design.md` §3 decision 43's 2026-08-25
//! correction: per-user, not machine-wide, because `/var/lib` is root-owned
//! and `embarch-api` runs as the engineer). It stopped being a logging
//! detail the moment a second thing needed the same directory —
//! `embarch-ui`'s recent-projects list (`embarch-ui/design.md` §3 decision
//! 14) — and it lives in *this* crate for the reason `api_log`'s own header
//! gives: `embarch-core-client` is the only crate both `embarch-api` and
//! `embarch-ui` depend on, and a path two repos resolve independently is a
//! path they will eventually resolve differently.
//!
//! **Not created here.** Every caller creates whatever subdirectory it
//! needs, on first write, and treats a missing directory as "nothing stored
//! yet" rather than an error.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `%LOCALAPPDATA%\embarch`.
#[cfg(windows)]
pub fn user_data_dir() -> Result<PathBuf> {
    let local_app_data = std::env::var("LOCALAPPDATA")
        .context("LOCALAPPDATA environment variable is not set")?;
    Ok(PathBuf::from(local_app_data).join("embarch"))
}

/// `$XDG_DATA_HOME/embarch`, or `$HOME/.local/share/embarch` — the XDG
/// default, spelled out rather than pulling in a crate for two lines.
#[cfg(unix)]
pub fn user_data_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return Ok(PathBuf::from(xdg).join("embarch"));
        }
    }
    let home = std::env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home).join(".local").join("share").join("embarch"))
}
