use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use std::sync::Arc;

use crate::build::{BuildLocks, BuildOutcome};
use crate::config::{Config, ProjectConfig};
use crate::core_client::CoreClient;
use crate::resolve::{self, Selection};

#[derive(Clone)]
pub struct EmbarchApi {
    // Required by the #[tool_router]/#[tool_handler] macro pair even though
    // #[tool_handler] (used without an explicit `router = ...` argument)
    // rebuilds the router via `Self::tool_router()` rather than reading this
    // field directly — matches the upstream rmcp examples' own convention.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    config: Arc<Config>,
    core: CoreClient,
    build_locks: Arc<BuildLocks>,
}

impl EmbarchApi {
    pub fn new(config: Arc<Config>, core: CoreClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            config,
            core,
            build_locks: Arc::new(BuildLocks::new()),
        }
    }

    fn project(&self, name: &str) -> Result<&ProjectConfig, McpError> {
        self.config
            .project(name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    fn ok_json(value: serde_json::Value) -> Result<CallToolResult, McpError> {
        let text = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|e| format!("<failed to serialize result: {e}>"));
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    fn err_text(message: impl Into<String>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(message.into())]))
    }

    fn build_outcome_json(outcome: &BuildOutcome) -> serde_json::Value {
        serde_json::json!({
            "timed_out": outcome.timed_out,
            "exit_code": outcome.exit_code,
            "artifact_path": outcome.artifact_path.display().to_string(),
            "artifact_fresh": outcome.artifact_fresh,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
        })
    }
}

/// Parameters shared by every tool that operates on a single configured
/// project.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProjectParams {
    /// Name of a project from embarch-api's config file (see `list_projects`).
    pub project: String,
}

/// The four `discovery = "zephyr-west"` selection params (`design.md` §3
/// decision 12), shared by every tool that resolves a build target. Ignored
/// entirely for a `discovery = "static"` project.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct TargetParams {
    /// Name of a project from embarch-api's config file.
    pub project: String,
    /// Zephyr board name. Only meaningful for a discovery = "zephyr-west"
    /// project — see `list_targets`.
    pub board: Option<String>,
    /// Board variant (e.g. a product LED configuration). Only meaningful for
    /// a discovery = "zephyr-west" project.
    pub variant: Option<String>,
    /// Hardware revision. Only meaningful for a discovery = "zephyr-west"
    /// project.
    pub revision: Option<String>,
    /// App directory name under app/. Only meaningful for a discovery =
    /// "zephyr-west" project.
    pub app: Option<String>,
}

