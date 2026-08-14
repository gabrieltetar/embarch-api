//! Reading the facts about this machine that topology detection needs.
//!
//! Kept out of `topology.rs` so that module stays pure and copyable
//! (its mirrored-module rules). Everything here is a thin, unavoidably
//! platform-specific read; the decisions made from it are all next door.

use std::path::Path;
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

/// This WSL2 guest's distro name, as `embarch-umbrella`'s `init.rs` reads it
/// once at scaffold time — read here at call time instead, for a
/// `discovery = "zephyr-west"` project's per-target `artifact_path_for_core`
/// (`design.md` §3 decision 12, `embarch-umbrella/design.md` §3 decision 13's
/// original UNC computation moved to run per call).
pub fn wsl_distro_name() -> Option<String> {
    std::env::var("WSL_DISTRO_NAME").ok()
}

/// `embarch-umbrella/init.rs`'s `wsl_unc_path`, lifted verbatim: the
/// `\\wsl.localhost\<distro>\...` UNC form a Windows-hosted Core needs to
/// open a WSL2-local artifact path. Liftable-copy pattern
/// (`embarch-umbrella/design.md` §3 decision 15), applied a third time.
pub fn wsl_unc_path(distro: &str, absolute: &Path) -> String {
    let tail = absolute
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "\\");
    format!("\\\\wsl.localhost\\{distro}\\{tail}")
}
