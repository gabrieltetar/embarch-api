use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use std::sync::Arc;

use crate::build::{BuildLocks, BuildOutcome};
use crate::config::{Config, ProjectConfig};
use crate::core_client::{CoreClient, StudyConflictError, TopologyMismatchError};
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

    fn dev_bench_config(&self) -> Result<&crate::config::DevBenchConfig, McpError> {
        self.config.dev_bench.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                "no [dev_bench] configured — add a [dev_bench] table (source_path, \
                 west_binary) to build/flash embarch-dev-bench's own firmware",
                None,
            )
        })
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
    /// `-S` snippets to build with. Only meaningful for a discovery =
    /// "zephyr-west" project — see `list_targets`'s `snippets_by_app`.
    /// Omitted or empty falls back to the project's configured
    /// default_snippets, not "no snippets".
    pub snippets: Option<Vec<String>>,
    /// Extra `west build` flags (e.g. `["-p", "always"]`). Only meaningful
    /// for a discovery = "zephyr-west" project. Opaque passthrough, unlike
    /// snippets — not validated against anything. Omitted or empty falls
    /// back to the project's configured default_extra_args, not "no extra
    /// args".
    pub extra_args: Option<Vec<String>>,
}

impl TargetParams {
    fn selection(&self) -> Selection<'_> {
        Selection {
            board: self.board.as_deref(),
            variant: self.variant.as_deref(),
            revision: self.revision.as_deref(),
            app: self.app.as_deref(),
            snippets: self.snippets.as_deref().unwrap_or(&[]),
            extra_args: self.extra_args.as_deref().unwrap_or(&[]),
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
    /// `-S` snippets to build with. Only meaningful for a discovery =
    /// "zephyr-west" project. Omitted or empty falls back to the project's
    /// configured default_snippets, not "no snippets".
    pub snippets: Option<Vec<String>>,
    /// Extra `west build` flags. Only meaningful for a discovery =
    /// "zephyr-west" project. Omitted or empty falls back to the project's
    /// configured default_extra_args, not "no extra args".
    pub extra_args: Option<Vec<String>>,
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
            snippets: self.snippets.as_deref().unwrap_or(&[]),
            extra_args: self.extra_args.as_deref().unwrap_or(&[]),
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunStudyParams {
    /// The full Study to submit — embarch-study-designer's schema (name,
    /// steps, validations, steps_crc). Untyped here (rather than a typed
    /// Study field) since embarch-study-designer is a #![no_std] crate that
    /// doesn't depend on schemars; the object is validated by deserializing
    /// it into Study server-side, immediately on receipt. steps_crc is
    /// recomputed from steps and overwritten regardless of what's given.
    ///
    /// Real MCP-path gap found running `embarch-doc/embarch-api/milestone-8.md`
    /// §3.8 against a live MCP client: `serde_json::Value`'s own `JsonSchema`
    /// impl generates the JSON Schema literal `true` ("matches anything"),
    /// with no `"type": "object"` for a client to key off — at least one real
    /// client (Claude Code) read that as "no declared shape" and serialized
    /// whatever was passed as a JSON *string* rather than an inline object,
    /// which then failed server-side deserialization into `Study` with a
    /// confusing "expected struct Study, got a string" error. The CLI path
    /// never hits this (no JSON Schema involved — `--study-file` reads and
    /// parses the file directly). `schema_with` below overrides only the
    /// generated *schema* advertised to a client; deserialization here is
    /// unchanged, still a plain `serde_json::Value`.
    #[schemars(schema_with = "study_value_schema")]
    pub study: serde_json::Value,
}

/// `schemars(schema_with)` override for [`RunStudyParams::study`] — see that
/// field's doc comment for why `serde_json::Value`'s own default schema
/// (the literal `true`) isn't enough for at least one real MCP client to
/// send a structured object rather than a stringified one.
fn study_value_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": true
    })
}

