use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::build::BuildOutcome;
use embarch_api::json_out;
use crate::config::{Config, ProjectConfig};
use embarch_core_client::{
    render_hello_ack, CoreClient, DevBenchBusyError, DevBenchHandshakeError, FollowItem,
    FollowOptions, StudyConflictError, StudyEvent, TopologyMismatchError,
};
use embarch_study_designer::Outcome;
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
        Commands::Bootload { project, target, image_path } => {
            bootload(&config, &core, &project, &target, image_path.as_deref(), json).await
        }
        Commands::BuildAndBootload { project, target } => build_and_bootload(&config, &core, &project, &target, json).await,
        Commands::DeclareBootloadPorts { bootloader, bootloader_interface, app, app_interface } => {
            declare_bootload_ports(&core, app.as_deref(), app_interface, &bootloader, bootloader_interface, json).await
        }
        Commands::LeaseTake { holder, purpose, ttl_s } => {
            let report = crate::lease::take(
                &core,
                holder.as_deref().unwrap_or("embarch-api cli"),
                purpose.as_deref(),
                ttl_s.unwrap_or(crate::lease::CLI_DEFAULT_TTL_S),
            )
            .await;
            finish(json, report.success, report.value, report.human)
        }
        Commands::LeaseRenew { ttl_s } => {
            let report = crate::lease::renew(&core, ttl_s).await;
            finish(json, report.success, report.value, report.human)
        }
        Commands::LeaseRelease => {
            let report = crate::lease::release(&core).await;
            finish(json, report.success, report.value, report.human)
        }
        Commands::LeaseShow => {
            let report = crate::lease::show(&core).await;
            finish(json, report.success, report.value, report.human)
        }
        Commands::ShowBootloadPorts => show_bootload_ports(&core, json).await,
        Commands::ClearBootloadPorts => clear_bootload_ports(&core, json).await,
        Commands::DeclareInstrument {
            name,
            port,
            interface,
            preset,
            baud,
            write_terminator,
            read_terminator,
            min_interval_ms,
            ack_ok,
            ack_err,
        } => {
            let args = crate::instrument::DeclareArgs {
                name: &name,
                port: &port,
                interface,
                preset: preset.as_deref(),
                baud,
                write_terminator: write_terminator.as_deref(),
                read_terminator: read_terminator.as_deref(),
                min_interval_ms,
                ack_ok: ack_ok.as_deref(),
                ack_err: ack_err.as_deref(),
            };
            declare_instrument(&core, &args, json).await
        }
        Commands::ListInstruments => list_instruments(&core, json).await,
        Commands::RemoveInstrument { name } => remove_instrument(&core, &name, json).await,
        Commands::InstrumentExchange { name, line, query, timeout_ms, port_wait_ms } => {
            instrument_exchange(&core, &name, line, query, timeout_ms, port_wait_ms, json).await
        }
        Commands::SerialLog {
            project,
            signal,
            port_wait_ms,
            port,
            baud,
            duration_ms,
        } => serial_log(&config, &core, project.as_deref(), signal, port_wait_ms, port, baud, duration_ms, json).await,
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
        Commands::StudyStatus {
            study_id,
            follow,
            follow_timeout,
        } => {
            if follow {
                study_follow(&core, &study_id, follow_timeout, json).await
            } else {
                study_status(&core, &study_id, json).await
            }
        }
        Commands::StudyStreamData { study_id, name, raw, out } => {
            study_stream_data(&core, &study_id, &name, raw, out.as_deref(), json).await
        }
        Commands::ListStudyStreams { study_id } => {
            list_study_streams(&core, &study_id, json).await
        }
        Commands::StudyStreamLoad { study_id, name } => {
            study_stream_load(&core, &study_id, &name, json).await
        }
        Commands::BuildDevBench => build_dev_bench(&config, json).await,
        Commands::FlashDevBench { firmware_path, erase } => {
            flash_dev_bench(&config, &core, firmware_path, erase, json).await
        }
        Commands::BuildAndFlashDevBench { erase } => {
            build_and_flash_dev_bench(&config, &core, erase, json).await
        }
        Commands::ResetDevBench => reset_dev_bench(&config, &core, json).await,
        Commands::DevBenchHello => dev_bench_hello(&core, json).await,
        Commands::EnrollProbe { role, chip, probe_serial, name } => {
            enroll_probe(&core, role.as_deref().unwrap_or(""), &chip, probe_serial.as_deref(), name.as_deref(), json)
                .await
        }
        Commands::SetRoleBoard { role, board, chip } => {
            set_role_board(&core, &role, &board, &chip, json).await
        }
        Commands::UnenrollProbe { role, probe_serial } => match (role, probe_serial) {
            (_, Some(serial)) => unenroll_by_probe(&core, &serial, json).await,
            (Some(role), None) => unenroll_probe(&core, &role, json).await,
            (None, None) => error_result(json, "unenroll-probe needs --role or --probe-serial".to_string()),
        },
        Commands::Validate { role, probe_serial } => match crate::tools::board_of(&role, &probe_serial, None) {
            Ok(board) => validate(&core, board, json).await,
            Err(e) => error_result(json, format!("validate: {e}")),
        },
        Commands::Alerts { limit } => alerts(&core, limit, json).await,
        Commands::ListSerialPorts => serial_ports(&core, json).await,
        Commands::DeclareSignal {
            name,
            origin_role,
            origin_board,
            direction,
            route_kind,
            port_serial,
            interface,
            rx_pin,
            tx_pin,
            baud,
        } => declare_signal(&core, name, origin_role, origin_board, &direction, &route_kind, port_serial, interface, rx_pin, tx_pin, baud, json).await,
        Commands::SignalExchange { name, write, until, timeout_ms, no_newline, port_wait_ms, match_after_echo, clean } => {
            signal_exchange(&core, &name, &write, until.as_deref(), timeout_ms, port_wait_ms, match_after_echo, !no_newline, clean, json).await
        }
        Commands::CaptureStart { signal, max_s, max_bytes } => {
            let r = core.signal_capture_start(&signal, max_s, max_bytes).await;
            capture_result(&r, "capture-start", false, json)
        }
        Commands::CaptureRead { signal, after, limit, id } => {
            let r = core.signal_capture_read(&signal, after, limit, id.as_deref()).await;
            capture_result(&r, "capture-read", true, json)
        }
        Commands::CaptureStop { signal, after, limit, id } => {
            let r = core.signal_capture_stop(&signal, after, limit, id.as_deref()).await;
            capture_result(&r, "capture-stop", true, json)
        }
        Commands::BenchExchange { steps, listen, tail_ms, filters, snapshot, project, poll_ms } => {
            bench_exchange(&core, &config, steps, listen, tail_ms, filters, snapshot, project, poll_ms, json).await
        }
        Commands::MemRead { ranges, role, probe_serial } => match crate::tools::board_of(&role, &probe_serial, Some("dut")) {
            Ok(board) => mem_read(&core, board, &ranges, json).await,
            Err(e) => error_result(json, format!("mem-read: {e}")),
        },
        Commands::CoreState { role, probe_serial, halt, samples } => match crate::tools::board_of(&role, &probe_serial, Some("dut")) {
            Ok(board) => core_state(&core, board, halt, samples, json).await,
            Err(e) => error_result(json, format!("core-state: {e}")),
        },
        Commands::ListSignals => list_signals(&core, json).await,
        Commands::RemoveSignal { name } => remove_signal(&core, &name, json).await,
        Commands::DevBenchLink { serial, interface } => {
            dev_bench_link(&core, serial, interface, json).await
        }
        // Normally intercepted by `main` before a config is even looked for
        // (decision 52). Kept here so the subcommand surface is exhaustive
        // from either entry point, and so it behaves identically if a future
        // caller does route it through `run`.
        Commands::Versions => versions(json),
        Commands::Patches { action } => crate::patches::run(action, json),
        Commands::Stack { action } => crate::stack::run(action, json),
    }
}

