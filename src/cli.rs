use std::path::Path;
use std::sync::Arc;

use crate::build::BuildOutcome;
use crate::config::{Config, ProjectConfig};
use embarch_core_client::{CoreClient, StudyConflictError, TopologyMismatchError};
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
            erase,
        } => flash(&config, &core, &project, &target, firmware_path, erase, json).await,
        Commands::BuildAndFlash {
            project,
            target,
            erase,
        } => build_and_flash(&config, &core, &project, &target, erase, json).await,
        Commands::Reset { project, target } => reset(&config, &core, &project, &target, json).await,
        Commands::SerialLog {
            project,
            port,
            baud,
            duration_ms,
        } => serial_log(&config, &core, &project, port, baud, duration_ms, json).await,
        Commands::RunStudy {
            study_file,
            reflash,
            allow_version_mismatch,
            project,
            target,
        } => {
            run_study(
                &config,
                &core,
                &study_file,
                &reflash,
                allow_version_mismatch,
                project.as_deref(),
                &target,
                json,
            )
            .await
        }
        Commands::StudyStatus { study_id } => study_status(&core, &study_id, json).await,
        Commands::StudyPowerData { study_id, out } => {
            study_power_data(&core, &study_id, out.as_deref(), json).await
        }
        Commands::StudyWaveformData { study_id, out } => {
            study_waveform_data(&core, &study_id, out.as_deref(), json).await
        }
        Commands::StudyGattData { study_id, out } => {
            study_gatt_data(&core, &study_id, out.as_deref(), json).await
        }
        Commands::StudyStreamData { study_id, name, raw, out } => {
            study_stream_data(&core, &study_id, &name, raw, out.as_deref(), json).await
        }
        Commands::ListStudyStreams { study_id } => {
            list_study_streams(&core, &study_id, json).await
        }
        Commands::BuildDevBench => build_dev_bench(&config, json).await,
        Commands::FlashDevBench { firmware_path, erase } => {
            flash_dev_bench(&config, &core, firmware_path, erase, json).await
        }
        Commands::BuildAndFlashDevBench { erase } => {
            build_and_flash_dev_bench(&config, &core, erase, json).await
        }
        Commands::ResetDevBench => reset_dev_bench(&config, &core, json).await,
        Commands::EnrollProbe { role, chip, probe_serial } => {
            enroll_probe(&core, &role, &chip, probe_serial.as_deref(), json).await
        }
        Commands::Validate { role } => validate(&core, &role, json).await,
        Commands::Alerts { limit } => alerts(&core, limit, json).await,
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
    erase: bool,
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

    match core
        .flash(&resolved.chip, &path, &resolved.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref(), erase)
        .await
    {
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
    erase: bool,
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

    match core
        .flash(
            &resolved.chip,
            &core_firmware_path,
            &resolved.flash_format,
            resolved.base_address.as_deref(),
            resolved.probe_serial.as_deref(),
            erase,
        )
        .await
    {
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

    match core.reset(&resolved.chip, resolved.probe_serial.as_deref()).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "reset": resp.reset, "target": resolved.descriptor }),
            format!("reset '{project_name}': {}", resp.reset),
        ),
        Err(e) => error_result(json, format!("reset failed for '{project_name}': {e:#}")),
    }
}

fn dev_bench_config_or_exit(config: &Config, json: bool) -> Result<&crate::config::DevBenchConfig, i32> {
    config.dev_bench.as_ref().ok_or_else(|| {
        error_result(
            json,
            "no [dev_bench] configured — add a [dev_bench] table (source_path, west_binary) \
             to build/flash embarch-dev-bench's own firmware"
                .to_string(),
        )
    })
}

async fn build_dev_bench(config: &Config, json: bool) -> i32 {
    let dev_bench = match dev_bench_config_or_exit(config, json) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let resolved = match crate::dev_bench::resolve(dev_bench) {
        Ok(r) => r,
        Err(e) => return error_result(json, format!("{e:#}")),
    };

    let build_locks = crate::build::BuildLocks::new();
    match build_locks.run_build(&resolved.plan).await {
        Ok(outcome) => {
            let mut value = build_outcome_json(&outcome);
            value["success"] = serde_json::Value::Bool(outcome.build_succeeded());
            value["target"] = resolved.descriptor.clone();
            let human = build_human_summary("dev_bench", &outcome);
            finish(json, outcome.build_succeeded(), value, human)
        }
        Err(e) => error_result(json, format!("failed to run build for dev_bench: {e:#}")),
    }
}

