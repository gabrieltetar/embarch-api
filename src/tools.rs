use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::build::{BuildLocks, BuildOutcome};
use embarch_api::json_out;
use crate::config::{Config, ProjectConfig};
use embarch_core_client::{
    CoreClient, FollowItem, FollowMode, FollowOptions, StudyConflictError, StudyEvent,
    TopologyMismatchError,
};
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

    /// A successful tool result whose content is a JSON object.
    ///
    /// Serialized through [`json_out`] rather than directly, so an MCP
    /// result carries the same `schema_version` a CLI `--json` object does —
    /// which is what keeps `interfaces/tools.md`'s "the same fields the MCP
    /// result does" true in both directions (decision 50).
    fn ok_json(value: serde_json::Value) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            json_out::pretty(value),
        )]))
    }

    /// A tool **error** whose content is a JSON object rather than prose —
    /// a failed build, a refused flash. Stamped like every other object
    /// this crate emits: an agent parsing a failure is the last caller that
    /// should be handed the one shape without the version field.
    fn err_json(value: serde_json::Value) -> Result<CallToolResult, McpError> {
        Self::err_text(json_out::pretty(value))
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
/// decision 12), shared by every tool that resolves a build target. A
/// `discovery = "static"` project **refuses** any of them, naming which were
/// given (`design.md` §3 decision 51) — it builds its configured
/// `build_command` verbatim and has nowhere to apply them.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct TargetParams {
    /// Name of a project from embarch-api's config file.
    pub project: String,
    /// Zephyr board name. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it. See `list_targets`.
    pub board: Option<String>,
    /// Board variant (e.g. a product LED configuration). Only for a
    /// discovery = "zephyr-west" project — a static project refuses it.
    pub variant: Option<String>,
    /// Hardware revision. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it rather than ignoring it.
    pub revision: Option<String>,
    /// App directory name under app/. Only for a discovery = "zephyr-west"
    /// project — a static project refuses it.
    pub app: Option<String>,
    /// `-S` snippets to build with. Only for a discovery = "zephyr-west"
    /// project — a static project refuses them. See `list_targets`'s
    /// `snippets_by_app`. Omitted or empty falls back to the project's
    /// configured default_snippets, not "no snippets"; pass the reserved
    /// literal ["none"] to force no snippets despite that default. Mixing
    /// "none" with real names is refused, not guessed at.
    pub snippets: Option<Vec<String>>,
    /// Extra `west build` flags (e.g. `["-p", "always"]`). Only for a
    /// discovery = "zephyr-west" project — a static project refuses them.
    /// Opaque passthrough, unlike snippets — not validated against anything.
    /// Omitted or empty falls back to the project's configured
    /// default_extra_args, not "no extra args".
    pub extra_args: Option<Vec<String>>,
    /// Fully erase the chip before writing, rather than erasing only the
    /// sectors the new image covers — the equivalent of `west flash --erase`.
    /// Without it, flash regions the new image doesn't cover survive from
    /// the previous firmware, including a Zephyr settings/NVS partition and
    /// so any BLE bonds or provisioning state held there. Defaults to false. Only meaningful for `build_and_flash` — `build` and
    /// `reset` don't write flash, and ignore it.
    pub erase: Option<bool>,
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
    /// Zephyr board name. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it rather than ignoring it.
    pub board: Option<String>,
    /// Board variant. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it.
    pub variant: Option<String>,
    /// Hardware revision. Only for a discovery = "zephyr-west" project — a
    /// static project refuses it.
    pub revision: Option<String>,
    /// App directory name under app/. Only for a discovery = "zephyr-west"
    /// project — a static project refuses it.
    pub app: Option<String>,
    /// `-S` snippets to build with. Only for a discovery = "zephyr-west"
    /// project — a static project refuses them. Omitted or empty falls back
    /// to the project's configured default_snippets, not "no snippets"; pass
    /// the reserved literal ["none"] to force no snippets despite that
    /// default. Mixing "none" with real names is refused, not guessed at.
    pub snippets: Option<Vec<String>>,
    /// Extra `west build` flags. Only for a discovery = "zephyr-west"
    /// project — a static project refuses them. Omitted or empty falls back
    /// to the project's configured default_extra_args, not "no extra args".
    pub extra_args: Option<Vec<String>>,
    /// Path to a firmware file to flash instead of the project's configured
    /// artifact_path — use this to flash an already-built file without
    /// rebuilding. Bypasses target resolution entirely.
    pub firmware_path: Option<String>,
    /// Fully erase the chip before writing, rather than erasing only the
    /// sectors the new image covers — the equivalent of `west flash --erase`.
    /// Without it, flash regions the new image doesn't cover survive from
    /// the previous firmware, including a Zephyr settings/NVS partition and
    /// so any BLE bonds or provisioning state held there. Defaults to false.
    pub erase: Option<bool>,
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
    /// requires, steps, streams, steps_crc, streams_crc).
    /// Untyped here (rather than a typed Study field) since
    /// embarch-study-designer is a #![no_std] crate that doesn't depend on
    /// schemars; the object is validated by deserializing it into Study
    /// server-side, immediately on receipt. All three seals — steps_crc over
    /// steps, streams_crc over streams and protocols_crc over protocols —
    /// are recomputed and overwritten regardless of what's given.
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

    /// Which firmware to rebuild and reflash before running, from the
    /// working tree **as it currently stands**: "none" (the default),
    /// "dev-bench", "dut", or "both". Defaults to "none" because flashing is
    /// the destructive half — a study that merely observes a board somebody
    /// just flashed by hand must not silently overwrite it.
    ///
    /// This never checks out a git revision. If the tree isn't at the
    /// revision the study requires, the call fails naming both and leaves
    /// the tree (and, for the DUT, the board) exactly as they were.
    pub reflash: Option<String>,

    /// Proceed even though a version requirement isn't satisfied. The
    /// override is **recorded** in the result's
    /// provenance.overrides — a run that was waved through is never
    /// indistinguishable from one that met its requirements. Defaults to
    /// false.
    pub allow_version_mismatch: Option<bool>,

    /// Which configured project is the DUT. Required by reflash = "dut" or
    /// "both", ignored otherwise: a study isn't tied to a project, but
    /// rebuilding the DUT's firmware is, and there is nowhere else for the
    /// build target to come from.
    pub project: Option<String>,
    /// Zephyr board name for the DUT reflash. Only meaningful alongside
    /// project, for a discovery = "zephyr-west" project — see list_targets.
    /// A static DUT project refuses this and every field below it rather
    /// than ignoring it, which fails the run before anything is flashed.
    pub board: Option<String>,
    /// Board variant for the DUT reflash. Only meaningful alongside project.
    pub variant: Option<String>,
    /// Hardware revision for the DUT reflash. Only meaningful alongside project.
    pub revision: Option<String>,
    /// App directory name for the DUT reflash. Only meaningful alongside project.
    pub app: Option<String>,
    /// `-S` snippets for the DUT reflash. Omitted or empty falls back to the
    /// project's configured default_snippets, not "no snippets"; the reserved
    /// literal ["none"] forces no snippets despite that default.
    pub snippets: Option<Vec<String>>,
    /// Extra `west build` flags for the DUT reflash. Opaque passthrough.
    /// Omitted or empty falls back to the project's default_extra_args.
    pub extra_args: Option<Vec<String>>,
}