/// A defensive fallback for exactly the client behavior `study_value_schema`
/// documents: if `v` arrived as a JSON-encoded string rather than an inline
/// object, parse it and use the result; anything else (already an object,
/// or some other JSON type entirely — `Study`'s own deserialization is what
/// rejects that case) passes through untouched. Pure and split out from
/// [`EmbarchApi::run_study`] so it's unit-testable with no MCP/Core
/// plumbing involved, same posture as `study.rs`'s `recompute_steps_crc`.
fn unwrap_stringified_json(v: serde_json::Value) -> Result<serde_json::Value, serde_json::Error> {
    match v {
        serde_json::Value::String(s) => serde_json::from_str(&s),
        v => Ok(v),
    }
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct FlashDevBenchParams {
    /// Flash this file instead of dev-bench's own configured build artifact.
    pub firmware_path: Option<String>,
}

/// Shared by every tool that operates on an already-submitted study by id.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StudyIdParams {
    /// The study_id returned by run_study.
    pub study_id: String,
}

/// `embarch-core/design.md` §3 decision 22's `POST /probes/enroll`, wrapped
/// per decision 34. No `project`/`board`/`variant`/etc. — enrollment isn't
/// build-target selection, it's "record which physical probe I mean,"
/// matching `run_study`'s own "no project param when the concept genuinely
/// isn't project-shaped" precedent.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EnrollProbeParams {
    /// A human-chosen label for this board (e.g. "reference-dut-fw"
    /// or "dev-bench") — recorded verbatim, not validated against anything.
    pub role: String,
    /// The probe-rs chip target this probe should attach as (e.g.
    /// "nRF54L15", "esp32c5") — used both for the enrollment readback and,
    /// once enrolled, for every later `flash`/`reset`/study gate check
    /// against this same probe.
    pub chip: String,
}

/// `embarch-core/design.md` §3 decision 28's `POST /validate`, wrapped per
/// decision 34's own precedent (`EnrollProbeParams`, above): no
/// `project`/build-target params, since this isn't build-target selection
/// either — just "is the board enrolled as `role` still the one attached."
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ValidateParams {
    /// The enrollment role to re-check (e.g. "dev-bench" or a project's own
    /// DUT role) — matches `enroll_probe`'s own `role`.
    pub role: String,
}

