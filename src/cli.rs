use std::path::Path;
use std::sync::Arc;

use crate::build::BuildOutcome;
use crate::config::{Config, ProjectConfig};
use crate::core_client::{CoreClient, StudyConflictError};
use crate::resolve::{self, Resolved, Selection};
use crate::{Commands, TargetSelection};

pub async fn run(command: Commands, json: bool, config: Arc<Config>, core: CoreClient) -> i32 {
    match command {
        Commands::ListProjects => list_projects(&config, json),
        Commands::ListTargets { project } => list_targets(&config, &project, json),
        Commands::Status => status(&core, json).await,
        Commands::Build { project, target } => build(&config, &core, &project, &target, json).await,
        Commands::Flash {
            project,
            target,
            firmware_path,
        } => flash(&config, &core, &project, &target, firmware_path, json).await,
        Commands::BuildAndFlash { project, target } => {
            build_and_flash(&config, &core, &project, &target, json).await
        }
        Commands::Reset { project, target } => reset(&config, &core, &project, &target, json).await,
        Commands::SerialLog {
            project,
            port,
            baud,
            duration_ms,
        } => serial_log(&config, &core, &project, port, baud, duration_ms, json).await,
        Commands::RunStudy { study_file } => run_study(&core, &study_file, json).await,
        Commands::StudyStatus { study_id } => study_status(&core, &study_id, json).await,
        Commands::StudyPowerData { study_id, out } => {
            study_power_data(&core, &study_id, out.as_deref(), json).await
        }
        Commands::StudyWaveformData { study_id, out } => {
            study_waveform_data(&core, &study_id, out.as_deref(), json).await
        }
    }
}

fn lookup_project<'a>(config: &'a Config, name: &str) -> Result<&'a ProjectConfig, String> {
    config.project(name).map_err(|e| e.to_string())
}

impl TargetSelection {
    fn selection(&self) -> Selection<'_> {
        Selection {
            board: self.board.as_deref(),
            variant: self.variant.as_deref(),
            revision: self.revision.as_deref(),
            app: self.app.as_deref(),
            snippets: &self.snippet,
            extra_args: &self.extra_arg,
        }
    }
}

async fn resolve_or_exit(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    json: bool,
) -> Result<Resolved, i32> {
    let project = match lookup_project(config, project_name) {
        Ok(p) => p,
        Err(e) => return Err(error_result(json, e)),
    };
    resolve::resolve(project, target.selection(), core)
        .await
        .map_err(|e| error_result(json, format!("{e:#}")))
}

/// Prints `value` (in `--json` mode) or `human` (otherwise) to the right
/// stream and returns the process exit code, per design.md §5a: `0` on
/// success, `1` on any operation failure, distinguished only by the
/// message/JSON text, never a per-failure-kind code.
fn finish(json: bool, success: bool, value: serde_json::Value, human: String) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|e| format!(
                "{{\"success\": false, \"error\": \"failed to serialize result: {e}\"}}"
            ))
        );
    } else if success {
        println!("{human}");
    } else {
        eprintln!("{human}");
    }
    if success {
        0
    } else {
        1
    }
}

fn error_result(json: bool, message: String) -> i32 {
    finish(
        json,
        false,
        serde_json::json!({ "success": false, "error": message }),
        message,
    )
}

