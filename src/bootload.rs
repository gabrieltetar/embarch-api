//! `bootload` and `build_and_bootload`: a signed image to the DUT's MCUboot
//! serial-recovery bootloader, through Core's `POST /bootload` (decisions
//! 80, 81; `embarch-core` decision 77). The MCP tools and the CLI
//! subcommands are two front-ends over the functions here, so the flow is
//! written once.
//!
//! Neither touches a probe or a chip: target resolution stops at the build
//! plan (`resolve::resolve_build`), because the bootloader is reached by the
//! DUT's USB identity, which Core holds, not by anything this crate resolves.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use embarch_core_client::{BootloadOptions, BootloadResponse, CoreClient, UsbPortId};
use embarch_firmware_build::zephyr;

use crate::build::{artifact_is_fresh, BuildLocks, BuildOutcome, BuildPlan};
use crate::config::ProjectConfig;
use crate::resolve::{self, Selection};

/// What both front-ends print: the JSON object, a line for a person, and
/// whether it was a success.
pub struct Report {
    pub success: bool,
    pub value: serde_json::Value,
    pub human: String,
}

impl Report {
    fn failed(message: String) -> Report {
        Report { success: false, value: serde_json::json!({ "success": false, "error": message }), human: message }
    }
}

fn options(project: &ProjectConfig) -> BootloadOptions {
    let declared = project.bootload.clone().unwrap_or_default();
    BootloadOptions {
        entry_command: declared.entry_command,
        entry_line_ending: declared.entry_line_ending,
        buffer_size: declared.buffer_size,
    }
}

fn signed_image(project: &ProjectConfig, plan: &BuildPlan) -> anyhow::Result<PathBuf> {
    let name = project.bootload.as_ref().map_or(crate::config::DEFAULT_BOOTLOAD_ARTIFACT, |b| b.artifact());
    zephyr::signed_image_path(&plan.artifact_path, name)
}

/// The result's own fields, plus a `warning` when the application did not
/// come back — a real outcome Core reports rather than raises, and the one an
/// agent reading only `success` would otherwise miss.
fn result_json(project: &str, image: &Path, target: serde_json::Value, r: &BootloadResponse) -> (serde_json::Value, String) {
    let mut value = serde_json::json!({
        "success": true,
        "image_path": image.display().to_string(),
        "target": target,
        "bytes": r.bytes,
        "requests": r.requests,
        "duration_ms": r.duration_ms,
        "upload_ms": r.upload_ms,
        "entered_via": r.entered_via,
        "bootloader_port": r.bootloader_port,
        "app_reappeared": r.app_reappeared,
    });
    let came_back = match r.app_reappeared {
        Some(true) => "the application came back",
        Some(false) => {
            value["warning"] = serde_json::Value::String(
                "the bootloader took the image, and the application did not enumerate again within \
                 Core's timeout (a placeholder, not yet measured)"
                    .into(),
            );
            "the application did NOT come back"
        }
        None => "no application port is declared, so its return was not watched",
    };
    let human = format!(
        "bootloaded '{project}' ({} bytes, {} requests, {} ms) through {}, entered via {}; {came_back}",
        r.bytes, r.requests, r.duration_ms, r.bootloader_port, r.entered_via
    );
    (value, human)
}

/// `bootload`: the already-built signed image, or `image_path` when given.
pub async fn bootload(core: &CoreClient, project: &ProjectConfig, selection: Selection<'_>, image_path: Option<&str>) -> Report {
    let (image, target) = match image_path {
        // Bypasses resolution entirely: there is no chip to resolve, so
        // nothing about the target is needed once the file is named.
        Some(path) => (PathBuf::from(path), serde_json::json!({ "project": project.name })),
        None => {
            let build = match resolve::resolve_build(project, selection) {
                Ok(b) => b,
                Err(e) => return Report::failed(format!("{e:#}")),
            };
            match signed_image(project, &build.plan) {
                Ok(path) => (path, build.descriptor),
                Err(e) => return Report::failed(format!("{e:#}")),
            }
        }
    };
    if !image.is_file() {
        return Report::failed(format!(
            "no signed image at {} — build it first (build_and_bootload), or pass image_path",
            image.display()
        ));
    }
    match core.bootload(&image, &options(project)).await {
        Ok(r) => {
            let (value, human) = result_json(&project.name, &image, target, &r);
            Report { success: true, value, human }
        }
        Err(e) => Report::failed(format!("bootload failed for '{}': {e:#}", project.name)),
    }
}