impl RunStudyParams {
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

/// `study_stream_data`'s params (`design.md` §3 decision 39) — one declared
/// tap's capture, by the name the `Study` gave it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StudyStreamParams {
    /// The study_id returned by run_study.
    pub study_id: String,
    /// The tap's declared name — `StreamTap.name` in the submitted Study.
    /// Call list_study_streams to see what a completed study actually
    /// captured rather than guessing.
    pub name: String,
    /// Return the tap's byte-for-byte capture instead of its rendered file.
    /// Only makes a difference for a tap whose declared encoding has a
    /// rendering at all (Samples, GattTranscript); a Raw or OutpostTrace tap
    /// has none and returns its raw bytes either way. Defaults to false.
    pub raw: Option<bool>,
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
/// plumbing involved, same posture as `study.rs`'s `reseal_study`.
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
    /// Fully erase the chip before writing, rather than erasing only the
    /// sectors the new image covers — the equivalent of `west flash --erase`.
    /// Without it, flash regions the new image doesn't cover survive from
    /// the previous firmware, including a Zephyr settings/NVS partition and
    /// so any BLE bonds or provisioning state held there. Defaults to false.
    pub erase: Option<bool>,
}

/// Shared by every tool that operates on an already-submitted study by id.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StudyIdParams {
    /// The study_id returned by run_study.
    pub study_id: String,
}

