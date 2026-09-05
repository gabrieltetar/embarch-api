mod capacity;
mod cli;
mod config;
mod dev_bench;
mod logging;
mod reflash;
mod resolve;
mod study;
mod tools;
mod zephyr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
// `build` is the one module that lives behind this package's `lib` target
// rather than in the binary, so `tests/` can reach it at all (see `lib.rs`).
// Re-imported at the crate root so the sibling modules' existing
// `crate::build::…` paths keep resolving.
pub(crate) use embarch_api::build;
use embarch_core_client::CoreClient;
use rmcp::ServiceExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::Config;
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
/// chip. A `discovery = "static"` project **refuses** any of them, naming
/// which were given (`design.md` §3 decision 51) — it builds its configured
/// `build_command` verbatim and has nowhere to apply them.
#[derive(clap::Args, Debug)]
pub struct TargetSelection {
    /// Zephyr board name. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it rather than ignoring it.
    #[arg(long)]
    pub board: Option<String>,
    /// Board variant (e.g. a product LED configuration). Only for a
    /// discovery = "zephyr-west" project — a static project refuses it.
    #[arg(long)]
    pub variant: Option<String>,
    /// Hardware revision. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it rather than ignoring it.
    #[arg(long)]
    pub revision: Option<String>,
    /// App directory name under app/. Only for a discovery = "zephyr-west"
    /// project — a static project refuses it.
    #[arg(long)]
    pub app: Option<String>,
    /// A `-S` snippet to build with; may be given more than once. Only for a
    /// discovery = "zephyr-west" project — a static project refuses it.
    /// Omitted entirely falls back to the project's configured
    /// default_snippets, not "no snippets" — see `list-targets` for what's
    /// available. `--snippet none` (the reserved literal, alone) forces a
    /// build with no snippets despite that default; mixed with real names it
    /// is refused rather than guessed at.
    #[arg(long = "snippet")]
    pub snippet: Vec<String>,
    /// An extra `west build` flag (e.g. `-p`, `always`, passed as two
    /// separate --extra-arg occurrences); may be given more than once. Only
    /// for a discovery = "zephyr-west" project — a static project refuses it.
    /// Opaque passthrough — unlike snippets, not validated against anything.
    /// Omitted entirely falls back to the project's configured
    /// default_extra_args, not "no extra args".
    #[arg(long = "extra-arg")]
    pub extra_arg: Vec<String>,
}

