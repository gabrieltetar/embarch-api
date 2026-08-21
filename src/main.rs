mod build;
mod cli;
mod config;
mod core_client;
mod dev_bench;
mod resolve;
mod study;
mod token_discovery;
mod tools;
mod zephyr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::Config;
use core_client::CoreClient;
use tools::EmbarchApi;

#[derive(Parser)]
#[command(name = "embarch-api", version)]
#[command(about = "MCP server + build orchestrator sitting between Claude Code and embarch-core")]
struct Cli {
    /// Path to the TOML config file. Falls back to the EMBARCH_API_CONFIG
    /// env var if not given.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON on stdout instead of human-readable text.
    /// Only meaningful alongside a subcommand; harmless (ignored) in MCP mode.
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

/// The four `discovery = "zephyr-west"` selection flags (`design.md` §3
/// decision 12), shared by every subcommand that runs a build or needs a
/// chip. Ignored entirely for a `discovery = "static"` project.
#[derive(clap::Args)]
pub struct TargetSelection {
    /// Zephyr board name. Only meaningful for a discovery = "zephyr-west"
    /// project; ignored otherwise.
    #[arg(long)]
    pub board: Option<String>,
    /// Board variant (e.g. a product LED configuration). Only meaningful for
    /// a discovery = "zephyr-west" project.
    #[arg(long)]
    pub variant: Option<String>,
    /// Hardware revision. Only meaningful for a discovery = "zephyr-west"
    /// project.
    #[arg(long)]
    pub revision: Option<String>,
    /// App directory name under app/. Only meaningful for a discovery =
    /// "zephyr-west" project.
    #[arg(long)]
    pub app: Option<String>,
    /// A `-S` snippet to build with; may be given more than once. Only
    /// meaningful for a discovery = "zephyr-west" project. Omitted entirely
    /// falls back to the project's configured default_snippets, not "no
    /// snippets" — see `list-targets` for what's available.
    #[arg(long = "snippet")]
    pub snippet: Vec<String>,
    /// An extra `west build` flag (e.g. `-p`, `always`, passed as two
    /// separate --extra-arg occurrences); may be given more than once. Only
    /// meaningful for a discovery = "zephyr-west" project. Opaque passthrough
    /// — unlike snippets, not validated against anything. Omitted entirely
    /// falls back to the project's configured default_extra_args, not "no
    /// extra args".
    #[arg(long = "extra-arg")]
    pub extra_arg: Vec<String>,
}

/// CLI subcommand surface (design.md §3.10/§5a) — mirrors embarch-api's MCP
/// tools in `tools.rs` one-for-one, so a human with no MCP client can invoke
/// the identical operations directly.
#[derive(Subcommand)]
pub enum Commands {
    /// List every project configured in embarch-api's config file.
    ListProjects,
    /// List live-discovered build targets for a discovery = "zephyr-west"
    /// project, or the hand-authored [[projects.targets]] menu for a
    /// discovery = "static" one.
    ListTargets { project: String },
    /// Get embarch-core's status: reachability and connected debug probes.
    Status,
    /// Build a configured project by running its configured build command.
    Build {
        project: String,
        #[command(flatten)]
        target: TargetSelection,
    },
    /// Flash a firmware artifact via embarch-core.
    Flash {
        project: String,
        #[command(flatten)]
        target: TargetSelection,
        /// Flash this file instead of the project's configured artifact_path.
        #[arg(long)]
        firmware_path: Option<String>,
    },
    /// Build a project and, only if it succeeds with a fresh artifact, flash it.
    BuildAndFlash {
        project: String,
        #[command(flatten)]
        target: TargetSelection,
    },
    /// Reset a project's target chip via embarch-core.
    Reset {
        project: String,
        #[command(flatten)]
        target: TargetSelection,
    },
    /// Read the serial console log for a project via embarch-core.
    SerialLog {
        project: String,
        /// Serial port to read from. Defaults to the project's configured serial_port.
        #[arg(long)]
        port: Option<String>,
        /// Baud rate. Defaults to the project's configured serial_baud, or 115200.
        #[arg(long)]
        baud: Option<u32>,
        /// How long to read for, in milliseconds. Defaults to 2000.
        #[arg(long = "duration-ms")]
        duration_ms: Option<u64>,
    },
    /// Submit a Study (embarch-study-designer's schema) for embarch-core to
    /// run against whatever DUT is connected through its dev-bench serial
    /// link. No project — a study isn't tied to a configured project.
    /// steps_crc is recomputed from steps and overwritten regardless of
    /// what's in the file.
    RunStudy {
        /// Path to a JSON file matching Study's schema.
        #[arg(long = "study-file")]
        study_file: PathBuf,
    },
    /// Get a submitted study's status via embarch-core.
    StudyStatus { study_id: String },
    /// Fetch a study's power-measurement CSV data via embarch-core. Writes
    /// to stdout, or to --out if given. A study with no power_sample steps
    /// has no power data — that's reported as an error naming study_id, not
    /// silently empty output.
    StudyPowerData {
        study_id: String,
        /// Write the CSV to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Fetch a study's waveform CSV data via embarch-core. Same
    /// stdout/--out behavior as study-power-data.
    StudyWaveformData {
        study_id: String,
        /// Write the CSV to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Build embarch-dev-bench's own firmware (the ESP32-C5 espressif
    /// workspace) by running `west build`. No project — dev-bench isn't a
    /// `[[projects]]` entry, see config.rs's `DevBenchConfig`. Requires
    /// [dev_bench] to be configured.
    BuildDevBench,
    /// Flash embarch-dev-bench's own firmware via embarch-core.
    FlashDevBench {
        /// Flash this file instead of dev-bench's own configured build
        /// artifact.
        #[arg(long)]
        firmware_path: Option<String>,
    },
    /// Build embarch-dev-bench's own firmware and, only if the build
    /// succeeds with a fresh artifact, flash it.
    BuildAndFlashDevBench,
    /// Reset embarch-dev-bench's own chip via embarch-core — needed after
    /// flash-dev-bench/build-and-flash-dev-bench, since flashing halts the
    /// core rather than starting it running.
    ResetDevBench,
    /// Enroll a physical probe with embarch-core's known_boards table
    /// (design.md decision 22), recording which board its serial number is
    /// wired to. Requires exactly one debug probe currently attached.
    EnrollProbe {
        /// A human-chosen label for this board (e.g.
        /// "reference-dut-fw" or "dev-bench").
        #[arg(long)]
        role: String,
        /// The probe-rs chip target this probe should attach as (e.g.
        /// "nRF54L15", "esp32c5").
        #[arg(long)]
        chip: String,
    },
}

/// Walks up from `start` looking for `embarch/embarch.toml` at each level —
/// the conventional location `embarch init` scaffolds
/// (`embarch-umbrella/design.md` §3 decision 10), same discovery pattern
/// `git`/`west` themselves use for their own config/workspace root.
///
/// Only ever consulted as a third fallback, after `--config` and
/// `EMBARCH_API_CONFIG` — never the sole mechanism. `embarch-api/design.md`
/// §4 already rejected cwd-inference *as the only source*, for a real
/// reason: an MCP client controls the spawn cwd, so silently trusting it
/// unconditionally would be a hidden assumption the config's origin
/// couldn't be audited against. A last-resort fallback behind two explicit
/// ones doesn't have that problem — an explicit `--config`/env var always
/// wins outright, this only fires when neither was given at all, and it
/// solves a real gap those two miss: an engineer working across several
/// firmware repos has no single `EMBARCH_API_CONFIG` value that's ever
/// right, and a fresh `claude mcp add` per repo isn't "no --config needed,"
/// it's "typed once instead of every time."
fn find_config_upward(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join("embarch").join("embarch.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the MCP JSON-RPC transport — logging must go to stderr, or
    // it corrupts the stream the moment anything logs.
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .or_else(|| std::env::var_os("EMBARCH_API_CONFIG").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok().and_then(|cwd| find_config_upward(&cwd)))
        .context(
            "no config path given: pass --config <path>, set EMBARCH_API_CONFIG, \
             or run from within (or under) a firmware repo containing embarch/embarch.toml",
        )?;

    let config = Config::load_from_path(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    let core = CoreClient::new(&config.core).context("failed to build embarch-core client")?;

    if let Some(command) = cli.command {
        let exit_code = cli::run(command, cli.json, Arc::new(config), core).await;
        std::process::exit(exit_code);
    }

    tracing::info!(
        "embarch-api starting: {} project(s) configured, core base_url={}{}",
        config.projects.len(),
        config.core.base_url,
        if config.core.is_auto() {
            " (resolved on first use)"
        } else {
            ""
        }
    );

    let server = EmbarchApi::new(Arc::new(config), core);
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start MCP stdio server")?;
    running.waiting().await.context("MCP server exited with an error")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Minimal tempdir helper — same pattern used throughout this crate's
    // other test modules (config.rs, zephyr.rs).
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "embarch-api-main-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }

    #[test]
    fn finds_embarch_toml_in_the_starting_directory_itself() {
        let dir = tempdir();
        fs::create_dir_all(dir.path().join("embarch")).unwrap();
        fs::write(dir.path().join("embarch/embarch.toml"), "").unwrap();

        assert_eq!(
            find_config_upward(dir.path()),
            Some(dir.path().join("embarch/embarch.toml"))
        );
    }

    #[test]
    fn finds_embarch_toml_several_levels_up() {
        let dir = tempdir();
        fs::create_dir_all(dir.path().join("embarch")).unwrap();
        fs::write(dir.path().join("embarch/embarch.toml"), "").unwrap();
        let deep = dir.path().join("app/widget/src");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_config_upward(&deep), Some(dir.path().join("embarch/embarch.toml")));
    }

    #[test]
    fn does_not_find_a_sibling_repos_embarch_toml() {
        // A firmware repo with no embarch/embarch.toml of its own must not
        // pick up a *different* repo's config just because it happens to
        // share a parent directory — walking stops correctly short of a
        // sibling, not just short of the filesystem root.
        let dir = tempdir();
        let other_repo = dir.path().join("other-repo");
        fs::create_dir_all(other_repo.join("embarch")).unwrap();
        fs::write(other_repo.join("embarch/embarch.toml"), "").unwrap();

        let this_repo = dir.path().join("this-repo/src");
        fs::create_dir_all(&this_repo).unwrap();

        // Walking up from this-repo/src reaches dir.path() (their common
        // parent) without ever finding an embarch/ under this-repo or
        // dir.path() itself — other-repo's is a sibling, not an ancestor.
        assert_eq!(find_config_upward(&this_repo), None);
    }

    #[test]
    fn returns_none_when_no_embarch_toml_exists_anywhere_up_to_root() {
        let dir = tempdir();
        let deep = dir.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_config_upward(&deep), None);
    }
}