/// `study_watch`'s parameters — `study_id` plus the three bounds that make a
/// live stream safe to hand to a request/response tool call.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StudyWatchParams {
    /// The study_id returned by run_study.
    pub study_id: String,
    /// Stop watching after this many seconds and return what happened so
    /// far. Defaults to 60, capped at 600.
    ///
    /// An MCP call is request/response, so a watch that followed a study to
    /// its end unconditionally would hang the caller for the study's whole
    /// duration. The bound is the tool's contract, not a limitation: call it
    /// again to keep watching.
    pub wait_secs: Option<u64>,
    /// At most this many events in the returned array. Defaults to 100,
    /// capped at 1000. Anything past it is counted, not returned.
    pub max_events: Option<u32>,
    /// Return every SampleBatch and GattTranscript event individually
    /// instead of counting them. Defaults to false, and false is almost
    /// always right: a study with a power tap emits sample batches
    /// continuously, and a list of them is bulk data this tool is the wrong
    /// way to fetch — study_stream_data is the right one.
    pub include_samples: Option<bool>,
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
    /// Picks which currently-attached probe to enroll when more than one is
    /// present (`embarch-topology/design.md` §3 decision 15). Omitted,
    /// Core falls back to its "exactly one attached" requirement.
    #[serde(default)]
    pub probe_serial: Option<String>,
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
                    "base_address": p.base_address.map(|a| format!("{a:#x}")),
                    "source_path": p.source_path.display().to_string(),
                    "has_serial_defaults": p.serial_port.is_some(),
                })
            })
            .collect();
        Self::ok_json(serde_json::json!({ "projects": projects }))
    }

    #[tool(description = "List a project's buildable targets. For a discovery = \"zephyr-west\" project: live-scans boards/ and app/ and returns every file-backing-validated (board, soc, cpucluster, variant, revision, app) tuple, plus snippets_by_app (every real -S snippet available per app), default_snippets, and default_extra_args. For a discovery = \"static\" project: returns exactly one row — the project itself, with its configured build_command, chip and resolved artifact_path — because a static project has one target and no selection params it can honour (build/flash refuse them). For a zephyr-west project it also returns default_target: the configured base (board, variant, revision, app) a call narrows from, i.e. which of these rows a call that names nothing already resolves to.")]
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

    #[tool(description = "Build a configured project by running its build command (configured, or, for a discovery = \"zephyr-west\" project, assembled at call time from board/variant/revision/app/snippets/extra_args). snippets/extra_args, if omitted, fall back to the project's configured default_snippets/default_extra_args, not \"none\" — pass snippets = [\"none\"], the reserved literal alone, to force a build with no snippets despite a configured default (mixing it with real names is refused, not guessed at). board/variant/revision/app, if omitted, fall back per field to the project's configured default_target before narrowing the live scan. extra_args is opaque passthrough (e.g. [\"-p\", \"always\"] for a pristine rebuild) — unlike snippets, not validated against anything. A discovery = \"static\" project builds its configured build_command verbatim and REJECTS board/variant/revision/app/snippets/extra_args, naming which were given, rather than accepting and discarding them. Does not touch hardware. Use build_and_flash to build and then flash in one call.")]
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
                    Self::err_json(value)
                }
            }
            Err(e) => Self::err_text(format!("failed to run build for '{}': {e:#}", project.name)),
        }
    }

    #[tool(description = "Flash a firmware artifact via embarch-core. Defaults to the resolved artifact_path (configured, or computed at call time for a discovery = \"zephyr-west\" target), or pass firmware_path to flash a specific file without rebuilding — this bypasses the artifact path, but still resolves the target for its chip, so a discovery = \"static\" project still rejects board/variant/revision/app/snippets/extra_args either way.")]
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
                    params.erase.unwrap_or(false),
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
            .flash(&resolved.chip, &path, &project.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref(), params.erase.unwrap_or(false))
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

    #[tool(description = "Build a project and, only if the build succeeds and produces a fresh artifact, flash it via embarch-core. Refuses to flash a stale or failed build. A discovery = \"static\" project rejects board/variant/revision/app/snippets/extra_args rather than discarding them — see build.")]
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
            return Self::err_json(value);
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
                params.erase.unwrap_or(false),
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

    #[tool(description = "Build embarch-dev-bench's own firmware by running west build. No project param — dev-bench isn't a configured project, it's EmbArch's own test rig, one board at a time. Which board that is comes from [dev_bench] config (source_path, west_binary, board, chip, flash_format, artifact_path), not from a constant: this bench has been an nRF54L15DK and an ESP32-C5 and is an nRF54L15DK again, and the two disagree about every one of those. Does not touch hardware — use build_and_flash_dev_bench to build and then flash in one call.")]
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
                    Self::err_json(value)
                }
            }
            Err(e) => Self::err_text(format!("failed to run build for dev_bench: {e:#}")),
        }
    }

    #[tool(description = "Flash embarch-dev-bench's own firmware via embarch-core. Defaults to dev-bench's own configured build artifact, or pass firmware_path to flash a specific file without rebuilding.")]
    async fn flash_dev_bench(
        &self,
        Parameters(FlashDevBenchParams { firmware_path, erase }): Parameters<FlashDevBenchParams>,
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
            .flash(&resolved.chip, &path, &resolved.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref(), erase.unwrap_or(false))
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
            return Self::err_json(value);
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
                // build_and_flash_dev_bench takes no parameters at all (see
                // its own tool description), so there's nowhere to put an
                // erase request. Use flash_dev_bench with erase: true for
                // that.
                false,
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

    #[tool(description = "Reset a project's target chip via embarch-core. For a discovery = \"zephyr-west\" project, board/variant/revision/app select which target's chip to reset (extends design.md §3 decision 12's params to reset, for the same reason build/flash need them: there's no single stored chip to fall back to). A discovery = \"static\" project has one stored chip and rejects those params rather than discarding them.")]
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

    #[tool(description = "Enroll a physical probe with embarch-topology's enrollment storage (design.md decision 22), recording which board its serial number is wired to. Requires exactly one debug probe currently attached, unless probe_serial picks a specific one — Core refuses (naming every candidate) otherwise, since the whole point is knowing exactly which board is meant before confirming. Once enrolled, flash/reset/run_study all refuse to touch that probe unless a live hardware-ID readback still matches what was recorded here. No project param — this isn't build-target selection.")]
    async fn enroll_probe(
        &self,
        Parameters(EnrollProbeParams { role, chip, probe_serial }): Parameters<EnrollProbeParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.enroll_probe(&role, &chip, probe_serial.as_deref()).await {
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

    #[tool(description = "Explicit, non-destructive re-check of an already-enrolled board's live identity via embarch-core's POST /validate (design.md §3 decision 28) — the same check flash/reset/run_study already run mid-attach, callable on its own without touching hardware otherwise. On a match, returns the enrolled board's fields. On a topology mismatch (the attached chip no longer matches what was recorded), returns an error naming both the recorded and live hardware IDs plus a fix_it_url pointing at embarch-ui's Topology tab — relayed as text, never auto-opened (embarch-topology/design.md §3 decision 12: opening/focusing the UI is the caller's job). On no board enrolled under role yet, returns a plain not-enrolled error.")]
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

    #[tool(description = "Submit a Study (embarch-study-designer's schema: name, requires, steps, streams, steps_crc, streams_crc) for embarch-core to run against whatever DUT is connected through its dev-bench serial link. All three seals (steps_crc over steps, streams_crc over streams, protocols_crc over protocols) are recomputed and overwritten regardless of what's submitted. Returns { study_id } immediately (async) — call study_status to poll progress. Errors if a study is already in-flight on Core.\n\nStudy.requires names the dev-bench and DUT firmware builds the study is meant to run against ('any' if it genuinely doesn't matter). reflash says what to do about it: 'none' (default), 'dev-bench', 'dut', or 'both' — build and flash from the working tree AS IT CURRENTLY STANDS, then verify. This never runs git checkout: if the tree isn't at the revision the study wants, the call fails naming both revisions and leaves the tree, and the board, alone. Reflashing the DUT needs project (plus the usual board/variant/revision/app/snippets/extra_args, which a discovery = \"static\" DUT project rejects rather than discards — that failure happens before anything is built or flashed), since a study isn't project-shaped but a firmware build is. allow_version_mismatch proceeds anyway and the override is recorded in the result's provenance.overrides — never silently honoured.")]
    async fn run_study(
        &self,
        Parameters(params): Parameters<RunStudyParams>,
    ) -> Result<CallToolResult, McpError> {
        let reflash = match params.reflash.as_deref() {
            Some(raw) => match crate::reflash::ReflashTarget::parse(raw) {
                Ok(target) => target,
                Err(e) => return Self::err_text(format!("{e:#}")),
            },
            None => crate::reflash::ReflashTarget::None,
        };
        let study = params.study.clone();
        let study = match unwrap_stringified_json(study) {
            Ok(v) => v,
            Err(e) => {
                return Self::err_text(format!(
                    "study was sent as a JSON-encoded string but didn't parse as JSON: {e}"
                ))
            }
        };
        // Deserialized from `&study` rather than by value so the `Value`
        // survives a failure: decision 27's capacity message is built from it,
        // and only on the error path.
        let mut study: embarch_study_designer::Study =
            match serde::Deserialize::deserialize(&study) {
                Ok(s) => s,
                Err(e) => {
                    return match crate::capacity::explain(&study) {
                        Some(detail) => Self::err_text(format!("study {detail}")),
                        None => Self::err_text(format!(
                            "study did not match the expected Study schema: {e}"
                        )),
                    }
                }
            };

        // design.md §3 decision 26: recompute and overwrite all three of a
        // study's seals unconditionally, regardless of whatever values
        // (including missing/zero ones) were in the submitted JSON.
        if let Err(e) = crate::study::reseal_study(&mut study) {
            return Self::err_text(format!(
                "{e} — should be unreachable given embarch-study-designer's configured limits"
            ));
        }

        let request = crate::reflash::RunStudyRequest {
            reflash,
            allow_version_mismatch: params.allow_version_mismatch.unwrap_or(false),
            project: params.project.as_deref(),
            selection: params.selection(),
        };

        match crate::reflash::run_study(&self.config, &self.core, &self.build_locks, &study, request)
            .await
        {
            Ok(outcome) => Self::ok_json(outcome.to_json()),
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

    #[tool(description = "Watch a running study live via embarch-core's SSE event stream (GET /study/{id}/events) instead of polling: returns every step completion, status change and (optionally) sample batch that happened while watching, in order, as they were pushed. Bounded by wait_secs (default 60) — this is a request/response call, so it returns what happened in that window and you call it again to keep watching; `complete: true` means the study reached a terminal status and there is nothing left to watch.\n\nThis is an addition to study_status, not a replacement: study_status is still the way to get one snapshot or the finished StudyResult, and this tool falls back to polling it automatically if the live stream will not open or drops mid-study (`transport` says which happened).\n\nTwo different kinds of incompleteness are reported separately and must not be confused. `lagged` is embarch-core telling you IT dropped events because this subscriber could not keep up — the study is unaffected and its own record on disk is complete, so re-read it with study_status/study_steps. `events_omitted` is this tool's own max_events cap. Neither is an error.")]
    async fn study_watch(
        &self,
        Parameters(StudyWatchParams {
            study_id,
            wait_secs,
            max_events,
            include_samples,
        }): Parameters<StudyWatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let wait = wait_secs.unwrap_or(60).min(600);
        let max_events = max_events.unwrap_or(100).min(1000) as usize;
        let include_samples = include_samples.unwrap_or(false);

        let options = FollowOptions {
            deadline: Some(Duration::from_secs(wait)),
            ..FollowOptions::default()
        };

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut omitted: u64 = 0;
        // Bulk events, counted per tap rather than listed, when
        // `include_samples` is false. Counting is what makes this tool's
        // answer bounded in a way a study with a power tap does not break.
        let mut sample_batches: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut gatt_entries: u64 = 0;
        let mut saw_live = false;
        let mut saw_polling = false;

        let outcome = self
            .core
            .follow_study(&study_id, &options, |item| {
                match &item {
                    FollowItem::Transport { mode, .. } => match mode {
                        FollowMode::Live => saw_live = true,
                        FollowMode::Polling => saw_polling = true,
                    },
                    FollowItem::Event(StudyEvent::SampleBatch {
                        stream_name,
                        samples,
                        ..
                    }) if !include_samples => {
                        let entry = sample_batches.entry(stream_name.clone()).or_insert((0, 0));
                        entry.0 += 1;
                        entry.1 += samples.len() as u64;
                        return;
                    }
                    FollowItem::Event(StudyEvent::GattTranscript { .. }) if !include_samples => {
                        gatt_entries += 1;
                        return;
                    }
                    _ => {}
                }
                if events.len() < max_events {
                    events.push(item.to_json());
                } else {
                    omitted += 1;
                }
            })
            .await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                return Self::err_text(format!(
                    "study_watch failed for '{study_id}': {e:#}"
                ))
            }
        };

        let transport = match (saw_live, saw_polling) {
            (true, true) => "live+polling",
            (true, false) => "live",
            (false, true) => "polling",
            // follow_study always announces its transport, so this is
            // unreachable; named rather than unwrapped so a future change
            // to that guarantee shows up as a string and not a panic.
            (false, false) => "unknown",
        };

        Self::ok_json(serde_json::json!({
            "study_id": study_id,
            "status": outcome.terminal_status,
            "reason": outcome.reason,
            "complete": outcome.terminal_status.is_some(),
            "timed_out": outcome.timed_out,
            "watched_secs": wait,
            "transport": transport,
            "lagged": if outcome.lagged_events > 0 {
                serde_json::json!({
                    "events": outcome.lagged_events,
                    "note": embarch_core_client::LAGGED_NOTE,
                })
            } else {
                serde_json::Value::Null
            },
            "events": events,
            "events_omitted": omitted,
            "sample_batches": if include_samples {
                serde_json::Value::Null
            } else {
                serde_json::Value::Object(
                    sample_batches
                        .into_iter()
                        .map(|(name, (batches, samples))| {
                            (name, serde_json::json!({ "batches": batches, "samples": samples }))
                        })
                        .collect(),
                )
            },
            "gatt_entries": if include_samples { serde_json::Value::Null } else { serde_json::json!(gatt_entries) },
        }))
    }

    #[tool(description = "Alias for study_stream_data, kept for one release: fetches whichever declared tap answers the 'power' alias (a Samples-encoded tap on a PowerFrontEnd source), as rendered CSV text. Prefer study_stream_data { study_id, name } — a study can declare several taps and only one of them can answer this alias. Call list_study_streams to see what a completed study actually captured, including whether a capture was truncated, which this tool cannot tell you. A study that declared no power tap has no power data, and that's a clear error naming study_id, not empty output.")]
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

    #[tool(description = "Alias for study_stream_data, kept for one release: fetches whichever declared tap answers the 'waveform' alias (a Samples-encoded tap on any source other than PowerFrontEnd), as rendered CSV text. Prefer study_stream_data { study_id, name }, and call list_study_streams to see what a study actually captured and whether it was truncated. A study that declared no such tap has no waveform data — that's a clear error naming study_id, not empty output.")]
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

    #[tool(description = "Alias for study_stream_data, kept for one release: fetches whichever declared tap answers the 'gatt' alias (a GattTranscript-encoded tap), as rendered CSV text. This is the exhaustive record — every notification, indication, read, write, subscribe and connect/disconnect event across every step, with each payload in both hex and printable-ASCII columns — Every study with a monitor step gets one automatically as of schema v14 (embarch-study-designer/design.md decision 54, which retired the capped per-step gatt_activity that used to be the only inline record). Prefer study_stream_data { study_id, name }, and call list_study_streams to see what a study captured and whether it was truncated. A study with no GATT transcript tap has none; that's a clear error naming study_id, not empty output.")]
    async fn study_gatt_data(
        &self,
        Parameters(StudyIdParams { study_id }): Parameters<StudyIdParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.get_study_gatt_data(&study_id).await {
            Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(csv) => Ok(CallToolResult::success(vec![ContentBlock::text(csv)])),
                Err(e) => Self::err_text(format!("gatt-data response wasn't valid UTF-8: {e}")),
            },
            Err(e) => Self::err_text(format!("study_gatt_data failed for '{study_id}': {e:#}")),
        }
    }

    #[tool(description = "Fetch one declared stream tap's capture from a study, by the name the Study gave it. Replaces study_power_data/study_waveform_data/study_gatt_data, which are now aliases over the same mechanism and each answer for at most one tap. Returns the tap's rendered file when its declared StreamEncoding has one (CSV for Samples and GattTranscript), or its byte-for-byte capture when it doesn't (Raw, OutpostTrace) or when raw is true. What a tap's bytes mean is declared in the Study, never guessed from their content. Call list_study_streams first rather than guessing a name: a 404 names the taps the study did declare, and also covers the separate case of a declared tap that captured nothing.")]
    async fn study_stream_data(
        &self,
        Parameters(StudyStreamParams { study_id, name, raw }): Parameters<StudyStreamParams>,
    ) -> Result<CallToolResult, McpError> {
        let raw = raw.unwrap_or(false);
        match self.core.get_study_stream(&study_id, &name, raw).await {
            Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
                // A `Raw`/`OutpostTrace` tap is bytes, and bytes are not
                // required to be text. Saying how many arrived, and that
                // they are on Core's disk, beats a decoding error that reads
                // like the capture failed.
                Err(e) => Self::err_text(format!(
                    "stream '{name}' of study '{study_id}' is {} bytes that aren't valid UTF-8 \
                     ({e}) — this is expected for a tap whose declared encoding is Raw or \
                     OutpostTrace. The capture is intact on embarch-core; fetch it with the \
                     study-stream-data CLI subcommand and --out to write it to a file.",
                    e.as_bytes().len()
                )),
            },
            Err(e) => Self::err_text(format!(
                "study_stream_data failed for '{study_id}' stream '{name}': {e:#}"
            )),
        }
    }

    #[tool(description = "List what a completed study actually captured: one entry per declared stream tap, with its name, how many bytes it wrote, and whether it was TRUNCATED. Read truncated: it is how you learn a capture is short rather than complete — either a retention rotation deleted a segment, or dev-bench reported dropping records — and a capture that lost data must not be read as a whole one. An entry with bytes_written 0 is a tap that was declared and produced nothing, which is a different fact from a tap that wasn't declared at all. Only a completed study has this; a pending, running or failed one returns its status instead. Use the names from here with study_stream_data.")]
    async fn list_study_streams(
        &self,
        Parameters(StudyIdParams { study_id }): Parameters<StudyIdParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.core.get_study_status(&study_id).await {
            Ok(resp) => match resp.result {
                Some(result) => Self::ok_json(serde_json::json!({
                    "study_id": study_id,
                    "status": resp.status,
                    "streams": streams_json(&result),
                })),
                None => Self::ok_json(serde_json::json!({
                    "study_id": study_id,
                    "status": resp.status,
                    "streams": serde_json::Value::Null,
                    "reason": resp.reason.unwrap_or_else(|| {
                        "no result yet — a study reports what it captured once it completes"
                            .to_string()
                    }),
                })),
            },
            Err(e) => Self::err_text(format!("list_study_streams failed for '{study_id}': {e:#}")),
        }
    }
}

/// `StudyResult.streams` as JSON (`embarch-study-designer/design.md` §4.8).
///
/// Carries `truncated` through verbatim, which is the field this listing
/// exists for: `StreamRef.truncated` is set both when a retention rotation
/// deleted a segment and when a `StreamClose` reported a non-zero `dropped`,
/// and a listing that dropped it would hand back a capture that reads
/// complete and isn't. Shared by the MCP tool and the CLI subcommand so the
/// two cannot disagree about what a stream listing is.
pub fn streams_json(result: &embarch_study_designer::StudyResult) -> serde_json::Value {
    serde_json::Value::Array(
        result
            .streams
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name.as_str(),
                    "bytes_written": s.bytes_written,
                    "truncated": s.truncated,
                })
            })
            .collect(),
    )
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
