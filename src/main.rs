mod build;
mod cli;
mod config;
mod core_client;
mod env;
mod probe;
mod resolve;
mod token_discovery;
mod tools;
mod topology;
mod zephyr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use std::path::PathBuf;
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
        .context("no config path given: pass --config <path> or set EMBARCH_API_CONFIG")?;

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
