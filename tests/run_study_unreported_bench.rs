//! `run-study`'s dev-bench gate against a Core that does not report
//! `dev_bench_version` — decision 105, driven through the real subprocess.
//!
//! A Core older than `embarch-core` decision 108 answers `GET
//! /dev-bench/hello` with the bench's build under `firmware_version` and no
//! `dev_bench_version`. The client reads that as `None` and does not fall
//! back to the old key. What this pins is what the gate then does with a
//! pinned `requires.dev_bench_version`: it always refuses, and
//! `allow_version_mismatch`, which lets a mismatch through, does not let this
//! through (the owner's call, 2026-11-11). It never treats the unknown version
//! as matching; a requirement of `any` is the one way to run against that Core.

mod support;

use std::path::{Path, PathBuf};


use serde_json::json;
use support::{Behavior, MockCore};

/// The version the study pins, and also the value the older Core sends under
/// the key this client must not read — so a client that fell back to it
/// would see a match and pass the gate.
const PINNED: &str = "g1a2b3c4";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let base = support::unique_dir("run-study-unreported-bench");
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A Core older than the rename: `/dev-bench/hello` carries the bench's build
/// as `firmware_version` only; `/status` matches this crate's host schema, so
/// `POST /study` is reached, and accepts.
async fn older_core() -> MockCore {
    MockCore::start(Behavior::Router {
        routes: vec![
            (
                "/dev-bench/hello".to_string(),
                Behavior::json_ok(json!({
                    "schema_version": 15,
                    "compatible": true,
                    "firmware_version": PINNED,
                    "self_reported_hardware_id": "AAAABBBB",
                    "link_identity": "undeclared",
                    "probe_hardware_id": "BBBBAAAA",
                })),
            ),
            (
                "/status".to_string(),
                Behavior::json_ok(json!({
                    "status": "ok",
                    "probes": [],
                    "study_designer_schema_version": embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
                })),
            ),
        ],
        otherwise: Box::new(Behavior::json_ok(json!({ "study_id": "s-unreported", "status": "accepted" }))),
    })
    .await
}

/// The config and a study whose `requires.dev_bench_version` is `bench`. No
/// `[[projects]]`: `--reflash none` builds nothing, and the run names a
/// project that does not exist so the ledger step skips without asking a desk.
fn write_inputs(dir: &Path, base_url: &str, bench: &str) -> (PathBuf, PathBuf) {
    let config_path = dir.join("api.toml");
    std::fs::write(
        &config_path,
        format!("[core]\nbase_url = \"{base_url}\"\ntoken = \"run-study-unreported-bench-test-token\"\n"),
    )
    .unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/self_test_study.json");
    let mut study: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    study["requires"]["dev_bench_version"] = json!(bench);
    let study_path = dir.join("study.json");
    std::fs::write(&study_path, study.to_string()).unwrap();
    (config_path, study_path)
}

fn run_study(dir: &Path, config: &Path, study: &Path, extra: &[&str]) -> serde_json::Value {
    let log_dir = dir.join("data");
    let output = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(config)
        .arg("--json")
        .arg("run-study")
        .arg("--study-file")
        .arg(study)
        .args(["--project", "no-such-project"])
        .args(extra)
        .env("XDG_DATA_HOME", &log_dir)
        .env("LOCALAPPDATA", &log_dir)
        .output()
        .expect("failed to run the binary for run-study");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not one JSON object: {e}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"))
}

fn posted_a_study(mock: &MockCore) -> bool {
    mock.requests().iter().any(|r| r.route() == ("POST".to_string(), "/study".to_string()))
}

// Multi-thread flavor: the test blocks its own task on `Command::output()`,
// which would starve the mock's accept loop on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreported_bench_version_is_refused_and_no_study_is_submitted() {
    let mock = older_core().await;
    let dir = TempDir::new();
    let (config, study) = write_inputs(dir.path(), mock.base_url(), PINNED);

    let value = run_study(dir.path(), &config, &study, &[]);

    assert_eq!(value["success"], json!(false), "{value}");
    let error = value.to_string();
    assert!(error.contains("dev-bench version unknown"), "{value}");
    assert!(error.contains("embarch-core decision 108"), "{value}");
    assert!(!posted_a_study(&mock), "the gate must refuse before POST /study");
}

/// The owner's call (2026-11-11): the mismatch override does not reach an
/// unreported version. It covers a version that was read and differs; here
/// nothing was read, so the run is refused exactly as without it.
#[tokio::test(flavor = "multi_thread")]
async fn the_mismatch_override_does_not_let_an_unreported_bench_version_through() {
    let mock = older_core().await;
    let dir = TempDir::new();
    let (config, study) = write_inputs(dir.path(), mock.base_url(), PINNED);

    let value = run_study(dir.path(), &config, &study, &["--allow-version-mismatch"]);

    assert_eq!(value["success"], json!(false), "{value}");
    let error = value.to_string();
    assert!(error.contains("dev-bench version unknown"), "{value}");
    assert!(error.contains("allow_version_mismatch does not apply"), "{value}");
    assert!(!posted_a_study(&mock), "the override must not reach POST /study");
}

/// The one way to run against a Core older than the rename: a bench
/// requirement of `any`, which the gate never opens the link to check.
#[tokio::test(flavor = "multi_thread")]
async fn a_bench_requirement_of_any_still_runs_against_a_core_older_than_the_rename() {
    let mock = older_core().await;
    let dir = TempDir::new();
    let (config, study) = write_inputs(dir.path(), mock.base_url(), "any");

    let value = run_study(dir.path(), &config, &study, &[]);

    assert_eq!(value["success"], json!(true), "{value}");
    assert!(posted_a_study(&mock), "an `any` study must reach Core");
    assert_eq!(value["dev_bench_version"], json!(null), "{value}");
}