impl TargetParams {
    fn selection(&self) -> Selection<'_> {
        Selection {
            board: self.board.as_deref(),
            variant: self.variant.as_deref(),
            revision: self.revision.as_deref(),
            app: self.app.as_deref(),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FlashParams {
    /// Name of a project from embarch-api's config file.
    pub project: String,
    /// Zephyr board name. Only meaningful for a discovery = "zephyr-west"
    /// project.
    pub board: Option<String>,
    /// Board variant. Only meaningful for a discovery = "zephyr-west" project.
    pub variant: Option<String>,
    /// Hardware revision. Only meaningful for a discovery = "zephyr-west" project.
    pub revision: Option<String>,
    /// App directory name under app/. Only meaningful for a discovery =
    /// "zephyr-west" project.
    pub app: Option<String>,
    /// Path to a firmware file to flash instead of the project's configured
    /// artifact_path — use this to flash an already-built file without
    /// rebuilding. Bypasses target resolution entirely.
    pub firmware_path: Option<String>,
}

impl FlashParams {
    fn selection(&self) -> Selection<'_> {
        Selection {
            board: self.board.as_deref(),
            variant: self.variant.as_deref(),
            revision: self.revision.as_deref(),
            app: self.app.as_deref(),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SerialLogParams {
    /// Name of a project from embarch-api's config file.
    pub project: String,
    /// Serial port to read from. Defaults to the project's configured
    /// serial_port if omitted.
    pub port: Option<String>,
    /// Baud rate. Defaults to the project's configured serial_baud, or 115200.
    pub baud: Option<u32>,
    /// How long to read for, in milliseconds. Defaults to 2000.
    pub duration_ms: Option<u64>,
}

#[tool_router]
impl EmbarchApi {
    #[tool(description = "List every project configured in embarch-api's config file, with its chip, flash format, and source path. chip is omitted for a discovery = \"zephyr-west\" project, since it's resolved per call via list_targets/build/flash instead of stored.")]
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let projects: Vec<_> = self
            .config
            .projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "discovery": if p.is_zephyr_west() { "zephyr-west" } else { "static" },
                    "chip": p.chip,
                    "flash_format": p.flash_format,
                    "source_path": p.source_path.display().to_string(),
                    "has_serial_defaults": p.serial_port.is_some(),
                })
            })
            .collect();
        Self::ok_json(serde_json::json!({ "projects": projects }))
    }

    #[tool(description = "List a project's buildable targets. For a discovery = \"zephyr-west\" project: live-scans boards/ and app/ and returns every file-backing-validated (board, soc, cpucluster, variant, revision, app) tuple. For a discovery = \"static\" project with [[projects.targets]] rows: returns those verbatim. Otherwise errors with the TOML shape needed to populate [[projects.targets]] by hand.")]
    async fn list_targets(
        &self,
        Parameters(ProjectParams { project }): Parameters<ProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        match resolve::list_targets(project) {
            Ok(value) => Self::ok_json(value),
            Err(e) => Self::err_text(format!("{e:#}")),
        }
    }

    #[tool(description = "Get embarch-core's status: whether it's reachable and what debug probes it currently sees.")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        match self.core.status().await {
            Ok(status) => Self::ok_json(serde_json::json!({
                "status": status.status,
                "probes": status.probes.iter().map(|p| serde_json::json!({
                    "identifier": p.identifier,
                    "vendor_id": p.vendor_id,
                    "product_id": p.product_id,
                    "serial_number": p.serial_number,
                })).collect::<Vec<_>>(),
            })),
            Err(e) => Self::err_text(format!(
                "embarch-core unreachable or returned an error: {e:#}"
            )),
        }
    }

    #[tool(description = "Build a configured project by running its build command (configured, or, for a discovery = \"zephyr-west\" project, assembled at call time from board/variant/revision/app). Does not touch hardware. Use build_and_flash to build and then flash in one call.")]
    async fn build(
        &self,
        Parameters(params): Parameters<TargetParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&params.project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        let resolved = match resolve::resolve(project, params.selection(), &self.core).await {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        match self.build_locks.run_build(&resolved.plan).await {
            Ok(outcome) => {
                let mut value = Self::build_outcome_json(&outcome);
                value["success"] = serde_json::Value::Bool(outcome.build_succeeded());
                value["target"] = resolved.descriptor.clone();
                if outcome.build_succeeded() && !outcome.artifact_fresh {
                    value["warning"] = serde_json::Value::String(
                        "build exited 0 but no fresh artifact was found at artifact_path".into(),
                    );
                }
                if outcome.build_succeeded() {
                    Self::ok_json(value)
                } else {
                    Self::err_text(serde_json::to_string_pretty(&value).unwrap_or_default())
                }
            }
            Err(e) => Self::err_text(format!("failed to run build for '{}': {e:#}", project.name)),
        }
    }

    #[tool(description = "Flash a firmware artifact via embarch-core. Defaults to the resolved artifact_path (configured, or computed at call time for a discovery = \"zephyr-west\" target), or pass firmware_path to flash a specific file without rebuilding — this bypasses target resolution entirely.")]
    async fn flash(
        &self,
        Parameters(params): Parameters<FlashParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&params.project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        if let Some(firmware_path) = &params.firmware_path {
            // Bypasses target resolution entirely — still needs a chip,
            // which for a zephyr-west project only resolution can provide,
            // so an override still requires enough of a selection to
            // resolve one target's chip.
            let resolved = match resolve::resolve(project, params.selection(), &self.core).await {
                Ok(r) => r,
                Err(e) => return Self::err_text(format!("{e:#}")),
            };
            return match self.core.flash(&resolved.chip, firmware_path, &project.flash_format).await {
                Ok(resp) => Self::ok_json(serde_json::json!({
                    "flashed": resp.flashed,
                    "chip": resp.chip,
                    "firmware_path": firmware_path,
                    "target": resolved.descriptor,
                })),
                Err(e) => Self::err_text(format!("flash failed for '{}': {e:#}", project.name)),
            };
        }

        let resolved = match resolve::resolve(project, params.selection(), &self.core).await {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };
        let path = resolved
            .artifact_path_for_core
            .clone()
            .unwrap_or_else(|| resolved.plan.artifact_path.display().to_string());

        match self.core.flash(&resolved.chip, &path, &project.flash_format).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": path,
                "target": resolved.descriptor,
            })),
            Err(e) => Self::err_text(format!("flash failed for '{}': {e:#}", project.name)),
        }
    }

    #[tool(description = "Build a project and, only if the build succeeds and produces a fresh artifact, flash it via embarch-core. Refuses to flash a stale or failed build.")]
    async fn build_and_flash(
        &self,
        Parameters(params): Parameters<TargetParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&params.project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        let resolved = match resolve::resolve(project, params.selection(), &self.core).await {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        let outcome = match self.build_locks.run_build(&resolved.plan).await {
            Ok(outcome) => outcome,
            Err(e) => {
                return Self::err_text(format!(
                    "failed to run build for '{}': {e:#}",
                    project.name
                ))
            }
        };

        let mut build_json = Self::build_outcome_json(&outcome);
        build_json["target"] = resolved.descriptor.clone();

        if !outcome.ready_to_flash() {
            let mut value = build_json;
            value["success"] = serde_json::Value::Bool(false);
            value["reason"] = serde_json::Value::String(if outcome.timed_out {
                "build timed out".into()
            } else if outcome.exit_code != Some(0) {
                "build failed".into()
            } else {
                "build succeeded but no fresh artifact was found — refusing to flash".into()
            });
            return Self::err_text(serde_json::to_string_pretty(&value).unwrap_or_default());
        }

        let artifact_path = outcome.artifact_path.display().to_string();
        let core_firmware_path = resolved
            .artifact_path_for_core
            .clone()
            .unwrap_or_else(|| artifact_path.clone());
        match self
            .core
            .flash(&resolved.chip, &core_firmware_path, &project.flash_format)
            .await
        {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "success": true,
                "build": build_json,
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": core_firmware_path,
            })),
            Err(e) => Self::err_text(format!(
                "build succeeded but flash failed for '{}': {e:#}",
                project.name
            )),
        }
    }

    #[tool(description = "Reset a project's target chip via embarch-core. For a discovery = \"zephyr-west\" project, board/variant/revision/app select which target's chip to reset (extends design.md §3 decision 12's params to reset, for the same reason build/flash need them: there's no single stored chip to fall back to).")]
    async fn reset(
        &self,
        Parameters(params): Parameters<TargetParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&params.project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        let resolved = match resolve::resolve(project, params.selection(), &self.core).await {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        match self.core.reset(&resolved.chip).await {
            Ok(resp) => Self::ok_json(serde_json::json!({ "reset": resp.reset, "target": resolved.descriptor })),
            Err(e) => Self::err_text(format!("reset failed for '{}': {e:#}", project.name)),
        }
    }

    #[tool(description = "Read the serial console log for a project via embarch-core. Falls back to the project's configured serial_port/serial_baud if not overridden.")]
    async fn serial_log(
        &self,
        Parameters(SerialLogParams {
            project,
            port,
            baud,
            duration_ms,
        }): Parameters<SerialLogParams>,
    ) -> Result<CallToolResult, McpError> {
        let project = match self.project(&project) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        let port = match port.or_else(|| project.serial_port.clone()) {
            Some(port) => port,
            None => {
                return Self::err_text(format!(
                    "no serial port given and project '{}' has no configured serial_port",
                    project.name
                ))
            }
        };
        let baud = baud.or(project.serial_baud).unwrap_or(115_200);
        let duration_ms = duration_ms.unwrap_or(2_000);

        match self.core.serial_log(&port, baud, duration_ms).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "port": resp.port,
                "lines": resp.lines,
            })),
            Err(e) => Self::err_text(format!("serial-log failed for '{}': {e:#}", project.name)),
        }
    }
}

#[tool_handler]
impl ServerHandler for EmbarchApi {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "embarch-api: build and flash embedded firmware onto hardware owned by embarch-core. \
             Call list_projects first to see available projects. Use build_and_flash for the common \
             'get this working on hardware' case; use build/flash separately when iterating on \
             compiler errors or re-flashing without rebuilding.",
        )
    }
}
