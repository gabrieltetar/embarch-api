//! Reading the facts about this machine that topology detection needs.
//!
//! Kept out of `topology.rs` so that module stays pure and copyable
//! (its mirrored-module rules). Everything here is a thin, unavoidably
//! platform-specific read; the decisions made from it are all next door.

use std::process::Command;

use crate::topology::detect_wsl2;

/// Is this a WSL2 guest?
pub fn under_wsl2() -> bool {
    let proc_version = std::fs::read_to_string("/proc/version").ok();
    let wsl_distro = std::env::var("WSL_DISTRO_NAME").ok();
    detect_wsl2(proc_version.as_deref(), wsl_distro.as_deref())
}

/// The default gateway, i.e. the Windows host of this WSL2 guest.
///
/// Only meaningful under WSL2, and only called there — `ip` doesn't exist on
/// Windows or macOS. Every failure (no `ip` binary, nonzero exit, no default
/// route, no `via`) collapses to `None`: the caller's answer is the same
/// either way, which is to skip the gateway candidate.
pub fn default_gateway() -> Option<String> {
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    crate::topology::parse_default_gateway(&String::from_utf8_lossy(&output.stdout))
}