/// `embarch-core/design.md` §3 decision 28's `GET /alerts`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct AlertsParams {
    /// How many of the most recent alerts to return. Defaults to 20.
    pub limit: Option<usize>,
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

    #[tool(description = "List a project's buildable targets. For a discovery = \"zephyr-west\" project: live-scans boards/ and app/ and returns every file-backing-validated (board, soc, cpucluster, variant, revision, app) tuple, plus snippets_by_app (every real -S snippet available per app), default_snippets, and default_extra_args. For a discovery = \"static\" project with [[projects.targets]] rows: returns those verbatim. Otherwise errors with the TOML shape needed to populate [[projects.targets]] by hand.")]
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

    #[tool(description = "Build a configured project by running its build command (configured, or, for a discovery = \"zephyr-west\" project, assembled at call time from board/variant/revision/app/snippets/extra_args). snippets/extra_args, if omitted, fall back to the project's configured default_snippets/default_extra_args, not \"none\". extra_args is opaque passthrough (e.g. [\"-p\", \"always\"] for a pristine rebuild) — unlike snippets, not validated against anything. Does not touch hardware. Use build_and_flash to build and then flash in one call.")]
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
            return match self
                .core
                .flash(
                    &resolved.chip,
                    firmware_path,
                    &project.flash_format,
                    resolved.base_address.as_deref(),
                    resolved.probe_serial.as_deref(),
                )
                .await
            {
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
        // Always the WSL2-local path — `core.flash` itself decides whether
        // that can go straight to Core as-is or needs uploading, based on
        // topology (`core_client.rs`'s own doc comment, `design.md` §9).
        let path = resolved.plan.artifact_path.display().to_string();

        match self
            .core
            .flash(&resolved.chip, &path, &project.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref())
            .await
        {
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

        // Always the WSL2-local path — see the sibling `flash` fn above.
        let core_firmware_path = outcome.artifact_path.display().to_string();
        match self
            .core
            .flash(
                &resolved.chip,
                &core_firmware_path,
                &project.flash_format,
                resolved.base_address.as_deref(),
                resolved.probe_serial.as_deref(),
            )
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

    #[tool(description = "Build embarch-dev-bench's own firmware (the ESP32-C5 espressif workspace) by running west build. No project param — dev-bench isn't a configured project, it's EmbArch's one fixed test rig (board/chip/flash format are constants, not config). Requires [dev_bench] to be configured (source_path, west_binary). Does not touch hardware — use build_and_flash_dev_bench to build and then flash in one call.")]
    async fn build_dev_bench(&self) -> Result<CallToolResult, McpError> {
        let dev_bench = match self.dev_bench_config() {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let resolved = match crate::dev_bench::resolve(dev_bench) {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        match self.build_locks.run_build(&resolved.plan).await {
            Ok(outcome) => {
                let mut value = Self::build_outcome_json(&outcome);
                value["success"] = serde_json::Value::Bool(outcome.build_succeeded());
                value["target"] = resolved.descriptor.clone();
                if outcome.build_succeeded() {
                    Self::ok_json(value)
                } else {
                    Self::err_text(serde_json::to_string_pretty(&value).unwrap_or_default())
                }
            }
            Err(e) => Self::err_text(format!("failed to run build for dev_bench: {e:#}")),
        }
    }

    #[tool(description = "Flash embarch-dev-bench's own firmware via embarch-core. Defaults to dev-bench's own configured build artifact, or pass firmware_path to flash a specific file without rebuilding.")]
    async fn flash_dev_bench(
        &self,
        Parameters(FlashDevBenchParams { firmware_path }): Parameters<FlashDevBenchParams>,
    ) -> Result<CallToolResult, McpError> {
        let dev_bench = match self.dev_bench_config() {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let resolved = match crate::dev_bench::resolve(dev_bench) {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        let path = firmware_path.unwrap_or_else(|| resolved.plan.artifact_path.display().to_string());

        match self
            .core
            .flash(&resolved.chip, &path, &resolved.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref())
            .await
        {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": path,
                "target": resolved.descriptor,
            })),
            Err(e) => Self::err_text(format!("flash failed for dev_bench: {e:#}")),
        }
    }

    #[tool(description = "Build embarch-dev-bench's own firmware and, only if the build succeeds and produces a fresh artifact, flash it via embarch-core. Refuses to flash a stale or failed build. No project param, see build_dev_bench.")]
    async fn build_and_flash_dev_bench(&self) -> Result<CallToolResult, McpError> {
        let dev_bench = match self.dev_bench_config() {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let resolved = match crate::dev_bench::resolve(dev_bench) {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        let outcome = match self.build_locks.run_build(&resolved.plan).await {
            Ok(outcome) => outcome,
            Err(e) => return Self::err_text(format!("failed to run build for dev_bench: {e:#}")),
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

        let core_firmware_path = outcome.artifact_path.display().to_string();
        match self
            .core
            .flash(
                &resolved.chip,
                &core_firmware_path,
                &resolved.flash_format,
                resolved.base_address.as_deref(),
                resolved.probe_serial.as_deref(),
            )
            .await
        {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "success": true,
                "build": build_json,
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": core_firmware_path,
            })),
            Err(e) => Self::err_text(format!("build succeeded but flash failed for dev_bench: {e:#}")),
        }
    }

    #[tool(description = "Reset embarch-dev-bench's own chip via embarch-core — needed after flash_dev_bench/build_and_flash_dev_bench, since flashing halts the core rather than starting it running. No project param, see build_dev_bench.")]
    async fn reset_dev_bench(&self) -> Result<CallToolResult, McpError> {
        let dev_bench = match self.dev_bench_config() {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let resolved = match crate::dev_bench::resolve(dev_bench) {
            Ok(r) => r,
            Err(e) => return Self::err_text(format!("{e:#}")),
        };

        match self.core.reset(&resolved.chip, resolved.probe_serial.as_deref()).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "reset": resp.reset,
                "target": resolved.descriptor,
            })),
            Err(e) => Self::err_text(format!("reset failed for dev_bench: {e:#}")),
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

        match self.core.reset(&resolved.chip, resolved.probe_serial.as_deref()).await {
            Ok(resp) => Self::ok_json(serde_json::json!({ "reset": resp.reset, "target": resolved.descriptor })),
            Err(e) => Self::err_text(format!("reset failed for '{}': {e:#}", project.name)),
        }
    }

    #[tool(description = "Enroll a physical probe with embarch-topology's enrollment storage (design.md decision 22), recording which board its serial number is wired to. Requires exactly one debug probe currently attached — Core refuses (naming every candidate) otherwise, since the whole point is a human physically isolating the one board they mean before confirming. Once enrolled, flash/reset/run_study all refuse to touch that probe unless a live hardware-ID readback still matches what was recorded here. No project param — this isn't build-target selection.")]
    async fn enroll_probe(
        &self,
        Parameters(EnrollProbeParams { role, chip }): Parameters<EnrollProbeParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.enroll_probe(&role, &chip).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "probe_serial": resp.probe_serial,
                "role": resp.role,
                "chip": resp.chip,
                "hardware_id": resp.hardware_id,
                "confirmed_at_utc_ms": resp.confirmed_at_utc_ms,
            })),
            Err(e) => Self::err_text(format!("enroll_probe failed: {e:#}")),
        }
    }

    #[tool(description = "Explicit, non-destructive re-check of an already-enrolled board's live identity via embarch-core's POST /validate (design.md §3 decision 28) — the same check flash/reset/run_study already run mid-attach, callable on its own without touching hardware otherwise. On a match, returns the enrolled board's fields. On a topology mismatch (the attached chip no longer matches what was recorded), returns an error naming both the recorded and live hardware IDs plus a fix_it_url pointing at embarch-topology's UI — relayed as text, never auto-opened (embarch-topology/design.md §3 decision 12: opening/focusing the UI is the caller's job). On no board enrolled under role yet, returns a plain not-enrolled error.")]
    async fn validate(
        &self,
        Parameters(ValidateParams { role }): Parameters<ValidateParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.validate(&role).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "ok": true,
                "role": resp.role,
                "probe_serial": resp.probe_serial,
                "chip": resp.chip,
                "hardware_id": resp.hardware_id,
                "confirmed_at_utc_ms": resp.confirmed_at_utc_ms,
            })),
            Err(e) => match e.downcast_ref::<TopologyMismatchError>() {
                Some(mismatch) => Self::err_text(format!(
                    "topology mismatch for role '{}' (probe {}, chip '{}'): {} (recorded \
                     hardware_id {}, live {:?}) — fix it at {}",
                    mismatch.role,
                    mismatch.probe_serial,
                    mismatch.chip,
                    mismatch.reason,
                    mismatch.recorded_hardware_id,
                    mismatch.live_hardware_id,
                    mismatch.fix_it_url
                )),
                None => Self::err_text(format!("validate failed: {e:#}")),
            },
        }
    }

    #[tool(description = "List the most recent topology-mismatch alerts from embarch-core's durable log via GET /alerts (design.md §3 decision 28) — what a validate 409, or a mismatch caught mid-flash/reset/run_study, gets logged as. Defaults to the 20 most recent.")]
    async fn alerts(
        &self,
        Parameters(AlertsParams { limit }): Parameters<AlertsParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.alerts(limit.unwrap_or(20)).await {
            Ok(alerts) => Self::ok_json(serde_json::json!({
                "alerts": alerts.into_iter().map(|a| serde_json::json!({
                    "id": a.id,
                    "occurred_at_utc_ms": a.occurred_at_utc_ms,
                    "role": a.role,
                    "probe_serial": a.probe_serial,
                    "chip": a.chip,
                    "recorded_hardware_id": a.recorded_hardware_id,
                    "live_hardware_id": a.live_hardware_id,
                    "reason": a.reason,
                })).collect::<Vec<_>>(),
            })),
            Err(e) => Self::err_text(format!("alerts failed: {e:#}")),
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

    #[tool(description = "Submit a Study (embarch-study-designer's schema: name, steps, validations, steps_crc) for embarch-core to run against whatever DUT is connected through its dev-bench serial link. No project param — a study isn't tied to a configured project, unlike build/flash. steps_crc is recomputed from steps and overwritten regardless of what's submitted. Returns { study_id } immediately (async) — call study_status to poll progress. Errors if a study is already in-flight on Core.")]
    async fn run_study(
        &self,
        Parameters(RunStudyParams { study }): Parameters<RunStudyParams>,
    ) -> Result<CallToolResult, McpError> {
        let study = match unwrap_stringified_json(study) {
            Ok(v) => v,
            Err(e) => {
                return Self::err_text(format!(
                    "study was sent as a JSON-encoded string but didn't parse as JSON: {e}"
                ))
            }
        };
        let mut study: embarch_study_designer::Study = match serde_json::from_value(study) {
            Ok(s) => s,
            Err(e) => {
                return Self::err_text(format!("study did not match the expected Study schema: {e}"))
            }
        };

        // design.md §3 decision 26: recompute and overwrite steps_crc
        // unconditionally, regardless of whatever value (including a
        // missing/zero one) was in the submitted JSON.
        if crate::study::recompute_steps_crc(&mut study).is_err() {
            return Self::err_text(
                "one step's postcard encoding was too large to compute steps_crc over \
                 (StepTooLargeError) — should be unreachable given embarch-study-designer's \
                 configured limits",
            );
        }

        match self.core.post_study(&study).await {
            Ok(resp) => Self::ok_json(serde_json::json!({ "study_id": resp.study_id })),
            Err(e) => match e.downcast_ref::<StudyConflictError>() {
                Some(conflict) => Self::err_text(format!(
                    "a study is already in-flight on embarch-core (study_id: {})",
                    conflict.study_id
                )),
                None => Self::err_text(format!("run_study failed: {e:#}")),
            },
        }
    }

    #[tool(description = "Get a submitted study's status via embarch-core: status (\"pending\"|\"running\"|\"completed\"|\"failed\"), current_step, total_steps, result (once completed), and reason (once failed) — returned verbatim.")]
    async fn study_status(
        &self,
        Parameters(StudyIdParams { study_id }): Parameters<StudyIdParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.get_study_status(&study_id).await {
            Ok(resp) => Self::ok_json(serde_json::json!({
                "status": resp.status,
                "current_step": resp.current_step,
                "total_steps": resp.total_steps,
                "result": resp.result,
                "reason": resp.reason,
            })),
            Err(e) => Self::err_text(format!("study_status failed for '{study_id}': {e:#}")),
        }
    }

    #[tool(description = "Fetch a study's power-measurement CSV data via embarch-core, returned as text content. A study with no power_sample steps has no power data — that's a clear error naming study_id, not empty output.")]
    async fn study_power_data(
        &self,
        Parameters(StudyIdParams { study_id }): Parameters<StudyIdParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.get_study_power_data(&study_id).await {
            Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(csv) => Ok(CallToolResult::success(vec![ContentBlock::text(csv)])),
                Err(e) => Self::err_text(format!("power-data response wasn't valid UTF-8: {e}")),
            },
            Err(e) => Self::err_text(format!("study_power_data failed for '{study_id}': {e:#}")),
        }
    }

    #[tool(description = "Fetch a study's waveform CSV data via embarch-core, returned as text content. A study with no StreamCapture steps has no waveform data — that's a clear error naming study_id, not empty output.")]
    async fn study_waveform_data(
        &self,
        Parameters(StudyIdParams { study_id }): Parameters<StudyIdParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.get_study_waveform_data(&study_id).await {
            Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(csv) => Ok(CallToolResult::success(vec![ContentBlock::text(csv)])),
                Err(e) => Self::err_text(format!("waveform-data response wasn't valid UTF-8: {e}")),
            },
            Err(e) => Self::err_text(format!("study_waveform_data failed for '{study_id}': {e:#}")),
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

#[cfg(test)]
mod run_study_tests {
    use super::unwrap_stringified_json;

    #[test]
    fn a_json_encoded_string_is_parsed_into_the_object_it_names() {
        let wrapped = serde_json::Value::String(r#"{"name":"x","steps":[]}"#.to_string());
        let unwrapped = unwrap_stringified_json(wrapped).unwrap();
        assert_eq!(unwrapped, serde_json::json!({"name": "x", "steps": []}));
    }

    #[test]
    fn an_inline_object_passes_through_untouched() {
        let obj = serde_json::json!({"name": "x", "steps": []});
        assert_eq!(unwrap_stringified_json(obj.clone()).unwrap(), obj);
    }

    #[test]
    fn a_string_that_is_not_valid_json_is_a_clear_error_not_a_panic() {
        let wrapped = serde_json::Value::String("not json at all".to_string());
        assert!(unwrap_stringified_json(wrapped).is_err());
    }

    #[test]
    fn a_non_string_non_object_value_also_passes_through() {
        // Not a valid Study either way -- Study's own deserialization is
        // what should reject this, not this unwrap step.
        let n = serde_json::json!(42);
        assert_eq!(unwrap_stringified_json(n.clone()).unwrap(), n);
    }
}