/// `desk` names the open desk's firmware repo (`embarch-open-desk-proposal.md`).
fn lookup_project<'a>(config: &'a Config, name: &str) -> Result<&'a ProjectConfig, String> {
    let (own, name) = crate::open_desk::resolve(config, name).map_err(|e| format!("{e:#}"))?;
    own.unwrap_or(config).project(&name).map_err(|e| e.to_string())
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
/// stream and returns the process exit code, per this repo's `spec.md`: `0` on
/// success, `1` on any operation failure, distinguished only by the
/// message/JSON text, never a per-failure-kind code.
///
/// Every `--json` object this subcommand surface emits goes through here or
/// through [`study_follow`]'s two NDJSON sites, and all three serialize via
/// [`json_out`] — which is what makes decision 24's `schema_version`
/// unconditional rather than a thing each emitter remembers (decision 50).
fn finish(json: bool, success: bool, value: serde_json::Value, human: String) -> i32 {
    if json {
        println!("{}", json_out::pretty(value));
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

/// A failure that happened **before** any subcommand ran — an unreadable or
/// invalid config file, a bearer token that will not resolve.
///
/// `main` used to return these as an `anyhow::Error`, so the process printed
/// Rust's default `Err` rendering to stderr and `--json` emitted nothing at
/// all. That contradicted the `--json` contract in
/// `embarch-doc/embarch-api/interfaces/tools.md` — "on failure the error
/// goes into that same object on stdout ... so a script only has to check
/// the exit code, not which stream carried the result" — for exactly the
/// class of failure a script is most likely to hit on a fresh machine.
/// Found while building decision 50 and fixed with it.
pub fn startup_failure(json: bool, message: String) -> i32 {
    error_result(json, message)
}

/// `versions`: the version numbers compiled into **this binary**, read from
/// constants, touching neither the config file nor embarch-core.
///
/// The number that motivated it is `host_type_schema_version`
/// ([`embarch_study_designer::HOST_TYPE_SCHEMA_VERSION`]) — the study-designer
/// host type schema this binary submits studies under, and the one
/// `embarch-core-client::post_study` refuses a submit over when Core's served
/// copy differs. It was compiled in and readable from nowhere outside the
/// process, so `embarch doctor`'s schema-agreement check substituted the
/// `embarch` binary's own copy: exact when all three binaries came from one
/// archive, wrong for the hand-built mixed install this suite is developed
/// as (decision 52).
///
/// `schema_version` in the `--json` object is a *different* number — the
/// version of this crate's JSON shape, stamped on every object it emits
/// (decision 24) — which is why the payload key is spelled out in full
/// rather than shortened to `schema_version`.
///
/// Always succeeds: there is nothing here that can fail.
pub fn versions(json: bool) -> i32 {
    let api_version = env!("CARGO_PKG_VERSION");
    let host_type_schema_version = embarch_study_designer::HOST_TYPE_SCHEMA_VERSION;
    let human = format!(
        "embarch-api {}; study-designer host type schema v{host_type_schema_version}; \
         --json shape v{}; built in {}",
        crate::build_info::VERSION_LINE,
        json_out::SCHEMA_VERSION,
        crate::build_info::SOURCE_DIR
    );
    finish(
        json,
        true,
        serde_json::json!({
            "success": true,
            "api_version": api_version,
            "host_type_schema_version": host_type_schema_version,
            // Decision 93: which build this is, and the checkout it came
            // from, so `embarch doctor` check 18 can say it is behind.
            "commit": crate::build_info::COMMIT,
            "dirty": crate::build_info::dirty(),
            "source_dir": crate::build_info::SOURCE_DIR,
        }),
        human,
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
                "base_address": p.base_address.map(|a| format!("{a:#x}")),
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
                    "{} (discovery={}, chip={}, flash_format={}{}, source_path={}, serial_defaults={})",
                    p.name,
                    if p.is_zephyr_west() { "zephyr-west" } else { "static" },
                    p.chip.as_deref().unwrap_or("<resolved per call>"),
                    p.flash_format,
                    // Only shown when set: it's meaningful for a `bin`
                    // project and ignored everywhere else, so printing an
                    // empty column on every hex/elf project is noise.
                    p.base_address
                        .map(|a| format!(", base_address={a:#x}"))
                        .unwrap_or_default(),
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
            let human = json_out::human_render(&value);
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
            let human = format!("{human}\nembarch-api {}", crate::build_info::VERSION_LINE);

            finish(
                json,
                true,
                serde_json::json!({
                    "success": true, "status": status.status, "probes": probes,
                    "lease": status.lease, "last_images": status.last_images,
                    // Decision 93: the build answering, beside Core's own.
                    "api_build": crate::build_info::json(),
                }),
                human,
            )
        }
        Err(e) => error_result(
            json,
            format!(
                "embarch-core unreachable or returned an error: {e:#} (embarch-api {})",
                crate::build_info::VERSION_LINE
            ),
        ),
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
    // (`core_client.rs`'s own doc comment, decision 15).
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
                "erased": resp.erased,
                "firmware_path": path,
                "target": resolved.descriptor,
            }),
            format!("flashed '{project_name}' via chip {} ({}){}", resp.chip, path, erased_note(resp.erased.as_deref())),
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
                "erased": resp.erased,
                "firmware_path": core_firmware_path,
            }),
            format!(
                "build and flash succeeded for '{project_name}' via chip {} ({}){}",
                resp.chip, core_firmware_path, erased_note(resp.erased.as_deref())
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

async fn bootload(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    image_path: Option<&str>,
    json: bool,
) -> i32 {
    let project = match lookup_project(config, project_name) {
        Ok(p) => p,
        Err(e) => return error_result(json, e),
    };
    let report = crate::bootload::bootload(core, project, target.selection(), image_path).await;
    finish(json, report.success, report.value, report.human)
}

async fn build_and_bootload(
    config: &Config,
    core: &CoreClient,
    project_name: &str,
    target: &TargetSelection,
    json: bool,
) -> i32 {
    let project = match lookup_project(config, project_name) {
        Ok(p) => p,
        Err(e) => return error_result(json, e),
    };
    let build_locks = crate::build::BuildLocks::new();
    let report =
        crate::bootload::build_and_bootload(core, &build_locks, project, target.selection(), build_outcome_json).await;
    finish(json, report.success, report.value, report.human)
}

async fn declare_bootload_ports(
    core: &CoreClient,
    app: Option<&str>,
    app_interface: Option<u8>,
    bootloader: &str,
    bootloader_interface: Option<u8>,
    json: bool,
) -> i32 {
    let (app, bootloader) = match crate::bootload::parse_ports(app, app_interface, bootloader, bootloader_interface) {
        Ok(ports) => ports,
        Err(e) => return error_result(json, e),
    };
    match core.declare_bootload_ports(app.as_ref(), &bootloader).await {
        Ok(ports) => {
            let human = match &ports.app {
                Some(app) => format!("declared the DUT's bootloader as {} and its application as {app}", ports.bootloader),
                None => format!("declared the DUT's bootloader as {}, with no application port", ports.bootloader),
            };
            finish(json, true, serde_json::json!({ "success": true, "declared": ports }), human)
        }
        Err(e) => error_result(json, format!("declare-bootload-ports failed: {e:#}")),
    }
}

async fn show_bootload_ports(core: &CoreClient, json: bool) -> i32 {
    match core.bootload_ports().await {
        Ok(ports) => {
            let human = match &ports {
                Some(p) => format!(
                    "bootloader {}; application {}",
                    p.bootloader,
                    p.app.as_ref().map_or("not declared".to_string(), ToString::to_string)
                ),
                None => "no bootload ports declared".to_string(),
            };
            finish(json, true, serde_json::json!({ "success": true, "declared": ports }), human)
        }
        Err(e) => error_result(json, format!("show-bootload-ports failed: {e:#}")),
    }
}

async fn clear_bootload_ports(core: &CoreClient, json: bool) -> i32 {
    match core.clear_bootload_ports().await {
        Ok(removed) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "removed": removed }),
            if removed { "cleared the DUT's bootload ports".to_string() } else { "no bootload ports were declared".to_string() },
        ),
        Err(e) => error_result(json, format!("clear-bootload-ports failed: {e:#}")),
    }
}