fn list_projects(config: &Config, json: bool) -> i32 {
    let projects: Vec<_> = config
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

    let human = if config.projects.is_empty() {
        "no projects configured".to_string()
    } else {
        config
            .projects
            .iter()
            .map(|p| {
                format!(
                    "{} (discovery={}, chip={}, flash_format={}, source_path={}, serial_defaults={})",
                    p.name,
                    if p.is_zephyr_west() { "zephyr-west" } else { "static" },
                    p.chip.as_deref().unwrap_or("<resolved per call>"),
                    p.flash_format,
                    p.source_path.display(),
                    p.serial_port.is_some()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    finish(
        json,
        true,
        serde_json::json!({ "success": true, "projects": projects }),
        human,
    )
}

fn list_targets(config: &Config, project_name: &str, json: bool) -> i32 {
    let project = match lookup_project(config, project_name) {
        Ok(p) => p,
        Err(e) => return error_result(json, e),
    };

    match resolve::list_targets(project) {
        Ok(mut value) => {
            let human = serde_json::to_string_pretty(&value).unwrap_or_default();
            // Merge "success" into the full value rather than rebuilding a
            // "targets"-only object — for a zephyr-west project, value also
            // carries snippets_by_app/default_snippets/default_extra_args
            // (resolve::list_targets), which an earlier version of this
            // rebuild silently dropped from --json output (the human-text
            // form above was unaffected, since it's built from the full
            // value already).
            if let Some(obj) = value.as_object_mut() {
                obj.insert("success".to_string(), serde_json::Value::Bool(true));
            }
            finish(json, true, value, human)
        }
        Err(e) => error_result(json, format!("{e:#}")),
    }
}

async fn status(core: &CoreClient, json: bool) -> i32 {
    match core.status().await {
        Ok(status) => {
            let probes: Vec<_> = status
                .probes
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "identifier": p.identifier,
                        "vendor_id": p.vendor_id,
                        "product_id": p.product_id,
                        "serial_number": p.serial_number,
                    })
                })
                .collect();

            let human = if status.probes.is_empty() {
                format!("core status: {} (no probes connected)", status.status)
            } else {
                let probe_lines: Vec<_> = status
                    .probes
                    .iter()
                    .map(|p| {
                        format!(
                            "  {} (vid={:#06x}, pid={:#06x}, serial={})",
                            p.identifier,
                            p.vendor_id,
                            p.product_id,
                            p.serial_number.as_deref().unwrap_or("<none>")
                        )
                    })
                    .collect();
                format!("core status: {}\n{}", status.status, probe_lines.join("\n"))
            };

            finish(
                json,
                true,
                serde_json::json!({ "success": true, "status": status.status, "probes": probes }),
                human,
            )
        }
        Err(e) => error_result(json, format!("embarch-core unreachable or returned an error: {e:#}")),
    }
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

fn build_human_summary(project: &str, outcome: &BuildOutcome) -> String {
    if outcome.build_succeeded() {
        let mut summary = format!(
            "build succeeded for '{}': artifact={} (fresh={})",
            project,
            outcome.artifact_path.display(),
            outcome.artifact_fresh
        );
        if !outcome.artifact_fresh {
            summary.push_str("\nwarning: build exited 0 but no fresh artifact was found at artifact_path");
        }
        summary
    } else if outcome.timed_out {
        format!(
            "build timed out for '{project}'\n--- stdout ---\n{}\n--- stderr ---\n{}",
            outcome.stdout, outcome.stderr
        )
    } else {
        format!(
            "build failed for '{}' (exit_code={:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            project, outcome.exit_code, outcome.stdout, outcome.stderr
        )
    }
}

async fn build(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    json: bool,
) -> i32 {
    let resolved = match resolve_or_exit(config, core, project_name, target, json).await {
        Ok(r) => r,
        Err(code) => return code,
    };

    let build_locks = crate::build::BuildLocks::new();
    match build_locks.run_build(&resolved.plan).await {
        Ok(outcome) => {
            let mut value = build_outcome_json(&outcome);
            value["success"] = serde_json::Value::Bool(outcome.build_succeeded());
            value["target"] = resolved.descriptor.clone();
            let human = build_human_summary(project_name, &outcome);
            finish(json, outcome.build_succeeded(), value, human)
        }
        Err(e) => error_result(json, format!("failed to run build for '{project_name}': {e:#}")),
    }
}

async fn flash(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    firmware_path: Option<String>,
    json: bool,
) -> i32 {
    let resolved = match resolve_or_exit(config, core, project_name, target, json).await {
        Ok(r) => r,
        Err(code) => return code,
    };

    // Always the WSL2-local path — `core.flash` itself decides whether that
    // can go straight to Core as-is or needs uploading, based on topology
    // (`core_client.rs`'s own doc comment, `design.md` §9).
    let path = firmware_path.unwrap_or_else(|| resolved.plan.artifact_path.display().to_string());

    match core.flash(&resolved.chip, &path, &resolved.flash_format).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({
                "success": true,
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": path,
                "target": resolved.descriptor,
            }),
            format!("flashed '{project_name}' via chip {} ({})", resp.chip, path),
        ),
        Err(e) => error_result(json, format!("flash failed for '{project_name}': {e:#}")),
    }
}

