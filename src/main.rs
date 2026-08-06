mod build;
mod cli;
mod config;
mod core_client;
mod env;
mod probe;
mod token_discovery;
mod tools;
mod topology;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use core_client::CoreClient;
use tools::EmbarchApi;

#[derive(Parser)]
#[command(name = "embarch-api")]
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

/// CLI subcommand surface (design.md §3.10/§5a) — mirrors embarch-api's six
/// MCP tools in `tools.rs` one-for-one, so a human with no MCP client can
/// invoke the identical operations directly.
#[derive(Subcommand)]
pub enum Commands {
    /// List every project configured in embarch-api's config file.
    ListProjects,
    /// Get embarch-core's status: reachability and connected debug probes.
    Status,
    /// Build a configured project by running its configured build command.
    Build { project: String },
    /// Flash a firmware artifact via embarch-core.
    Flash {
        project: String,
        /// Flash this file instead of the project's configured artifact_path.
        #[arg(long)]
        firmware_path: Option<String>,
    },
    /// Build a project and, only if it succeeds with a fresh artifact, flash it.
    BuildAndFlash { project: String },
    /// Reset a project's target chip via embarch-core.
    Reset { project: String },
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