async fn declare_instrument(core: &CoreClient, args: &crate::instrument::DeclareArgs<'_>, json: bool) -> i32 {
    let inst = match crate::instrument::build(args) {
        Ok(i) => i,
        Err(e) => return error_result(json, e),
    };
    match core.declare_instrument(&inst).await {
        Ok(stored) => {
            let human = format!("declared {}", crate::instrument::describe(&stored));
            finish(json, true, serde_json::json!({ "success": true, "declared": stored }), human)
        }
        Err(e) => error_result(json, format!("declare-instrument failed: {e:#}")),
    }
}

async fn list_instruments(core: &CoreClient, json: bool) -> i32 {
    match core.instruments().await {
        Ok(list) => {
            let human = if list.is_empty() {
                "no instruments declared".to_string()
            } else {
                list.iter().map(crate::instrument::describe).collect::<Vec<_>>().join("\n")
            };
            finish(json, true, serde_json::json!({ "success": true, "instruments": list }), human)
        }
        Err(e) => error_result(json, format!("list-instruments failed: {e:#}")),
    }
}

async fn remove_instrument(core: &CoreClient, name: &str, json: bool) -> i32 {
    match core.remove_instrument(name).await {
        Ok(removed) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "removed": removed }),
            if removed { format!("removed instrument {name}") } else { format!("no instrument was declared as {name}") },
        ),
        Err(e) => error_result(json, format!("remove-instrument failed: {e:#}")),
    }
}

async fn instrument_exchange(
    core: &CoreClient,
    name: &str,
    line: String,
    query: bool,
    timeout_ms: Option<u64>,
    port_wait_ms: Option<u64>,
    json: bool,
) -> i32 {
    let req = embarch_core_client::InstrumentExchange {
        line,
        query,
        timeout_ms: timeout_ms.unwrap_or(embarch_core_client::INSTRUMENT_DEFAULT_TIMEOUT_MS),
        port_wait_ms: port_wait_ms.unwrap_or(0),
    };
    match core.instrument_exchange(name, &req).await {
        Ok(r) => {
            // A query's reply, or a set's acknowledgement, is the answer; an unacknowledged set
            // or a query with no whole reply line exits 1 (decision 97).
            let ok = r.succeeded();
            let human = match (r.query, r.ack) {
                (true, _) if r.terminated => r.reply.clone(),
                (true, _) => format!("no whole reply line from {name} (got '{}')", r.reply),
                (false, None) => format!("sent '{}' to {name}", r.line),
                (false, Some(ack)) => format!("{name} answered '{}' ({ack:?})", r.reply),
            };
            finish(json, ok, serde_json::json!({ "success": ok, "reply": r }), human)
        }
        Err(e) => error_result(json, format!("instrument-exchange failed: {e:#}")),
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
                "erased": resp.erased,
                "firmware_path": path,
                "target": resolved.descriptor,
            }),
            format!("flashed dev_bench via chip {} ({}){}", resp.chip, path, erased_note(resp.erased.as_deref())),
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
                "erased": resp.erased,
                "firmware_path": core_firmware_path,
            }),
            format!(
                "build and flash succeeded for dev_bench via chip {} ({}){}",
                resp.chip, core_firmware_path, erased_note(resp.erased.as_deref())
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

async fn dev_bench_hello(core: &CoreClient, json: bool) -> i32 {
    match core.dev_bench_hello().await {
        Ok(info) => {
            let human = render_hello_ack(&info);
            finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "dev_bench_schema_version": info.schema_version,
                    "compatible": info.compatible,
                    "firmware_version": info.firmware_version,
                    "self_reported_hardware_id": info.self_reported_hardware_id,
                    "probe_hardware_id": info.probe_hardware_id,
                    "link_identity": info.link_identity,
                }),
                human,
            )
        }
        Err(e) => match e.downcast_ref::<DevBenchBusyError>() {
            Some(busy) => error_result(
                json,
                format!(
                    "dev_bench_hello refused: a study is already in flight and using the \
                     dev-bench link (409) — this is not a fault, retry once it finishes: {busy}"
                ),
            ),
            None => match e.downcast_ref::<DevBenchHandshakeError>() {
                Some(handshake) => error_result(
                    json,
                    format!(
                        "dev_bench_hello failed: the Hello/HelloAck handshake itself did not \
                         succeed (502) — check the bench, this is not a busy signal: {handshake}"
                    ),
                ),
                None => error_result(json, format!("dev_bench_hello failed: {e:#}")),
            },
        },
    }
}

async fn enroll_probe(
    core: &CoreClient,
    role: &str,
    chip: &str,
    probe_serial: Option<&str>,
    name: Option<&str>,
    json: bool,
) -> i32 {
    match core.enroll_probe(role, chip, probe_serial, name).await {
        Ok(resp) => {
            let moved = resp.moved_roles_note();
            let mut v = serde_json::json!({
                "success": true,
                "probe_serial": resp.probe_serial,
                "role": resp.role,
                "name": resp.name,
                "chip": resp.chip,
                "hardware_id": resp.hardware_id,
                "confirmed_at_utc_ms": resp.confirmed_at_utc_ms,
            });
            if !moved.is_empty() {
                v["displaced"] = serde_json::to_value(&resp.displaced).unwrap_or_default();
                v["vacated"] = serde_json::to_value(&resp.vacated).unwrap_or_default();
                v["vacated_kept"] = serde_json::to_value(resp.vacated_kept).unwrap_or_default();
                v["moved"] = moved.clone().into();
            }
            finish(
            json,
            true,
            v,
            format!(
                "enrolled probe {} as {} ({}, chip '{}', hardware_id {}){}",
                resp.probe_serial,
                if resp.role.is_empty() { "a bench board".to_string() } else { format!("role '{}'", resp.role) },
                if resp.name.is_empty() { "unnamed".to_string() } else { format!("board '{}'", resp.name) },
                resp.chip,
                resp.hardware_id,
                if moved.is_empty() { String::new() } else { format!("; {moved}") }
            ),
        )
        }
        Err(e) => error_result(json, format!("enroll-probe failed: {e:#}")),
    }
}