/// CLI subcommand surface (design.md §3.10/§5a) — mirrors embarch-api's MCP
/// tools in `tools.rs` one-for-one, so a human with no MCP client can invoke
/// the identical operations directly.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List every project configured in embarch-api's config file.
    ListProjects,
    /// List live-discovered build targets for a discovery = "zephyr-west"
    /// project, or the single configured target — the project itself — for a
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
        /// Fully erase the chip before writing, rather than erasing only the
        /// sectors the new image covers. The equivalent of `west flash
        /// --erase`: without it, flash regions the new image doesn't cover
        /// (a Zephyr settings/NVS partition, and so any BLE bonds or
        /// provisioning state in it) survive from the previous firmware.
        #[arg(long)]
        erase: bool,
    },
    /// Build a project and, only if it succeeds with a fresh artifact, flash it.
    BuildAndFlash {
        project: String,
        #[command(flatten)]
        target: TargetSelection,
        /// Fully erase the chip before writing, rather than erasing only the
        /// sectors the new image covers. The equivalent of `west flash
        /// --erase`: without it, flash regions the new image doesn't cover
        /// (a Zephyr settings/NVS partition, and so any BLE bonds or
        /// provisioning state in it) survive from the previous firmware.
        #[arg(long)]
        erase: bool,
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
    /// link. All three seals (steps_crc over steps, streams_crc over
    /// streams, protocols_crc over protocols) are recomputed and overwritten
    /// regardless of what's in the file.
    ///
    /// The study's `requires` names the dev-bench and DUT builds it is meant
    /// to run against ("any" if it genuinely doesn't matter); --reflash says
    /// what to do about it. --project is needed only to reflash the DUT: a
    /// study isn't tied to a configured project, but rebuilding that DUT's
    /// firmware is.
    RunStudy {
        /// Path to a JSON file matching Study's schema.
        #[arg(long = "study-file")]
        study_file: PathBuf,
        /// Which firmware to rebuild and reflash before running, from the
        /// working tree AS IT CURRENTLY STANDS: none (the default),
        /// dev-bench, dut, or both. Defaults to none because flashing is the
        /// destructive half — a study that merely observes a board you just
        /// flashed by hand should not silently overwrite it.
        ///
        /// This never runs `git checkout`. If the tree isn't at the revision
        /// the study requires, the run fails naming both revisions and
        /// leaves the tree — and the board — alone.
        #[arg(long, default_value = "none")]
        reflash: String,
        /// Proceed even though a version requirement isn't satisfied. The
        /// override is recorded in the result's provenance.overrides, never
        /// silently honoured.
        #[arg(long = "allow-version-mismatch")]
        allow_version_mismatch: bool,
        /// Which configured project is the DUT. Required by
        /// `--reflash dut|both`, ignored otherwise: a study isn't tied to a
        /// project, but rebuilding the DUT's firmware is.
        #[arg(long)]
        project: Option<String>,
        #[command(flatten)]
        target: TargetSelection,
    },
    /// Get a submitted study's status via embarch-core.
    ///
    /// Without --follow this is a single snapshot, exactly as it has always
    /// been. With --follow it subscribes to embarch-core's live event stream
    /// (`GET /study/{id}/events`) and reports each step, sample batch and
    /// status change as it happens, until the study finishes.
    StudyStatus {
        study_id: String,
        /// Watch the study live instead of taking one snapshot.
        ///
        /// Subscribes to embarch-core's SSE event stream and prints one line
        /// per event. If the stream will not open, or drops mid-study, this
        /// falls back to polling the same endpoint the snapshot uses and says
        /// so on its own line — a lost stream is never a failed command.
        ///
        /// Under --json the output is **NDJSON**: one compact JSON object per
        /// line, ending with a `{"type": "summary", ...}` line. Every other
        /// subcommand's --json prints a single pretty object; a stream cannot,
        /// and line-delimited is what a reader of a stream can consume
        /// incrementally.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Stop following after this many seconds even if the study has not
        /// finished. Only meaningful with --follow; unset follows to the end.
        ///
        /// Hitting it exits 1 — "watch this to the end, but not longer than
        /// N" was asked and the answer is no. A study that *fails* still
        /// exits 0: reporting a failed study is a successful report.
        #[arg(long = "follow-timeout")]
        follow_timeout: Option<u64>,
    },
    /// Alias for study-stream-data, kept for one release: fetches whichever
    /// declared tap answers the "power" alias (a Samples-encoded tap on a
    /// PowerFrontEnd source). Prefer `study-stream-data <study_id> --name
    /// <tap>`, and see `list-study-streams` for what a study actually
    /// captured. Writes to stdout, or to --out if given. A study that
    /// declared no power tap has no power data, and that's reported as an
    /// error naming study_id, not silently empty output.
    StudyPowerData {
        study_id: String,
        /// Write the CSV to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Alias for study-stream-data, kept for one release: fetches whichever
    /// declared tap answers the "waveform" alias (a Samples-encoded tap on
    /// any source other than PowerFrontEnd). Same stdout/--out behavior as
    /// study-power-data.
    StudyWaveformData {
        study_id: String,
        /// Write the CSV to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Alias for study-stream-data, kept for one release: fetches whichever
    /// declared tap answers the "gatt" alias (a GattTranscript-encoded tap)
    /// — every notification, indication, read, write, subscribe and connect
    /// event across every step, uncapped, with each payload rendered as both
    /// hex and printable ASCII. This is the exhaustive record, and every
    /// study with a monitor step now declares this tap automatically —
    /// `StepResult.gatt_activity`, the capped per-step summary this used to
    /// be contrasted with, is retired at schema v14. Same stdout/--out
    /// behavior as study-power-data.
    StudyGattData {
        study_id: String,
        /// Write the CSV to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Fetch one declared stream tap's capture from a study, by the name the
    /// Study gave it. Replaces study-power-data/study-waveform-data/
    /// study-gatt-data, which are now aliases over the same mechanism. Serves
    /// the tap's rendered file when its declared StreamEncoding has one (CSV
    /// for Samples and GattTranscript), or its byte-for-byte capture when it
    /// doesn't (Raw, OutpostTrace) or when --raw is given. Same stdout/--out
    /// behavior as study-power-data — and --out is the way to get a binary
    /// capture out intact. Run list-study-streams first rather than guessing
    /// a name.
    StudyStreamData {
        study_id: String,
        /// The tap's declared name — StreamTap.name in the submitted Study.
        #[arg(long)]
        name: String,
        /// Serve the byte-for-byte capture instead of the tap's rendered
        /// file. No effect on a tap whose encoding has no rendering.
        #[arg(long)]
        raw: bool,
        /// Write the capture to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List what a completed study actually captured: one row per declared
    /// stream tap, with its name, bytes written, and whether it was
    /// TRUNCATED — which is how you learn a capture is short rather than
    /// complete (a retention rotation deleted a segment, or dev-bench
    /// reported dropping records). A row with 0 bytes is a tap that was
    /// declared and captured nothing, which is a different fact from a tap
    /// that was never declared.
    ListStudyStreams { study_id: String },
    /// Build embarch-dev-bench's own firmware by running `west build`.
    /// No project — dev-bench isn't a `[[projects]]` entry, see config.rs's
    /// `DevBenchConfig`. Which board gets built comes from [dev_bench]
    /// config (board/chip/flash_format/artifact_path), not from a constant.
    BuildDevBench,
    /// Flash embarch-dev-bench's own firmware via embarch-core.
    FlashDevBench {
        /// Flash this file instead of dev-bench's own configured build
        /// artifact.
        #[arg(long)]
        firmware_path: Option<String>,
        /// Fully erase the chip before writing, rather than erasing only the
        /// sectors the new image covers. The equivalent of `west flash
        /// --erase`: without it, flash regions the new image doesn't cover
        /// (a Zephyr settings/NVS partition, and so any BLE bonds or
        /// provisioning state in it) survive from the previous firmware.
        #[arg(long)]
        erase: bool,
    },
    /// Build embarch-dev-bench's own firmware and, only if the build
    /// succeeds with a fresh artifact, flash it.
    BuildAndFlashDevBench {
        /// Fully erase the chip before writing, rather than erasing only the
        /// sectors the new image covers. The equivalent of `west flash
        /// --erase`: without it, flash regions the new image doesn't cover
        /// (a Zephyr settings/NVS partition, and so any BLE bonds or
        /// provisioning state in it) survive from the previous firmware.
        #[arg(long)]
        erase: bool,
    },
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
        /// Picks which currently-attached probe to enroll when more than
        /// one is present (`embarch-topology/design.md` §3 decision 15).
        /// Omitted, falls back to "exactly one attached" — unchanged.
        #[arg(long)]
        probe_serial: Option<String>,
    },
    /// Explicit, non-destructive re-check of an already-enrolled board's
    /// live identity via embarch-core (design.md §3 decision 28) — the same
    /// check flash/reset/run-study already run mid-attach, callable on its
    /// own. A topology mismatch exits nonzero with the recorded/live
    /// hardware IDs and a fix_it_url printed to stderr — never opened
    /// automatically (`embarch-topology`'s own `validate` CLI does the same).
    Validate {
        /// The enrollment role to re-check (e.g. "dev-bench").
        #[arg(long)]
        role: String,
    },
    /// List the most recent topology-mismatch alerts from embarch-core's
    /// durable log (design.md §3 decision 28).
    Alerts {
        /// How many of the most recent alerts to return.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print the version numbers compiled into THIS binary: its crate
    /// version, the `embarch-study-designer` host type schema version it
    /// submits studies under, and its `--json` shape version.
    ///
    /// Loads no config and contacts no embarch-core — these are facts about
    /// the installed binary, not about a running system, and the caller that
    /// needs them most (`embarch doctor`'s schema-agreement check) is
    /// diagnosing a machine where the config or Core may be exactly what is
    /// broken. `--json` is the surface to read: `host_type_schema_version`.
    Versions,
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

fn main() -> Result<()> {
    // `Builder::thread_stack_size` only sizes threads the *runtime* spawns
    // (worker/blocking-pool threads) — the top-level future driven by
    // `block_on` still runs on whatever thread calls it, which for a plain
    // `fn main` is the process's own main thread, at the OS/platform
    // default (8 MiB on Linux, 1 MiB on Windows) with no `Builder` knob to
    // change it. That's not enough to deserialize a real `StudyResult` in
    // place (~1.3 MB by value, `embarch-study-designer`'s
    // worst-case-capacity `heapless` fields, embarch-core/design.md
    // decision 24, plus unoptimized debug-build frame overhead on top) —
    // confirmed via a real crash, not just the type's known size: this
    // exact shape (`Builder::thread_stack_size` alone, no thread
    // respawn) still `SIGABRT`-crashed on `main` itself serving
    // `study_status` for a real 3-step Milestone 3 study
    // (`BleConnect`->`GattDiscover`->`GattMonitorAll`) against real
    // hardware, both over MCP and via this same subcommand run directly.
    // Matches the exact "debug builds only" risk
    // `embarch-study-designer/design.md` §7 already tracked from a smaller
    // 2-step case — this is that same bug, not a new one, just the first
    // real GATT-sized trigger, and the first time it's needed a real
    // production fix rather than a test-only `RUST_MIN_STACK`/
    // `std::thread::Builder` workaround (already used for exactly this in
    // `study.rs`'s own tests — same fix, now applied to the real binary).
    // The actual fix: spawn the runtime itself, `block_on` included, on a
    // dedicated thread with an explicit stack size, since that's the only
    // lever that covers the calling thread too. 64 MiB matches the
    // `RUST_MIN_STACK` value this repo's tests already use for the same
    // underlying cause.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(512 * 1024 * 1024)
                .build()
                .context("failed to build the tokio runtime")?
                .block_on(async_main())
        })
        .context("failed to spawn main worker thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("main worker thread panicked"))?
}

async fn async_main() -> Result<()> {
    // Parsed before logging is installed, not after: `logging::init` needs
    // to know which mode this process is, and the presence of a subcommand
    // is the only thing that says so (§3 decision 43 — one file, both
    // modes, every line tagged with which). Nothing logs before this point,
    // and clap reports its own errors without tracing.
    let cli = Cli::parse();
    logging::init(if cli.command.is_some() {
        logging::Mode::Cli
    } else {
        logging::Mode::Mcp
    });
    // `versions` answers from compiled constants alone, so it is dispatched
    // **before** config resolution rather than through `cli::run` (decision
    // 52). Every other subcommand needs a `Config` and a `CoreClient`, and a
    // missing or unreadable config exits 1 below — which would make the one
    // subcommand whose whole job is "what is this binary" unreadable on a
    // machine whose config is the thing being diagnosed. `cli::run` keeps a
    // matching arm so the surface is exhaustive from either entry point.
    if matches!(cli.command, Some(Commands::Versions)) {
        std::process::exit(cli::versions(cli.json));
    }
    // Grouped into one fallible step so a CLI run can report a startup
    // failure *through the surface it was asked for*. Returning these
    // straight out of `async_main` printed Rust's default `Err` rendering
    // to stderr and left `--json` with no object at all — see
    // `cli::startup_failure`. MCP mode still returns the error: there is no
    // JSON surface to put it on, and a protocol-level failure to start is
    // what `interfaces/tools.md` already says an unloadable config is.
    let startup = (|| -> Result<(PathBuf, Config, CoreClient)> {
        let config_path = cli
            .config
            .clone()
            .or_else(|| std::env::var_os("EMBARCH_API_CONFIG").map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok().and_then(|cwd| find_config_upward(&cwd)))
            .context(
                "no config path given: pass --config <path>, set EMBARCH_API_CONFIG, \
                 or run from within (or under) a firmware repo containing embarch/embarch.toml",
            )?;

        let config = Config::load_from_path(&config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?;
        let core = CoreClient::new(&config.core).context("failed to build embarch-core client")?;
        Ok((config_path, config, core))
    })();

    let (config_path, config, core) = match startup {
        Ok(started) => started,
        Err(e) => {
            if cli.command.is_some() {
                let message = format!("{e:#}");
                tracing::error!("cli invocation failed before it started: {message}");
                std::process::exit(cli::startup_failure(cli.json, message));
            }
            return Err(e);
        }
    };

    if let Some(command) = cli.command {
        // A one-shot CLI run would otherwise leave nothing in the logfile at
        // all — most subcommands emit no `tracing` events of their own, and
        // the process is gone before anyone looks (§3 decision 43's whole
        // motivating case). These two lines are the record: what was asked
        // for, and how it came out. `{command:?}` is clap's derived `Debug`,
        // which carries the subcommand's own arguments with it.
        tracing::info!("cli invocation: {command:?} (config {})", config_path.display());
        let exit_code = cli::run(command, cli.json, Arc::new(config), core).await;
        tracing::info!("cli invocation finished with exit code {exit_code}");
        std::process::exit(exit_code);
    }

    tracing::info!(
        "embarch-api starting (config {}): {} project(s) configured, core base_url={}{}",
        config_path.display(),
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
