use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::config::CoreConfig;
use crate::topology::{self, ProbeOutcome};

/// Where Core's address comes from.
enum Address {
    /// Declared in config, used as-is.
    Declared(String),
    /// `base_url = "auto"` — discovered by probing (config.rs, design.md
    /// §3.11). Deliberately not resolved at construction time: the startup
    /// connectivity check is MCP-mode-only and `list_projects` is meant to
    /// work with Core down, both of which eager resolution would break.
    Auto {
        host: Option<String>,
        port: u16,
    },
}

#[derive(Clone)]
pub struct CoreClient {
    address: Arc<Address>,
    /// Resolution happens at most once per process. Not persisted anywhere:
    /// the next invocation re-resolves, which is exactly what makes a changed
    /// WSL2 gateway IP a non-event.
    resolved: Arc<OnceCell<String>>,
    token: String,
    client: reqwest::Client,
    status_timeout: Duration,
    reset_timeout: Duration,
    flash_timeout: Duration,
    serial_timeout: Duration,
}

#[derive(Debug, Deserialize)]
pub struct ProbeInfo {
    pub identifier: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub probes: Vec<ProbeInfo>,
}

#[derive(Debug, Serialize)]
struct FlashRequest<'a> {
    chip: &'a str,
    firmware_path: &'a str,
    format: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct FlashResponse {
    pub flashed: bool,
    pub chip: String,
}

#[derive(Debug, Serialize)]
struct ResetRequest<'a> {
    chip: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct ResetResponse {
    pub reset: bool,
}

#[derive(Debug, Deserialize)]
pub struct SerialLogResponse {
    pub port: String,
    pub lines: Vec<String>,
}

impl CoreClient {
    pub fn new(config: &CoreConfig) -> Result<CoreClient> {
        let token = config.resolve_token()?;
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build reqwest client")?;

        let address = if config.is_auto() {
            Address::Auto {
                host: config.host.clone(),
                port: config.port,
            }
        } else {
            Address::Declared(config.base_url.trim_end_matches('/').to_string())
        };

        Ok(CoreClient {
            address: Arc::new(address),
            resolved: Arc::new(OnceCell::new()),
            token,
            client,
            status_timeout: Duration::from_secs(config.status_timeout_secs),
            reset_timeout: Duration::from_secs(config.reset_timeout_secs),
            flash_timeout: Duration::from_secs(config.flash_timeout_secs),
            serial_timeout: Duration::from_secs(config.serial_timeout_secs),
        })
    }

    /// Core's base URL, discovering it on first use if `base_url = "auto"`.
    ///
    /// The failure message names every candidate tried and what each one
    /// said, because "couldn't find Core" is useless on its own — the useful
    /// information is whether nothing was listening, or something answered
    /// and wasn't Core.
    async fn base_url(&self) -> Result<&str> {
        let url = self
            .resolved
            .get_or_try_init(|| async {
                let (host, port) = match self.address.as_ref() {
                    Address::Declared(url) => return Ok::<String, anyhow::Error>(url.clone()),
                    Address::Auto { host, port } => (host.as_deref(), *port),
                };

                let under_wsl2 = crate::env::under_wsl2();
                let gateway = if under_wsl2 {
                    crate::env::default_gateway()
                } else {
                    None
                };
                let candidates =
                    topology::candidates(under_wsl2, gateway.as_deref(), host, port);

                let client = &self.client;
                let attempts = topology::resolve(&candidates, move |url| async move {
                    crate::probe::probe_core(client, &url).await
                })
                .await;

                match topology::winner(&attempts) {
                    Some(found) => {
                        tracing::info!(
                            "embarch-core found at {} ({})",
                            found.candidate.base_url,
                            found.candidate.class.as_str()
                        );
                        Ok(found.candidate.base_url.clone())
                    }
                    None => {
                        let tried = attempts
                            .iter()
                            .map(|a| {
                                let why = match a.outcome {
                                    ProbeOutcome::Unreachable => "nothing listening".to_string(),
                                    ProbeOutcome::NotCore { status } => format!(
                                        "answered HTTP {status}, but isn't embarch-core"
                                    ),
                                    ProbeOutcome::Core { .. } => unreachable!("a hit would win"),
                                };
                                format!("\n  {} ({}) — {why}", a.candidate.base_url, a.candidate.class.as_str())
                            })
                            .collect::<String>();
                        Err(anyhow!(
                            "embarch-core not found (base_url = \"auto\"). Tried:{tried}\n\
                             Start embarch-core, or set [core].base_url to an explicit URL \
                             (or [core].host, for a Core on another machine)."
                        ))
                    }
                }
            })
            .await?;
        Ok(url.as_str())
    }

    /// Core's error responses are plain-text bodies (axum's IntoResponse for
    /// `(StatusCode, String)`), not JSON — so non-2xx bodies must be read as
    /// text, never parsed as JSON, or Core's actual error message is lost.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T> {
        let response = request
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await
            .context("request to embarch-core failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no response body>".to_string());
            return Err(anyhow!("embarch-core returned {status}: {body}"));
        }

        response
            .json::<T>()
            .await
            .context("failed to parse embarch-core's response as JSON")
    }

    pub async fn status(&self) -> Result<StatusResponse> {
        let url = format!("{}/status", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    pub async fn flash(&self, chip: &str, firmware_path: &str, format: &str) -> Result<FlashResponse> {
        let url = format!("{}/flash", self.base_url().await?);
        let body = FlashRequest {
            chip,
            firmware_path,
            format,
        };
        self.send(self.client.post(url).json(&body), self.flash_timeout)
            .await
    }

    pub async fn reset(&self, chip: &str) -> Result<ResetResponse> {
        let url = format!("{}/reset", self.base_url().await?);
        let body = ResetRequest { chip };
        self.send(self.client.post(url).json(&body), self.reset_timeout)
            .await
    }

    pub async fn serial_log(
        &self,
        port: &str,
        baud: u32,
        duration_ms: u64,
    ) -> Result<SerialLogResponse> {
        let url = format!("{}/serial-log", self.base_url().await?);
        let request = self
            .client
            .get(url)
            .query(&[("port", port), ("baud", &baud.to_string()), ("duration_ms", &duration_ms.to_string())]);
        self.send(request, self.serial_timeout).await
    }
}