/// `PUT /probes/enrolled/{role}/board` — the half of a role that names a
/// board type rather than binding a probe. Prints the probe half too, so a
/// role that still needs one says so rather than looking finished.
async fn set_role_board(
    core: &CoreClient,
    role: &str,
    board: &str,
    chip: &str,
    json: bool,
) -> i32 {
    match core.set_role_board(role, board, chip).await {
        Ok(row) => {
            let probe = row.probe_serial.clone();
            finish(
                json,
                true,
                serde_json::json!({
                    "success": true,
                    "role": row.role,
                    "board": row.name,
                    "chip": row.chip,
                    "probe_serial": row.probe_serial,
                }),
                format!(
                    "role '{}' holds board type '{}' (chip {}), probe {}",
                    row.role,
                    row.name,
                    row.chip,
                    probe.as_deref().unwrap_or("(none yet — drop one to verify identity)")
                ),
            )
        }
        Err(e) => error_result(json, format!("set-role-board failed: {e:#}")),
    }
}

/// `DELETE /probes/enrolled/{role}`. Nothing enrolled under the role is
/// reported as `removed: false` and a zero exit, not as a failure — the
/// same shape `remove-signal` answers a `404` with, and for the same reason:
/// a caller clearing a row it believed existed learns it did not, without
/// having to tell that apart from a Core that could not be reached.
async fn unenroll_probe(core: &CoreClient, role: &str, json: bool) -> i32 {
    match core.unenroll_probe(role).await {
        Ok(removed) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "removed": removed, "role": role }),
            if removed {
                format!("unenrolled role '{role}' — that role is now empty")
            } else {
                format!("nothing was enrolled under role '{role}'")
            },
        ),
        Err(e) => error_result(json, format!("unenroll-probe failed: {e:#}")),
    }
}

/// `DELETE /probes/by-serial/{serial}` — retracts the board enrolled
/// through a probe, whatever role it holds; the only retraction a bench
/// board has (decision 84). Same `removed: false` shape as above.
async fn unenroll_by_probe(core: &CoreClient, serial: &str, json: bool) -> i32 {
    match core.unenroll_probe_by_serial(serial).await {
        Ok(removed) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "removed": removed, "probe_serial": serial }),
            if removed {
                format!("unenrolled the board on probe {serial}")
            } else {
                format!("nothing was enrolled through probe {serial}")
            },
        ),
        Err(e) => error_result(json, format!("unenroll-probe failed: {e:#}")),
    }
}

