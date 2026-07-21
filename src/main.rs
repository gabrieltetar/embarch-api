mod build;
mod config;
mod core_client;
mod tools;

use anyhow::{Context, Result};
use clap::Parser;
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

    tracing::info!(
        "embarch-api starting: {} project(s) configured, core base_url={}",
        config.projects.len(),
        config.core.base_url
    );

    let server = EmbarchApi::new(Arc::new(config), core);
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start MCP stdio server")?;
    running.waiting().await.context("MCP server exited with an error")?;

    Ok(())
}