async fn build_and_flash(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    json: bool,
) -> i32 {
    let resolved = match resolve_or_exit(config, core, project_name, target, json).await {
        Ok(r) => r,
        Err(code) => return code,
    };

    let build_locks = crate::build::BuildLocks::new();
    let outcome = match build_locks.run_build(&resolved.plan).await {
        Ok(outcome) => outcome,
        Err(e) => return error_result(json, format!("failed to run build for '{project_name}': {e:#}")),
    };

    let mut build_json = build_outcome_json(&outcome);
    build_json["target"] = resolved.descriptor.clone();

    if !outcome.ready_to_flash() {
        let mut value = build_json;
        value["success"] = serde_json::Value::Bool(false);
        let reason = if outcome.timed_out {
            "build timed out".to_string()
        } else if outcome.exit_code != Some(0) {
            "build failed".to_string()
        } else {
            "build succeeded but no fresh artifact was found — refusing to flash".to_string()
        };
        value["reason"] = serde_json::Value::String(reason.clone());
        let human = format!("{reason} for '{project_name}' — refusing to flash");
        return finish(json, false, value, human);
    }

    // Always the WSL2-local path — see the sibling `flash` fn above.
    let core_firmware_path = outcome.artifact_path.display().to_string();

    match core.flash(&resolved.chip, &core_firmware_path, &resolved.flash_format).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({
                "success": true,
                "build": build_json,
                "flashed": resp.flashed,
                "chip": resp.chip,
                "firmware_path": core_firmware_path,
            }),
            format!(
                "build and flash succeeded for '{project_name}' via chip {} ({})",
                resp.chip, core_firmware_path
            ),
        ),
        Err(e) => error_result(
            json,
            format!("build succeeded but flash failed for '{project_name}': {e:#}"),
        ),
    }
}

async fn reset(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    json: bool,
) -> i32 {
    let resolved = match resolve_or_exit(config, core, project_name, target, json).await {
        Ok(r) => r,
        Err(code) => return code,
    };

    match core.reset(&resolved.chip).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "reset": resp.reset, "target": resolved.descriptor }),
            format!("reset '{project_name}': {}", resp.reset),
        ),
        Err(e) => error_result(json, format!("reset failed for '{project_name}': {e:#}")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn serial_log(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    port: Option<String>,
    baud: Option<u32>,
    duration_ms: Option<u64>,
    json: bool,
) -> i32 {
    let project = match lookup_project(config, project_name) {
        Ok(p) => p,
        Err(e) => return error_result(json, e),
    };

    let port = match port.or_else(|| project.serial_port.clone()) {
        Some(port) => port,
        None => {
            return error_result(
                json,
                format!(
                    "no serial port given and project '{}' has no configured serial_port",
                    project.name
                ),
            )
        }
    };
    let baud = baud.or(project.serial_baud).unwrap_or(115_200);
    let duration_ms = duration_ms.unwrap_or(2_000);

    match core.serial_log(&port, baud, duration_ms).await {
        Ok(resp) => {
            let human = if resp.lines.is_empty() {
                format!("serial-log on {}: no lines captured", resp.port)
            } else {
                format!("serial-log on {}:\n{}", resp.port, resp.lines.join("\n"))
            };
            finish(
                json,
                true,
                serde_json::json!({ "success": true, "port": resp.port, "lines": resp.lines }),
                human,
            )
        }
        Err(e) => error_result(json, format!("serial-log failed for '{}': {e:#}", project.name)),
    }
}

async fn run_study(core: &CoreClient, study_file: &Path, json: bool) -> i32 {
    let raw = match std::fs::read_to_string(study_file) {
        Ok(raw) => raw,
        Err(e) => {
            return error_result(
                json,
                format!("failed to read study file {}: {e}", study_file.display()),
            )
        }
    };

    let mut study: embarch_study_designer::Study = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            return error_result(
                json,
                format!(
                    "study file {} did not match the expected Study schema: {e}",
                    study_file.display()
                ),
            )
        }
    };

    // design.md §3 decision 26: recompute and overwrite steps_crc
    // unconditionally, regardless of whatever value (including a
    // missing/zero one) was in the submitted JSON.
    if crate::study::recompute_steps_crc(&mut study).is_err() {
        return error_result(
            json,
            "one step's postcard encoding was too large to compute steps_crc over \
             (StepTooLargeError) — should be unreachable given embarch-study-designer's \
             configured limits"
                .to_string(),
        );
    }

    match core.post_study(&study).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "study_id": resp.study_id }),
            format!("study submitted: study_id={}", resp.study_id),
        ),
        Err(e) => match e.downcast_ref::<StudyConflictError>() {
            Some(conflict) => error_result(
                json,
                format!(
                    "a study is already in-flight on embarch-core (study_id: {})",
                    conflict.study_id
                ),
            ),
            None => error_result(json, format!("run-study failed: {e:#}")),
        },
    }
}

