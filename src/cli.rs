use std::sync::Arc;

use crate::build::BuildOutcome;
use crate::config::{Config, ProjectConfig};
use crate::core_client::CoreClient;
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
        Ok(value) => {
            let human = serde_json::to_string_pretty(&value).unwrap_or_default();
            finish(json, true, serde_json::json!({ "success": true, "targets": value["targets"] }), human)
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

    let path = firmware_path
        .or(resolved.artifact_path_for_core.clone())
        .unwrap_or_else(|| resolved.plan.artifact_path.display().to_string());

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

    let artifact_path = outcome.artifact_path.display().to_string();
    let core_firmware_path = resolved
        .artifact_path_for_core
        .clone()
        .unwrap_or_else(|| artifact_path.clone());

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