/// `build_and_bootload`: build, then bootload **only a signed image this
/// build wrote** — never one left over from an earlier build.
pub async fn build_and_bootload(
    core: &CoreClient,
    locks: &BuildLocks,
    project: &ProjectConfig,
    selection: Selection<'_>,
    build_json: impl Fn(&BuildOutcome) -> serde_json::Value,
) -> Report {
    let build = match resolve::resolve_build(project, selection) {
        Ok(b) => b,
        Err(e) => return Report::failed(format!("{e:#}")),
    };
    let build_start = SystemTime::now();
    let outcome = match locks.run_build(&build.plan).await {
        Ok(outcome) => outcome,
        Err(e) => return Report::failed(format!("failed to run build for '{}': {e:#}", project.name)),
    };
    let mut built = build_json(&outcome);
    built["target"] = build.descriptor.clone();

    let refuse = |mut built: serde_json::Value, reason: String| {
        built["success"] = serde_json::Value::Bool(false);
        built["reason"] = serde_json::Value::String(reason.clone());
        Report { success: false, value: built, human: format!("{reason} for '{}' — refusing to bootload", project.name) }
    };
    if !outcome.build_succeeded() {
        return refuse(built, outcome.failure_reason());
    }
    // Located after the build, because a first sysbuild build is what writes
    // the `domains.yaml` that says where the image is. Freshness is the
    // image's own mtime against the build's start: the flash artifact beside
    // it proves nothing about whether the signing step ran.
    let image = match signed_image(project, &build.plan) {
        Ok(path) => path,
        Err(e) => return refuse(built, format!("{e:#}")),
    };
    if !image.is_file() || !artifact_is_fresh(&image, true, build_start) {
        return refuse(
            built,
            format!(
                "the build succeeded but wrote no fresh signed image at {}; is it configured to sign \
                 an MCUboot image?",
                image.display()
            ),
        );
    }

    match core.bootload(&image, &options(project)).await {
        Ok(r) => {
            let (mut value, human) = result_json(&project.name, &image, build.descriptor, &r);
            value["build"] = built;
            Report { success: true, value, human: format!("build succeeded; {human}") }
        }
        Err(e) => Report::failed(format!("build succeeded but bootload failed for '{}': {e:#}", project.name)),
    }
}

/// The two identities from their `VID:PID[:SERIAL]` spelling plus an
/// optional interface each — the shape both front-ends take them in.
pub fn parse_ports(
    app: Option<&str>,
    app_interface: Option<u8>,
    bootloader: &str,
    bootloader_interface: Option<u8>,
) -> Result<(Option<UsbPortId>, UsbPortId), String> {
    if app.is_none() && app_interface.is_some() {
        return Err("app_interface narrows the application's identity, and no app was given".to_string());
    }
    let with_interface = |spelled: &str, interface: Option<u8>| -> Result<UsbPortId, String> {
        let mut id: UsbPortId = spelled.parse()?;
        id.interface = interface;
        Ok(id)
    };
    let app = app.map(|a| with_interface(a, app_interface)).transpose()?;
    Ok((app, with_interface(bootloader, bootloader_interface)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_parse_with_their_interfaces() {
        let (app, boot) = parse_ports(Some("2fe3:0004"), Some(0), "0x2fe3:0x000c:SN1", Some(2)).unwrap();
        assert_eq!(app.unwrap().interface, Some(0));
        assert_eq!(boot.serial.as_deref(), Some("SN1"));
        assert_eq!(boot.interface, Some(2));
        assert_eq!(parse_ports(None, None, "2fe3:000c", None).unwrap().0, None);
        assert!(parse_ports(None, Some(1), "2fe3:000c", None).is_err());
        assert!(parse_ports(None, None, "2fe3", None).is_err());
    }
}