async fn validate(core: &CoreClient, board: embarch_core_client::Board<'_>, json: bool) -> i32 {
    match core.validate_board(board).await {
        Ok(resp) => {
            // `validated_at_utc_ms` is `None` from a Core older than
            // `tasks/core/026` (`embarch-api` decision 58) — never reported,
            // not "validated at 1970". `null` in the JSON surface is honest
            // for the same reason; the human line has to say it in words or
            // a reader sees a missing number and assumes a bug.
            let validated_at_word = match resp.validated_at_utc_ms {
                Some(ms) => ms.to_string(),
                None => "not reported by this Core".to_string(),
            };
            finish(
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
                    "validated_at_utc_ms": resp.validated_at_utc_ms,
                }),
                format!(
                    "ok: '{}' still matches hardware_id {} (validated_at_utc_ms {}, enrolled/confirmed_at_utc_ms {})",
                    resp.role, resp.hardware_id, validated_at_word, resp.confirmed_at_utc_ms
                ),
            )
        }
        Err(e) => match e.downcast_ref::<TopologyMismatchError>() {
            // Relay the mismatch and its fix_it_url as text — never opened
            // automatically (`embarch-topology validate`'s own CLI does the
            // same; `embarch-topology` decision 12). The URL is
            // a fixed `embarch-ui` Topology-tab link as of decision 19 there,
            // not a discovered one — still opaque from here either way.
            //
            // Branches on `kind` (`embarch-core` decision 59), not on
            // `reason`'s wording: a probe that is unavailable is not the
            // same condition as a live identity that disagrees with what's
            // recorded, and the two used to be rendered under the same
            // "topology mismatch" lead.
            //
            // Lead says "unavailable", not "not attached", and carries no
            // fixed instruction of its own (`embarch-api` decision 76):
            // `kind: "not_attached"` now also covers a probe that was found
            // but failed to open, power-check, attach, core-select, or
            // read a hardware ID from — none of which "plug it in" fixes,
            // and all of which `reason` already names correctly.
            Some(mismatch) if mismatch.is_not_attached() => error_result(
                json,
                format!(
                    "probe unavailable for role '{}' (probe {}, chip '{}'): {} (recorded \
                     hardware_id {}) — this is not a topology mismatch",
                    mismatch.role,
                    mismatch.probe_serial,
                    mismatch.chip,
                    mismatch.reason,
                    mismatch.recorded_hardware_id,
                ),
            ),
            // Branches on `is_unknown()`, not `is_not_attached()`'s inverse:
            // a Core old enough to predate `kind` entirely gets a third,
            // distinctly-worded lead rather than being folded into the
            // mismatch arm below — `embarch-api` decision 73.
            Some(mismatch) if mismatch.is_unknown() => error_result(
                json,
                format!(
                    "cannot classify role '{}' (probe {}, chip '{}'): this Core predates \
                     kind classification, so whether it's a mismatch or an unplugged probe \
                     can't be told apart here; treat as unresolved rather than re-enrolling \
                     (recorded hardware_id {}, live {:?}); {} (embarch-api decision 73)",
                    mismatch.role,
                    mismatch.probe_serial,
                    mismatch.chip,
                    mismatch.recorded_hardware_id,
                    mismatch.live_hardware_id,
                    mismatch.reason,
                ),
            ),
            Some(mismatch) => error_result(
                json,
                format!(
                    "topology mismatch for role '{}' (probe {}, chip '{}'): {} (recorded \
                     hardware_id {}, live {:?}){}",
                    mismatch.role,
                    mismatch.probe_serial,
                    mismatch.chip,
                    mismatch.reason,
                    mismatch.recorded_hardware_id,
                    mismatch.live_hardware_id,
                    match &mismatch.fix_it_url {
                        Some(url) => format!(" — fix it at {url}"),
                        None => String::new(),
                    }
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

async fn serial_ports(core: &CoreClient, json: bool) -> i32 {
    match core.list_serial_ports().await {
        Ok(ports) => {
            let human = if ports.is_empty() {
                "no serial ports enumerated on embarch-core's machine".to_string()
            } else {
                ports
                    .iter()
                    .map(|p| {
                        format!(
                            "{} (vendor_id={:?} product_id={:?} serial_number={:?} product={:?})",
                            p.port_name, p.vendor_id, p.product_id, p.serial_number, p.product
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
                    "ports": ports.iter().map(|p| serde_json::json!({
                        "port_name": p.port_name,
                        "detected_by": p.detected_by,
                        "vendor_id": p.vendor_id,
                        "product_id": p.product_id,
                        "serial_number": p.serial_number,
                        "product": p.product,
                        "interface": p.interface,
                    })).collect::<Vec<_>>(),
                }),
                human,
            )
        }
        Err(e) => error_result(json, format!("list-serial-ports failed: {e:#}")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn declare_signal(
    core: &CoreClient,
    name: String,
    origin_role: Option<String>,
    origin_board: Option<String>,
    direction: &str,
    route_kind: &str,
    port_serial: Option<String>,
    interface: Option<u8>,
    rx_pin: Option<String>,
    tx_pin: Option<String>,
    baud: Option<u32>,
    json: bool,
) -> i32 {
    let link = match crate::tools::parse_signal_link(name, origin_role, origin_board, direction, route_kind, port_serial, interface, rx_pin, tx_pin, baud) {
        Ok(l) => l,
        Err(e) => return error_result(json, e),
    };
    match core.declare_signal(&link).await {
        Ok(()) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "signal": link }),
            format!("declared signal '{}'", link.name),
        ),
        Err(e) => error_result(json, format!("declare-signal failed: {e:#}")),
    }
}

async fn list_signals(core: &CoreClient, json: bool) -> i32 {
    match core.list_signals().await {
        Ok(signals) => {
            let human = if signals.is_empty() {
                "no signals declared".to_string()
            } else {
                signals
                    .iter()
                    .map(|s| format!("{} (origin_role={}, direction={:?}, route={:?})", s.name, s.origin_role, s.direction, s.route))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            finish(
                json,
                true,
                serde_json::json!({ "success": true, "signals": signals }),
                human,
            )
        }
        Err(e) => error_result(json, format!("list-signals failed: {e:#}")),
    }
}

/// Prints a capture reply. Human mode: `text` alone on stdout (so it pipes),
/// one stderr line with the offsets and state, one per gap.
fn capture_result(r: &anyhow::Result<embarch_core_client::SignalCaptureResponse>, what: &str, text_only: bool, json: bool) -> i32 {
    match r {
        Ok(resp) => {
            if !json {
                eprintln!(
                    "{what}: id {} state {} next_offset {} total_bytes {} elapsed_ms {}",
                    resp.id, resp.state, resp.next_offset, resp.total_bytes, resp.elapsed_ms
                );
                for g in &resp.gaps {
                    match g.reopened_at_ms {
                        Some(r) => eprintln!("{what}: gap at offset {}: port lost {} ms, reopened {} ms", g.offset, g.lost_at_ms, r),
                        None => eprintln!("{what}: gap at offset {}: port lost {} ms, still away", g.offset, g.lost_at_ms),
                    }
                }
            }
            let human = if text_only { resp.text.clone() } else { format!("capture started: id {} state {}", resp.id, resp.state) };
            finish(json, true, serde_json::json!({ "success": true, "capture": resp }), human)
        }
        Err(e) => error_result(json, format!("{what} failed: {e:#}")),
    }
}

/// `bench-exchange`: the timeline on stdout, the per-step and capture facts
/// on stderr. A failed step exits 1 with the timeline so far still printed.
#[allow(clippy::too_many_arguments)]
async fn bench_exchange(
    core: &CoreClient,
    config: &Config,
    steps: Option<String>,
    listen: Vec<String>,
    tail_ms: Option<u64>,
    filters: Option<String>,
    snapshot: Option<String>,
    project: Option<String>,
    poll_ms: Option<u64>,
    json: bool,
) -> i32 {
    let parse = |what: &str, raw: &Option<String>| -> Result<Option<serde_json::Value>, String> {
        raw.as_deref().map(|r| serde_json::from_str(r).map_err(|e| format!("--{what} is not valid JSON: {e}"))).transpose()
    };
    let build = || -> Result<crate::bench_exchange::Request, String> {
        let steps = match parse("steps", &steps)? {
            Some(v) => serde_json::from_value(v).map_err(|e| format!("--steps: {e}"))?,
            None => Vec::new(),
        };
        let filters = match parse("filters", &filters)? {
            Some(v) => serde_json::from_value(v).map_err(|e| format!("--filters: {e}"))?,
            None => Default::default(),
        };
        Ok(crate::bench_exchange::Request { steps, listen: listen.clone(), tail_ms, filters, snapshot: snapshot.clone(), project: project.clone(), poll_ms })
    };
    let req = match build().and_then(|r| crate::bench_exchange::resolve(config, r)) {
        Ok(r) => r,
        Err(e) => return error_result(json, format!("bench-exchange: {e}")),
    };
    match crate::bench_exchange::run(core, req).await {
        Ok(v) => {
            let ok = v["success"].as_bool().unwrap_or(false);
            if !json {
                eprintln!("bench-exchange: times are {}", v["time_basis"].as_str().unwrap_or(""));
                if let Some(d) = v["dropped"].as_object().filter(|d| !d.is_empty()) {
                    for (sig, c) in d {
                        eprintln!("bench-exchange: {sig}: {} lines filtered out, {} over the cap", c["filtered"], c["capped"]);
                    }
                }
                if let Some(e) = v["error"].as_str() {
                    eprintln!("bench-exchange: {e}");
                }
            }
            let human = v["timeline"].as_str().unwrap_or_default().to_string();
            finish(json, ok, v, human)
        }
        Err(e) => error_result(json, format!("bench-exchange failed: {e}")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn signal_exchange(
    core: &CoreClient,
    name: &str,
    write: &str,
    until: Option<&str>,
    timeout_ms: Option<u64>,
    port_wait_ms: Option<u64>,
    match_after_echo: Option<bool>,
    append_newline: bool,
    clean: bool,
    json: bool,
) -> i32 {
    let line = crate::tools::exchange_line(write, append_newline);
    match core.signal_exchange_with(name, &line, until, timeout_ms, port_wait_ms, match_after_echo).await {
        Ok(resp) => {
            let text = if clean {
                clean_exchange(&resp.text, line.trim_end_matches('\n'), until.unwrap_or(""))
            } else {
                resp.text.clone()
            };
            let problem = exchange_problem(&resp, &line, until, timeout_ms);
            for n in resp.wire_notes(&line) {
                eprintln!("signal-exchange: {n}");
            }
            let code = finish(json, true, serde_json::json!({ "success": true, "exchange": resp }), text);
            match problem {
                Some((c, msg)) => {
                    eprintln!("{msg}");
                    c
                }
                None => code,
            }
        }
        Err(e) => error_result(json, format!("signal-exchange failed: {e:#}")),
    }
}

/// Exit code of `signal-exchange` when `--until` was given and never showed.
pub const EXIT_EXCHANGE_UNTIL_NOT_SEEN: i32 = 3;
/// Exit code of `signal-exchange` when zero bytes came back.
pub const EXIT_EXCHANGE_NO_REPLY: i32 = 4;
/// Exit code of `signal-exchange` when the reply was cut (`truncated`).
pub const EXIT_EXCHANGE_TRUNCATED: i32 = 5;

/// What is wrong with an exchange answer, as an exit code and the one stderr
/// line, or `None` for a complete reply. Zero bytes outranks the rest.
fn exchange_problem(
    resp: &embarch_core_client::SignalExchangeResponse,
    line: &str,
    until: Option<&str>,
    timeout_ms: Option<u64>,
) -> Option<(i32, String)> {
    let bytes = resp.text.len();
    let echo = if resp.echo_seen == Some(true) || resp.text.contains(line.trim_end_matches('\n')) { "seen" } else { "not seen" };
    if bytes == 0 {
        return Some((
            EXIT_EXCHANGE_NO_REPLY,
            format!("signal-exchange: no reply: 0 bytes read in {} ms (timeout {} ms)", resp.elapsed_ms, timeout_ms.map_or("default".to_string(), |t| t.to_string())),
        ));
    }
    if resp.port_lost == Some(true) {
        return Some((
            EXIT_EXCHANGE_TRUNCATED,
            format!("signal-exchange: port lost after {} ms: the command ran, the reply is incomplete: {bytes} bytes read, echo {echo}", resp.elapsed_ms),
        ));
    }
    if let Some(u) = until {
        if !resp.until_seen {
            return Some((
                EXIT_EXCHANGE_UNTIL_NOT_SEEN,
                format!("signal-exchange: no `{u}` within {} ms: {bytes} bytes read, echo {echo}", resp.elapsed_ms),
            ));
        }
    }
    if resp.truncated {
        return Some((
            EXIT_EXCHANGE_TRUNCATED,
            format!("signal-exchange: reply truncated after {} ms: {bytes} bytes read, echo {echo}", resp.elapsed_ms),
        ));
    }
    None
}

/// The command's output alone — the same transform as the atlas's
/// `live.clean()`: escape codes out (a cursor-right kept as spaces), `\r`
/// out, the echo and everything before it dropped, the closing prompt
/// dropped, one trailing newline.
pub fn clean_exchange(text: &str, command: &str, prompt: &str) -> String {
    let t = strip_escapes(text).replace('\r', "");
    let needle = format!("{command}\n");
    let mut t = match t.find(&needle) {
        Some(i) => t[i + needle.len()..].to_string(),
        None => t,
    };
    let p = prompt.trim_end();
    t.truncate(t.trim_end().len());
    if !p.is_empty() && t.ends_with(p) {
        t.truncate(t.len() - p.len());
    }
    let t = t.trim_end();
    format!("{}\n", t.trim_end_matches('\n'))
}

/// `ESC [ n C` becomes n spaces; any other `ESC [ params letter` goes.
fn strip_escapes(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'[' {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';' || b[j] == b'?') {
                j += 1;
            }
            if j < b.len() && b[j].is_ascii_alphabetic() {
                out.push_str(&text[start..i]);
                let params = &text[i + 2..j];
                if b[j] == b'C' && !params.is_empty() && params.bytes().all(|c| c.is_ascii_digit()) {
                    out.push_str(&" ".repeat(params.parse::<usize>().unwrap_or(0)));
                }
                i = j + 1;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[start..]);
    out
}

async fn mem_read(core: &CoreClient, board: embarch_core_client::Board<'_>, ranges: &[String], json: bool) -> i32 {
    let ranges = match ranges.iter().map(|r| crate::tools::parse_range(r)).collect::<Result<Vec<_>, _>>() {
        Ok(r) => r,
        Err(e) => return error_result(json, e),
    };
    match core.mem_read_board(board, &ranges).await {
        Ok(resp) => {
            let text = crate::tools::mem_read_text(&resp);
            finish(json, true, serde_json::json!({ "success": true, "read": resp }), text)
        }
        Err(e) => error_result(json, format!("mem-read failed: {e:#}")),
    }
}

async fn core_state(core: &CoreClient, board: embarch_core_client::Board<'_>, halt: bool, samples: u32, json: bool) -> i32 {
    match core.core_state_board(board, halt, samples).await {
        Ok(resp) => {
            let text = crate::tools::core_state_text(&resp);
            finish(json, true, serde_json::json!({ "success": true, "state": resp }), text)
        }
        Err(e) => error_result(json, format!("core-state failed: {e:#}")),
    }
}

async fn remove_signal(core: &CoreClient, name: &str, json: bool) -> i32 {
    match core.remove_signal(name).await {
        Ok(removed) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "removed": removed, "name": name }),
            if removed {
                format!("removed signal '{name}'")
            } else {
                format!("no signal named '{name}' was declared")
            },
        ),
        Err(e) => error_result(json, format!("remove-signal failed: {e:#}")),
    }
}

async fn dev_bench_link(core: &CoreClient, serial: Option<String>, interface: Option<u8>, json: bool) -> i32 {
    match core.set_dev_bench_link(serial.as_deref(), interface).await {
        Ok(()) => finish(
            json,
            true,
            serde_json::json!({ "success": true, "serial": serial, "interface": interface }),
            "dev-bench link updated".to_string(),
        ),
        Err(e) => error_result(json, format!("dev-bench-link failed: {e:#}")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn serial_log(
    config: &Config,
    core: &CoreClient,
    project_name: Option<&str>,
    signal: Option<String>,
    port_wait_ms: Option<u64>,
    port: Option<String>,
    baud: Option<u32>,
    duration_ms: Option<u64>,
    json: bool,
) -> i32 {
    let (result, label) = if let Some(signal) = signal {
        let r = core.serial_log_signal(&signal, baud, duration_ms.unwrap_or(2_000), port_wait_ms).await;
        (r, format!("signal '{signal}'"))
    } else {
        let project = match lookup_project(config, project_name.unwrap_or("")) {
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
        let r = core.serial_log(&port, baud, duration_ms.unwrap_or(2_000)).await;
        (r, format!("'{}'", project.name))
    };

    match result {
        Ok(resp) => {
            let mut human = if resp.lines.is_empty() {
                format!("serial-log on {}: no lines captured", resp.port)
            } else {
                format!("serial-log on {}:\n{}", resp.port, resp.lines.join("\n"))
            };
            if let Some(note) = resp.truncation_note() {
                human.push('\n');
                human.push_str(note);
            }
            let mut value = resp.to_json();
            value["success"] = true.into();
            finish(json, true, value, human)
        }
        Err(e) => error_result(json, format!("serial-log failed for {label}: {e:#}")),
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
            // decision 27: name the field and the limit when the refusal was
            // a capacity overflow. Re-parsing to a `Value` here costs nothing
            // on the path that matters, since we are already failing.
            let capacity = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .as_ref()
                .and_then(crate::capacity::explain);
            return error_result(
                json,
                match capacity {
                    Some(detail) => {
                        format!("study file {}: {detail}", study_file.display())
                    }
                    None => format!(
                        "study file {} did not match the expected Study schema: {e}",
                        study_file.display()
                    ),
                },
            );
        }
    };

    // `embarch-study-designer` decision 26: recompute and overwrite all three of a
    // study's seals unconditionally, regardless of whatever values
    // (including missing/zero ones) were in the submitted JSON.
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

/// `study-status --follow`: watch a study live.
///
/// # Why a flag on `study-status` rather than its own subcommand
///
/// "What is this study doing" is one question and it already has a name.
/// A separate `study-events` would split it in two and force a caller to
/// know which mechanism answers — which is precisely the thing this hides,
/// since a follow that loses its stream keeps answering by polling. The flag
/// also keeps the promise narrow: it says *watch until it is done*, not
/// *use SSE*, so the fallback is not a broken promise.
///
/// # Output
///
/// Human mode prints one line per item, prefixed by what it is. `--json`
/// prints **NDJSON** — one compact object per line, ending in a `summary`
/// line. This is the only subcommand whose `--json` is not a single pretty
/// object, and it has to be: a consumer of a live feed has to be able to
/// read a record before the last one has happened.
async fn study_follow(
    core: &CoreClient,
    study_id: &str,
    follow_timeout_secs: Option<u64>,
    json: bool,
) -> i32 {
    let options = FollowOptions {
        deadline: follow_timeout_secs.map(Duration::from_secs),
        ..FollowOptions::default()
    };

    let outcome = core
        .follow_study(study_id, &options, |item| {
            if json {
                // Compact, one line, unbuffered-ish: the point of NDJSON here
                // is that a reader sees a record when it happens — stamped
                // with `schema_version` like every other object, on *every*
                // line rather than only the `summary` one, because a reader
                // of a live feed has to know the shape from the first record.
                println!("{}", json_out::line(item.to_json()));
            } else {
                let rendered = render_follow_item(&item);
                println!("{rendered}");
            }
        })
        .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        // An unknown study_id (404 on the opening poll, stream open or not),
        // or no live stream *and* polling failed too (Core down). Everything
        // softer than that is already a `transport` item above.
        Err(e) => return error_result(json, format!("study-status --follow failed for '{study_id}': {e:#}")),
    };

    // `interrupted` ends the follow promptly but is not a success: exit 1,
    // `success: false`, `status: "interrupted"` in the summary. `completed`
    // and `failed` keep exiting 0 as before (the follow itself worked).
    let interrupted = outcome.terminal_status.as_deref() == Some("interrupted");
    let success = !outcome.timed_out && !interrupted;
    let lagged_line = if outcome.lagged_events > 0 {
        format!(
            "\n{} event(s) were dropped from this live feed by embarch-core \
             (it could not keep up); the study's own record is complete — \
             re-read it with study-status or list-study-streams",
            outcome.lagged_events
        )
    } else {
        String::new()
    };
    let human = match (&outcome.terminal_status, outcome.timed_out) {
        (Some(status), _) => format!(
            "study {study_id}: {status}{}{}{lagged_line}",
            if interrupted {
                " — embarch-core holds a partial record with no job behind it; it will not \
                 move on by itself"
            } else {
                ""
            },
            outcome
                .reason
                .as_deref()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        ),
        (None, true) => format!(
            "study {study_id}: still running after --follow-timeout {}s — stopped watching, \
             the study was not touched{lagged_line}",
            follow_timeout_secs.unwrap_or(0)
        ),
        (None, false) => format!("study {study_id}: stopped watching{lagged_line}"),
    };

    if json {
        let summary = serde_json::json!({
            "type": "summary",
            "success": success,
            "study_id": study_id,
            "status": outcome.terminal_status,
            "reason": outcome.reason,
            "lagged_events": outcome.lagged_events,
            "used_polling": outcome.used_polling,
            "timed_out": outcome.timed_out,
        });
        println!("{}", json_out::line(summary));
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

/// One [`FollowItem`] as a human-readable line.
///
/// Deliberately one line per item even for a `SampleBatch` of hundreds of
/// samples: a follow of a study with a power tap is a firehose, and the
/// useful human signal is that samples are arriving and roughly how fast,
/// not their values — those are what `study-stream-data` is for.
fn render_follow_item(item: &FollowItem) -> String {
    match item {
        FollowItem::Transport { mode, detail } => format!("[{}] {detail}", mode.as_str()),
        FollowItem::Lagged { missed } => format!(
            "[lagged] embarch-core dropped {missed} event(s) from this feed — not an error, \
             and the study's own record is unaffected"
        ),
        FollowItem::Unrecognized { event, reason, .. } => {
            format!("[?] frame '{event}': {reason}")
        }
        FollowItem::Polled {
            status,
            current_step,
            total_steps,
            reason,
        } => format!(
            "[poll] status={status} step={}/{}{}",
            current_step.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            total_steps.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            reason.as_deref().map(|r| format!(" — {r}")).unwrap_or_default(),
        ),
        FollowItem::Event(StudyEvent::StepCompleted {
            step_index, result, ..
        }) => {
            let outcome = match &result.outcome {
                Outcome::Pass => "PASS".to_string(),
                Outcome::Fail { reason } => format!("FAIL — {reason}"),
                Outcome::TimedOut => "TIMED OUT".to_string(),
            };
            format!("[step {step_index}] {}: {outcome}", result.step_name)
        }
        FollowItem::Event(StudyEvent::SampleBatch {
            stream_name,
            samples,
            ..
        }) => format!(
            "[samples] {stream_name}: {} sample(s){}",
            samples.len(),
            samples
                .last()
                .map(|s| format!(" (latest {} {:?})", s.value, s.unit))
                .unwrap_or_default()
        ),
        FollowItem::Event(StudyEvent::GattTranscript {
            step_index, entry, ..
        }) => format!(
            "[gatt step {step_index}] {} {} ({} byte payload)",
            entry.direction.as_str(),
            entry.kind.as_str(),
            entry.payload.len()
        ),
        FollowItem::Event(StudyEvent::OutpostRows {
            stream_name,
            frame_index,
            rows,
            header_seen,
            ..
        }) => format!(
            "[trace {stream_name}] frame {frame_index}: {} record(s){}",
            rows.len(),
            // A frame decoded before the capture's header arrived carries no
            // `us` and no names — said, rather than left for a reader to
            // notice two empty columns.
            if *header_seen { "" } else { " (no header yet: undated, unnamed)" }
        ),
        FollowItem::Event(StudyEvent::StreamText {
            stream_name,
            step_index,
            text,
            ..
        }) => {
            // Rendered on one line with escapes visible, because the chunk
            // is carried verbatim: it can end mid-line and mid-character,
            // and a renderer that printed it raw would splice unrelated
            // chunks into lines neither tap sent (`embarch-core` decision
            // 70). Line assembly is a consumer's job and this feed is not
            // the consumer that does it.
            format!(
                "[text step {step_index}] {stream_name}: {} byte(s) {:?}",
                text.len(),
                text
            )
        }
        FollowItem::Event(StudyEvent::StatusChanged { status, reason, .. }) => format!(
            "[status] {status}{}",
            reason.as_deref().map(|r| format!(" — {r}")).unwrap_or_default()
        ),
        FollowItem::Event(StudyEvent::StructChartValue {
            stream_name,
            field_name,
            value,
            ..
        }) => format!("[chart] {stream_name}: {field_name} = {value}"),
    }
}

/// Used by `study-stream-data`: write `bytes` to
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

/// The bracketed record-check tail of a stream line. A report with
/// `total == 0` checked nothing (empty capture, or no magic in it), so it says
/// "no records found" and never "0 of 0 verified", which reads as damage.
fn record_summary(r: &embarch_study_designer::RecordReport) -> String {
    let leading = if r.leading_bytes > 0 {
        format!(", {} leading bytes in no record", r.leading_bytes)
    } else {
        String::new()
    };
    if r.total == 0 {
        format!("  [no records found{leading}]")
    } else if r.all_verified() {
        format!("  [{} records, all verified]", r.total)
    } else {
        format!("  [{} of {} records verified{leading}; bad at {:?}]", r.verified, r.total, r.bad_offsets.as_slice())
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
                                "{} — {} bytes{}{}",
                                s.name,
                                s.bytes_written,
                                if s.truncated { "  [TRUNCATED — this capture is short]" } else { "" },
                                // Printed even when everything verified: "595
                                // of 598 records verified" is the sentence
                                // that decides whether a capture is usable,
                                // and "598 of 598" is what earns trusting it.
                                s.records.as_ref().map(record_summary).unwrap_or_default()
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

/// `GET /study/{id}/stream/{name}/load`'s CLI twin — the same call
/// `study_stream_load` (MCP) makes, so this and the tool are one
/// implementation with two front-ends (decisions 3/10; `suite/features.md`'s
/// `api-040 — CLI subcommands for every tool` row).
async fn study_stream_load(core: &CoreClient, study_id: &str, name: &str, json: bool) -> i32 {
    match core.get_study_load(study_id, name).await {
        Ok(answer) => {
            let summary = &answer.summary;
            let subjects_human = if summary.subjects.is_empty() {
                "no subjects".to_string()
            } else {
                summary
                    .subjects
                    .iter()
                    .map(|s| {
                        format!(
                            "  {} ({}) — share {:.1}%, {} measured span(s)",
                            s.label,
                            s.kind,
                            s.share * 100.0,
                            s.measured_spans
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
                    "name": name,
                    "rows": answer.rows,
                    "rows_dropped_by_cap": answer.rows_dropped_by_cap,
                    "row_cap": answer.row_cap,
                    "rows_unparsed": answer.rows_unparsed,
                    "summary": summary,
                }),
                format!(
                    "stream '{name}' of study '{study_id}': {} rows, window {} {unit}, gap {:.1}% \
                     ({} {unit}), records_lost {}\n{subjects_human}",
                    answer.rows,
                    summary.window_extent,
                    summary.gap_fraction * 100.0,
                    summary.gap_extent,
                    summary.records_lost,
                    unit = summary.unit,
                ),
            )
        }
        Err(e) => error_result(
            json,
            format!("study-stream-load failed for '{study_id}' stream '{name}': {e:#}"),
        ),
    }
}

/// `"; erased 0x..0x.., ..."` when Core blanked ranges, else nothing.
fn erased_note(erased: Option<&[String]>) -> String {
    match erased {
        Some(e) if !e.is_empty() => format!("; erased {}", e.join(", ")),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_zero_record_report_says_no_records_found_not_damage() {
        let mk = |total: u32, verified: u32, leading: u32| -> embarch_study_designer::RecordReport {
            serde_json::from_value(serde_json::json!({"total":total,"verified":verified,
                "bad_offsets":[],"leading_bytes":leading})).unwrap()
        };
        let empty = super::record_summary(&mk(0, 0, 0));
        assert_eq!(empty, "  [no records found]");
        assert!(super::record_summary(&mk(0, 0, 7)).contains("no records found, 7 leading bytes"));
        assert!(!empty.contains("of 0"));
        assert_eq!(super::record_summary(&mk(598, 598, 0)), "  [598 records, all verified]");
        assert!(super::record_summary(&mk(598, 595, 0)).contains("595 of 598 records verified"));
    }

    #[test]
    fn a_port_lost_reply_exits_five_and_a_silent_one_stays_four() {
        let mk = |text: &str, lost: bool| -> embarch_core_client::SignalExchangeResponse {
            serde_json::from_value(serde_json::json!({"signal":"s","port":"p","baud":1,"text":text,
                "until_seen":false,"truncated":false,"elapsed_ms":9,"written":true,"port_lost":lost})).unwrap()
        };
        let (c, m) = super::exchange_problem(&mk("ab", true), "x\n", Some("$ "), None).unwrap();
        assert_eq!(c, super::EXIT_EXCHANGE_TRUNCATED);
        assert!(m.contains("the command ran"), "{m}");
        let (c, _) = super::exchange_problem(&mk("", true), "x\n", None, None).unwrap();
        assert_eq!(c, super::EXIT_EXCHANGE_NO_REPLY);
    }

    #[test]
    fn clean_exchange_matches_atlas_clean() {
        use super::clean_exchange as c;
        // echo found, prompt at the end
        assert_eq!(c("help\r\nfoo\r\nbar\r\nuart:~$ ", "help", "uart:~$ "), "foo\nbar\n");
        // echo missing: nothing before it to drop
        assert_eq!(c("foo\r\nuart:~$ ", "help", "uart:~$ "), "foo\n");
        // escape codes out, cursor-right kept as spaces
        assert_eq!(c("\x1b[1;32mhelp\x1b[m\r\na\x1b[3Cb\r\n\x1b[1;32muart:~$ \x1b[m", "help", "uart:~$ "), "a   b\n");
        assert_eq!(c("help\nx\n\n", "help", ""), "x\n");
        assert_eq!(c("", "help", "uart:~$ "), "\n");
    }

    /// The two source files that make up this crate's machine-readable
    /// surface. Both are `main.rs` modules, so no integration test can
    /// reach them — the guard below reads them as text instead.
    const SURFACE_SOURCES: [(&str, &str); 2] = [
        ("src/cli.rs", include_str!("cli.rs")),
        ("src/tools.rs", include_str!("tools.rs")),
    ];

    /// Split so that this file's own source does not contain the literals
    /// it bans — otherwise the guard reports itself.
    fn banned_serializers() -> [String; 2] {
        [
            format!("serde_json::to_{}", "string_pretty"),
            format!("serde_json::to_{}(", "string"),
        ]
    }

    fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
        src.lines()
            .enumerate()
            .map(|(n, l)| (n + 1, l.trim()))
            .filter(|(_, l)| !l.starts_with("//"))
    }

    /// The structural half of decision 50.
    ///
    /// `finish` and `json_out` make `schema_version` unconditional for
    /// every emitter that routes through them. Nothing in Rust stops a new
    /// emitter from serializing its own object and `println!`-ing it
    /// instead — which is how the field stayed missing for the whole life
    /// of the crate the first time, while the docs told callers to read it.
    /// So the guard is a grep over this surface's own two source files,
    /// asserting the serializer lives in one module: `json_out` is the only
    /// place a `serde_json` value may be turned into text, and the only
    /// thing a bare `println!("{}", …)` may print.
    ///
    /// A false positive is cheap — route the new emitter through
    /// `json_out`, which is what it should do anyway. A false negative is a
    /// caller told to read a field that is not there.
    #[test]
    fn no_json_reaches_stdout_except_through_json_out() {
        for (name, src) in SURFACE_SOURCES {
            for banned in banned_serializers() {
                for (n, code) in code_lines(src) {
                    assert!(
                        !code.contains(&banned),
                        "{name}:{n} serializes a JSON value itself ({banned}). This \
                         surface has exactly one serializer, \
                         `embarch_api::json_out`, so decision 24's `schema_version` \
                         cannot be forgotten by a new emitter. Route it through \
                         `json_out::pretty` / `json_out::line`, or \
                         `json_out::human_render` if the text is for a person."
                    );
                }
            }

            for (n, code) in code_lines(src) {
                let printed = code
                    .strip_prefix(&format!("print{}!(\"{{}}\", ", "ln"))
                    .or_else(|| code.strip_prefix(&format!("print{}!(\"{{}}\", ", "")));
                if let Some(printed) = printed {
                    assert!(
                        printed.starts_with("json_out::"),
                        "{name}:{n} prints `{printed}` to stdout. Anything \
                         machine-readable on stdout is the `--json` surface and must \
                         go through `json_out`; anything human should be printed via \
                         an inline-captured binding so this guard can tell the two \
                         apart."
                    );
                }
            }
        }
    }

    /// A tripwire, not a truth: `tests/json_surface.rs` drives every
    /// subcommand through the real binary and asserts the stamp on whatever
    /// each one printed, and it cannot enumerate the subcommand enum itself
    /// (a binary crate has no importable surface — see `lib.rs`). So a new
    /// subcommand has to be added there by hand, and this is what says so
    /// rather than letting the new one go unchecked.
    #[test]
    fn every_subcommand_is_covered_by_the_json_surface_test() {
        let arms = include_str!("cli.rs")
            .lines()
            .filter(|l| l.trim_start().starts_with(&format!("Command{}::", "s")))
            .count();
        assert_eq!(
            arms, 51,
            "the subcommand surface changed. Add the new subcommand to \
             `tests/json_surface.rs`'s `EVERY_SUBCOMMAND` (so its `--json` output \
             is checked for `schema_version`) and update this count."
        );
    }
}
