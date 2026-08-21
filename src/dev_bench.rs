//! Resolves the one `embarch-dev-bench` build target this suite knows about
//! into a `resolve::Resolved`, the same shape `resolve::resolve` produces
//! for a `[[projects]]` entry — so `build.rs`'s `BuildLocks`/`run_build` and
//! `core_client.rs`'s `flash` are reused as-is, with no new build/flash
//! machinery. Deliberately **not** a `[[projects]]` entry itself (see
//! `config::DevBenchConfig`'s own doc comment for why): dev-bench isn't a
//! DUT a firmware engineer or an agent configures/discovers, it's EmbArch's
//! own fixed test rig, so its board/chip/flash format/flash base address
//! are constants declared here, not per-project config fields.
//!
//! embarch-api/design.md's dev-bench-flashing-pipeline decision.

use anyhow::Result;

use crate::build::BuildPlan;
use crate::config::DevBenchConfig;
use crate::resolve::Resolved;

/// This suite's one dev-bench board (`embarch-dev-bench/design.md` decision
/// 26) — a fixed fact, not resolved per call the way a `discovery =
/// "zephyr-west"` DUT project's board is, since there's exactly one.
pub const BOARD: &str = "esp32c5_devkitc/esp32c5/hpcore";
pub const CHIP: &str = "esp32c5";
pub const FLASH_FORMAT: &str = "bin";
/// Zephyr's own merge address for this board (`embarch-core/design.md` §3
/// decision 18) — meaningful only because `FLASH_FORMAT` is `"bin"`.
pub const BASE_ADDRESS: &str = "0x2000";
const APP_DIR: &str = "app";
const ARTIFACT_REL_PATH: &str = "build/zephyr/zephyr.bin";

/// Resolves `config` into a `Resolved`, ready for `build::BuildLocks::run_build`
/// and `core_client::CoreClient::flash`. Infallible today (every field is
/// either a fixed constant or already-validated config) — returns `Result`
/// anyway so a future check (e.g. confirming `app/` exists under
/// `source_path`) can be added without changing this function's signature.
pub fn resolve(config: &DevBenchConfig) -> Result<Resolved> {
    let artifact_path = config.source_path.join(ARTIFACT_REL_PATH);
    let command = vec![
        config.west_binary.display().to_string(),
        "build".to_string(),
        "-b".to_string(),
        BOARD.to_string(),
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
        chip: CHIP.to_string(),
        flash_format: FLASH_FORMAT.to_string(),
        base_address: Some(BASE_ADDRESS.to_string()),
        probe_serial: config.probe_serial.clone(),
        descriptor: serde_json::json!({
            "dev_bench": true,
            "board": BOARD,
            "chip": CHIP,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config() -> DevBenchConfig {
        DevBenchConfig {
            source_path: PathBuf::from("/some/dev-bench/workspaces/espressif"),
            west_binary: PathBuf::from("/some/venv/bin/west"),
            build_timeout_secs: 900,
            env: Default::default(),
            probe_serial: None,
        }
    }

    #[test]
    fn resolves_the_fixed_board_chip_and_format() {
        let resolved = resolve(&config()).unwrap();
        assert_eq!(resolved.chip, CHIP);
        assert_eq!(resolved.flash_format, FLASH_FORMAT);
        assert_eq!(resolved.base_address.as_deref(), Some(BASE_ADDRESS));
    }

    #[test]
    fn build_command_and_artifact_path_use_the_configured_source_path() {
        let resolved = resolve(&config()).unwrap();
        assert_eq!(
            resolved.plan.cwd,
            PathBuf::from("/some/dev-bench/workspaces/espressif")
        );
        assert_eq!(
            resolved.plan.command,
            vec!["/some/venv/bin/west", "build", "-b", BOARD, "app"]
        );
        assert_eq!(
            resolved.plan.artifact_path,
            PathBuf::from("/some/dev-bench/workspaces/espressif/build/zephyr/zephyr.bin")
        );
    }

    #[test]
    fn lock_key_is_stable_regardless_of_config_contents() {
        // Only one dev-bench build can ever be in flight at a time — unlike
        // a DUT project's lock_key (which varies per target so unrelated
        // builds don't serialize against each other), dev-bench's is a
        // fixed constant since there's only ever one.
        assert_eq!(resolve(&config()).unwrap().plan.lock_key, "dev_bench");
    }
}