async fn flash_dev_bench(
    config: &Config,
    core: &CoreClient,
    firmware_path: Option<String>,
    erase: bool,
    json: bool,
) -> i32 {
    let dev_bench = match dev_bench_config_or_exit(config, json) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let resolved = match crate::dev_bench::resolve(dev_bench) {
        Ok(r) => r,
        Err(e) => return error_result(json, format!("{e:#}")),
    };

    let path = firmware_path.unwrap_or_else(|| resolved.plan.artifact_path.display().to_string());

    match core
        .flash(&resolved.chip, &path, &resolved.flash_format, resolved.base_address.as_deref(), resolved.probe_serial.as_deref(), erase)
        .await
    {
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
            format!("flashed dev_bench via chip {} ({})", resp.chip, path),
        ),
        Err(e) => error_result(json, format!("flash failed for dev_bench: {e:#}")),
    }
}

async fn build_and_flash_dev_bench(
    config: &Config,
    core: &CoreClient,
    erase: bool,
    json: bool,
) -> i32 {
    let dev_bench = match dev_bench_config_or_exit(config, json) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let resolved = match crate::dev_bench::resolve(dev_bench) {
        Ok(r) => r,
        Err(e) => return error_result(json, format!("{e:#}")),
    };

    let build_locks = crate::build::BuildLocks::new();
    let outcome = match build_locks.run_build(&resolved.plan).await {
        Ok(outcome) => outcome,
        Err(e) => return error_result(json, format!("failed to run build for dev_bench: {e:#}")),
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
        let human = format!("{reason} for dev_bench — refusing to flash");
        return finish(json, false, value, human);
    }

    let core_firmware_path = outcome.artifact_path.display().to_string();

    match core
        .flash(
            &resolved.chip,
            &core_firmware_path,
            &resolved.flash_format,
            resolved.base_address.as_deref(),
            resolved.probe_serial.as_deref(),
            erase,
        )
        .await
    {
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
                "build and flash succeeded for dev_bench via chip {} ({})",
                resp.chip, core_firmware_path
            ),
        ),
        Err(e) => error_result(json, format!("build succeeded but flash failed for dev_bench: {e:#}")),
    }
}

async fn reset_dev_bench(config: &Config, core: &CoreClient, json: bool) -> i32 {
    let dev_bench = match dev_bench_config_or_exit(config, json) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let resolved = match crate::dev_bench::resolve(dev_bench) {
        Ok(r) => r,
        Err(e) => return error_result(json, format!("{e:#}")),
    };

    match core.reset(&resolved.chip, resolved.probe_serial.as_deref()).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "reset": resp.reset, "target": resolved.descriptor }),
            format!("reset dev_bench: {}", resp.reset),
        ),
        Err(e) => error_result(json, format!("reset failed for dev_bench: {e:#}")),
    }
}

async fn enroll_probe(core: &CoreClient, role: &str, chip: &str, probe_serial: Option<&str>, json: bool) -> i32 {
    match core.enroll_probe(role, chip, probe_serial).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({
                "success": true,
                "probe_serial": resp.probe_serial,
                "role": resp.role,
                "chip": resp.chip,
                "hardware_id": resp.hardware_id,
                "confirmed_at_utc_ms": resp.confirmed_at_utc_ms,
            }),
            format!(
                "enrolled probe {} as role '{}' (chip '{}', hardware_id {})",
                resp.probe_serial, resp.role, resp.chip, resp.hardware_id
            ),
        ),
        Err(e) => error_result(json, format!("enroll-probe failed: {e:#}")),
    }
}

async fn validate(core: &CoreClient, role: &str, json: bool) -> i32 {
    match core.validate(role).await {
        Ok(resp) => finish(
            json,
            true,
            serde_json::json!({
                "success": true,
                "ok": true,
                "role": resp.role,
                "probe_serial": resp.probe_serial,
                "chip": resp.chip,
                "hardware_id": resp.hardware_id,
                "confirmed_at_utc_ms": resp.confirmed_at_utc_ms,
            }),
            format!("ok: '{}' still matches hardware_id {}", resp.role, resp.hardware_id),
        ),
        Err(e) => match e.downcast_ref::<TopologyMismatchError>() {
            // Relay the mismatch and its fix_it_url as text — never opened
            // automatically (`embarch-topology validate`'s own CLI does the
            // same; `embarch-topology/design.md` §3 decision 12).
            Some(mismatch) => error_result(
                json,
                format!(
                    "topology mismatch for role '{}' (probe {}, chip '{}'): {} (recorded \
                     hardware_id {}, live {:?}) — fix it at {}",
                    mismatch.role,
                    mismatch.probe_serial,
                    mismatch.chip,
                    mismatch.reason,
                    mismatch.recorded_hardware_id,
                    mismatch.live_hardware_id,
                    mismatch.fix_it_url
                ),
            ),
            None => error_result(json, format!("validate failed: {e:#}")),
        },
    }
}