async fn study_status(core: &CoreClient, study_id: &str, json: bool) -> i32 {
    match core.get_study_status(study_id).await {
        Ok(resp) => {
            let human = format!(
                "study {study_id}: status={} current_step={:?} total_steps={:?} reason={:?}",
                resp.status, resp.current_step, resp.total_steps, resp.reason
            );
            finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "status": resp.status,
                    "current_step": resp.current_step,
                    "total_steps": resp.total_steps,
                    "result": resp.result,
                    "reason": resp.reason,
                }),
                human,
            )
        }
        Err(e) => error_result(json, format!("study-status failed for '{study_id}': {e:#}")),
    }
}

/// Shared by `study-power-data`/`study-waveform-data`: write `bytes` to
/// `out` if given, else straight to stdout. Raw payload data, unlike every
/// other subcommand's output — `--json` only changes how *status* is
/// reported (below, for the `--out` case), it never wraps a CSV payload as
/// a JSON string, and the no-`--out` stdout path stays free of any
/// wrapper/status text so it can be piped or redirected untouched.
fn write_study_csv(json: bool, kind: &str, study_id: &str, bytes: &[u8], out: Option<&Path>) -> i32 {
    match out {
        Some(path) => match std::fs::write(path, bytes) {
            Ok(()) => finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "study_id": study_id,
                    "bytes_written": bytes.len(),
                    "path": path.display().to_string(),
                }),
                format!(
                    "wrote {} bytes of {kind} for study {study_id} to {}",
                    bytes.len(),
                    path.display()
                ),
            ),
            Err(e) => error_result(json, format!("failed to write {kind} to {}: {e}", path.display())),
        },
        None => {
            use std::io::Write;
            match std::io::stdout().write_all(bytes) {
                Ok(()) => 0,
                Err(e) => error_result(json, format!("failed to write {kind} to stdout: {e}")),
            }
        }
    }
}

async fn study_power_data(core: &CoreClient, study_id: &str, out: Option<&Path>, json: bool) -> i32 {
    match core.get_study_power_data(study_id).await {
        Ok(bytes) => write_study_csv(json, "power-data", study_id, &bytes, out),
        Err(e) => error_result(json, format!("study-power-data failed for '{study_id}': {e:#}")),
    }
}

async fn study_waveform_data(core: &CoreClient, study_id: &str, out: Option<&Path>, json: bool) -> i32 {
    match core.get_study_waveform_data(study_id).await {
        Ok(bytes) => write_study_csv(json, "waveform-data", study_id, &bytes, out),
        Err(e) => error_result(json, format!("study-waveform-data failed for '{study_id}': {e:#}")),
    }
}
