//! Resolves the `embarch-dev-bench` build target this machine's bench is
//! wired to into a `resolve::Resolved`, the same shape `resolve::resolve`
//! produces for a `[[projects]]` entry — so `build.rs`'s
//! `BuildLocks`/`run_build` and `embarch-core-client`'s `CoreClient::flash`
//! are reused as-is, with no new build/flash machinery. Deliberately **not**
//! a `[[projects]]` entry itself (see `config::DevBenchConfig`'s own doc
//! comment for why): dev-bench isn't a DUT a firmware engineer or an agent
//! configures/discovers, it's EmbArch's own test rig.
//!
//! **This file used to hold the board itself as constants** — `BOARD`,
//! `CHIP`, `FLASH_FORMAT`, `BASE_ADDRESS`, all ESP32-C5 — on the premise
//! that there was exactly one dev-bench board and so nothing to resolve.
//! That premise died on 2026-08-31 when an nRF54L15DK became the bench for
//! the second time (`embarch-decision-reversals.md` row 13 was the first
//! reversal in this pair; this is its counter-reversal), and the two boards
//! disagree about every one of those four values. They now come from
//! `[dev_bench]` config, undefaulted — `config::DevBenchConfig`'s doc
//! comment has the reasoning for why no default is better than a plausible
//! one here.
//!
//! embarch-api/design.md's dev-bench-flashing-pipeline decision.

use anyhow::Result;

use crate::build::BuildPlan;
use crate::config::DevBenchConfig;
use crate::resolve::Resolved;

/// The app directory within the workspace. Still a constant, and legitimately
/// so: it's `embarch-dev-bench`'s own repo layout (`workspaces/*/app` is a
/// symlink to the one shared `app/`, that repo's `design.md` §2), identical
/// for every vendor-family workspace and not a property of any board.
const APP_DIR: &str = "app";

/// Resolves `config` into a `Resolved`, ready for `build::BuildLocks::run_build`
/// and `embarch_core_client::CoreClient::flash`. Infallible today (every field is
/// either already-validated config or repo layout) — returns `Result`
/// anyway so a future check (e.g. confirming `app/` exists under
/// `source_path`) can be added without changing this function's signature.
pub fn resolve(config: &DevBenchConfig) -> Result<Resolved> {
    let artifact_path = config.source_path.join(&config.artifact_path);
    let command = vec![
        config.west_binary.display().to_string(),
        "build".to_string(),
        "-b".to_string(),
        config.board.clone(),
        APP_DIR.to_string(),
    ];

    Ok(Resolved {
        plan: BuildPlan {
            lock_key: "dev_bench".to_string(),
            cwd: config.source_path.clone(),
            command,
            artifact_path,
            timeout_secs: config.build_timeout_secs,
            env: config.env.clone(),
        },
        chip: config.chip.clone(),
        flash_format: config.flash_format.clone(),
        base_address: crate::resolve::format_base_address(config.base_address),
        probe_serial: config.probe_serial.clone(),
        descriptor: serde_json::json!({
            "dev_bench": true,
            "board": config.board,
            "chip": config.chip,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The nRF54L15DK bench: NCS, so sysbuild, so the image is a level
    /// deeper than the espressif workspace's, and a `hex` with no offset.
    fn nordic_config() -> DevBenchConfig {
        DevBenchConfig {
            source_path: PathBuf::from("/some/dev-bench/workspaces/nordic"),
            west_binary: PathBuf::from("/some/venv/bin/west"),
            board: "nrf54l15dk/nrf54l15/cpuapp".to_string(),
            chip: "nRF54L15".to_string(),
            flash_format: "hex".to_string(),
            artifact_path: PathBuf::from("build/app/zephyr/zephyr.hex"),
            base_address: None,
            build_timeout_secs: 900,
            env: Default::default(),
            probe_serial: None,
        }
    }

    /// The ESP32-C5 bench this file used to hardcode, now expressed as
    /// config — kept as a test case precisely because it is the shape the
    /// constants encoded, and it must still resolve identically.
    fn espressif_config() -> DevBenchConfig {
        DevBenchConfig {
            source_path: PathBuf::from("/some/dev-bench/workspaces/espressif"),
            west_binary: PathBuf::from("/some/venv/bin/west"),
            board: "esp32c5_devkitc/esp32c5/hpcore".to_string(),
            chip: "esp32c5".to_string(),
            flash_format: "bin".to_string(),
            artifact_path: PathBuf::from("build/zephyr/zephyr.bin"),
            base_address: Some(0x2000),
            build_timeout_secs: 900,
            env: Default::default(),
            probe_serial: None,
        }
    }

    #[test]
    fn resolves_the_declared_board_chip_and_format() {
        let resolved = resolve(&nordic_config()).unwrap();
        assert_eq!(resolved.chip, "nRF54L15");
        assert_eq!(resolved.flash_format, "hex");
        assert_eq!(resolved.base_address, None);
    }

    /// The regression that matters: the two supported benches must not
    /// resolve to each other's board, chip, format, offset or artifact path.
    /// Every one of these five differed, which is why none of them could
    /// stay a constant.
    #[test]
    fn the_two_benches_agree_about_nothing_that_used_to_be_a_constant() {
        let nordic = resolve(&nordic_config()).unwrap();
        let espressif = resolve(&espressif_config()).unwrap();

        assert_ne!(nordic.chip, espressif.chip);
        assert_ne!(nordic.flash_format, espressif.flash_format);
        assert_ne!(nordic.base_address, espressif.base_address);
        assert_ne!(nordic.descriptor["board"], espressif.descriptor["board"]);
        assert_ne!(
            nordic.plan.artifact_path.file_name(),
            espressif.plan.artifact_path.file_name()
        );
    }

    /// A `bin` bench's offset survives the trip as the hex string Core's
    /// `/flash` wants, rather than as a decimal integer.
    #[test]
    fn a_bin_benchs_base_address_is_formatted_the_way_core_expects() {
        let resolved = resolve(&espressif_config()).unwrap();
        assert_eq!(resolved.base_address.as_deref(), Some("0x2000"));
    }

    #[test]
    fn build_command_and_artifact_path_use_the_configured_source_path() {
        let resolved = resolve(&nordic_config()).unwrap();
        assert_eq!(
            resolved.plan.cwd,
            PathBuf::from("/some/dev-bench/workspaces/nordic")
        );
        assert_eq!(
            resolved.plan.command,
            vec![
                "/some/venv/bin/west",
                "build",
                "-b",
                "nrf54l15dk/nrf54l15/cpuapp",
                "app"
            ]
        );
        assert_eq!(
            resolved.plan.artifact_path,
            PathBuf::from("/some/dev-bench/workspaces/nordic/build/app/zephyr/zephyr.hex")
        );
    }

    #[test]
    fn lock_key_is_stable_regardless_of_config_contents() {
        // Only one dev-bench build can ever be in flight at a time — unlike
        // a DUT project's lock_key (which varies per target so unrelated
        // builds don't serialize against each other), dev-bench's is a
        // fixed constant since there's only ever one. Still true now that
        // *which* board it is varies: the bench is one at a time.
        assert_eq!(resolve(&nordic_config()).unwrap().plan.lock_key, "dev_bench");
        assert_eq!(
            resolve(&espressif_config()).unwrap().plan.lock_key,
            "dev_bench"
        );
    }
}