async fn alerts(core: &CoreClient, limit: usize, json: bool) -> i32 {
    match core.alerts(limit).await {
        Ok(alerts) => {
            let human = if alerts.is_empty() {
                "no alerts recorded".to_string()
            } else {
                alerts
                    .iter()
                    .map(|a| format!("{} role={} probe={} reason={}", a.occurred_at_utc_ms, a.role, a.probe_serial, a.reason))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "alerts": alerts.iter().map(|a| serde_json::json!({
                        "id": a.id,
                        "occurred_at_utc_ms": a.occurred_at_utc_ms,
                        "role": a.role,
                        "probe_serial": a.probe_serial,
                        "chip": a.chip,
                        "recorded_hardware_id": a.recorded_hardware_id,
                        "live_hardware_id": a.live_hardware_id,
                        "reason": a.reason,
                    })).collect::<Vec<_>>(),
                }),
                human,
            )
        }
        Err(e) => error_result(json, format!("alerts failed: {e:#}")),
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

#[allow(clippy::too_many_arguments)]
async fn run_study(
    config: &Config,
    core: &CoreClient,
    study_file: &Path,
    reflash: &str,
    allow_version_mismatch: bool,
    project: Option<&str>,
    target: &TargetSelection,
    json: bool,
) -> i32 {
    let reflash = match crate::reflash::ReflashTarget::parse(reflash) {
        Ok(target) => target,
        Err(e) => return error_result(json, format!("{e:#}")),
    };

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

    // design.md §3 decision 26: recompute and overwrite both of a study's
    // seals unconditionally, regardless of whatever values (including
    // missing/zero ones) were in the submitted JSON.
    if let Err(e) = crate::study::reseal_study(&mut study) {
        return error_result(
            json,
            format!("{e} — should be unreachable given embarch-study-designer's configured limits"),
        );
    }

    let build_locks = crate::build::BuildLocks::new();
    let request = crate::reflash::RunStudyRequest {
        reflash,
        allow_version_mismatch,
        project,
        selection: target.selection(),
    };

    match crate::reflash::run_study(config, core, &build_locks, &study, request).await {
        Ok(outcome) => {
            let mut value = outcome.to_json();
            value["success"] = serde_json::Value::Bool(true);
            let mut human = format!("study submitted: study_id={}", outcome.study_id);
            for step in &outcome.reflashed {
                human = format!(
                    "reflashed {}: {}\n{human}",
                    step["target"].as_str().unwrap_or("?"),
                    step["artifact_path"].as_str().unwrap_or("?")
                );
            }
            finish(json, true, value, human)
        }
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

/// Shared by `study-power-data`/`study-waveform-data`/`study-gatt-data`: write `bytes` to
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

async fn study_gatt_data(core: &CoreClient, study_id: &str, out: Option<&Path>, json: bool) -> i32 {
    match core.get_study_gatt_data(study_id).await {
        Ok(bytes) => write_study_csv(json, "gatt-data", study_id, &bytes, out),
        Err(e) => error_result(json, format!("study-gatt-data failed for '{study_id}': {e:#}")),
    }
}

async fn study_stream_data(
    core: &CoreClient,
    study_id: &str,
    name: &str,
    raw: bool,
    out: Option<&Path>,
    json: bool,
) -> i32 {
    match core.get_study_stream(study_id, name, raw).await {
        Ok(bytes) => write_study_csv(json, &format!("stream '{name}'"), study_id, &bytes, out),
        Err(e) => error_result(
            json,
            format!("study-stream-data failed for '{study_id}' stream '{name}': {e:#}"),
        ),
    }
}

/// `StudyResult.streams` for a completed study — no new Core route, because
/// `GET /study/{id}` already returns the whole `StudyResult` inline once a
/// study completes and `streams` has been part of it since Milestone 7 Phase
/// B item 1. A listing endpoint Core doesn't need is a surface this suite
/// keeps deciding not to build.
async fn list_study_streams(core: &CoreClient, study_id: &str, json: bool) -> i32 {
    match core.get_study_status(study_id).await {
        Ok(resp) => match resp.result {
            Some(result) => {
                let streams = crate::tools::streams_json(&result);
                let human = if result.streams.is_empty() {
                    format!("study {study_id} declared no stream taps")
                } else {
                    result
                        .streams
                        .iter()
                        .map(|s| {
                            format!(
                                "{} — {} bytes{}",
                                s.name,
                                s.bytes_written,
                                if s.truncated { "  [TRUNCATED — this capture is short]" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                finish(
                    json,
                    true,
                    serde_json::json!({
                        "success": true,
                        "study_id": study_id,
                        "status": resp.status,
                        "streams": streams,
                    }),
                    human,
                )
            }
            None => finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "study_id": study_id,
                    "status": resp.status,
                    "streams": serde_json::Value::Null,
                }),
                format!(
                    "study {study_id} is {} — a study reports what it captured once it completes",
                    resp.status
                ),
            ),
        },
        Err(e) => error_result(json, format!("list-study-streams failed for '{study_id}': {e:#}")),
    }
}
