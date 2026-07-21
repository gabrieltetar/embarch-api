use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::CoreConfig;

#[derive(Clone)]
pub struct CoreClient {
    base_url: String,
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

        Ok(CoreClient {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            token,
            client,
            status_timeout: Duration::from_secs(config.status_timeout_secs),
            reset_timeout: Duration::from_secs(config.reset_timeout_secs),
            flash_timeout: Duration::from_secs(config.flash_timeout_secs),
            serial_timeout: Duration::from_secs(config.serial_timeout_secs),
        })
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
        let url = format!("{}/status", self.base_url);
        self.send(self.client.get(url), self.status_timeout).await
    }

    pub async fn flash(&self, chip: &str, firmware_path: &str, format: &str) -> Result<FlashResponse> {
        let url = format!("{}/flash", self.base_url);
        let body = FlashRequest {
            chip,
            firmware_path,
            format,
        };
        self.send(self.client.post(url).json(&body), self.flash_timeout)
            .await
    }

    pub async fn reset(&self, chip: &str) -> Result<ResetResponse> {
        let url = format!("{}/reset", self.base_url);
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
        let url = format!("{}/serial-log", self.base_url);
        let request = self
            .client
            .get(url)
            .query(&[("port", port), ("baud", &baud.to_string()), ("duration_ms", &duration_ms.to_string())]);
        self.send(request, self.serial_timeout).await
    }
}
