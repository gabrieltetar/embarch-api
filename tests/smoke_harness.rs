//! Decision 30's named smoke-harness tier — a throwaway Core instance plus a
//! synthetic fixture repo, re-running a fixed sequence of calls. It was
//! named in `decisions/tests.md` 30 and never written; `open.md` carried it
//! as a standing "named, unwritten" entry until this file.
//!
//! # What this tier covers, and how it differs from decision 46's tier
//!
//! Decision 46's `tests/*.rs` files pin `embarch_api::build` and
//! `CoreClient`'s HTTP behaviour directly — a function call or a raw request
//! against a hand-rolled mock, one invariant at a time. Nothing in that tier
//! ever runs the **compiled `embarch-api` binary itself**: not its arg
//! parsing, not `main.rs`'s config-then-client startup sequence, not
//! `cli::run`'s dispatch from a `Commands` variant to the same code the MCP
//! surface calls.
//!
//! This tier does exactly that, end to end, as a subprocess:
//!
//! 1. a **synthetic fixture repo** — a temp directory holding one
//!    `discovery = "static"` project whose `build_command` is a POSIX shell
//!    one-liner that writes a fake artifact, so a real build runs with no
//!    toolchain and no hardware;
//! 2. a **throwaway Core instance** — `tests/support::MockCore`, a real
//!    loopback HTTP server bound to an ephemeral port for the duration of
//!    this test and torn down with it. It is not the live, deployed Core
//!    this fleet's hard boundary forbids touching (the Windows service, a
//!    single long-lived instance with real probes behind it) — it is a
//!    fresh process this test starts and nothing else ever sees;
//! 3. a **fixed sequence of calls** through the actual binary
//!    (`env!("CARGO_BIN_EXE_embarch-api")`), each a real subprocess spawn
//!    with `--json`, checked for exit code and parsed JSON shape:
//!    `list-projects`, `list-targets`, `status`, `build`, `build` again (the
//!    same command run twice, the way a real session would, to catch
//!    anything that only breaks on a second invocation against a
//!    now-existing artifact).
//!
//! **No hardware, no probe, no live Core** — the mock never touches a
//! serial port or a debug probe, and every request it answers is one this
//! test wrote. **`#[cfg(unix)]`**, for the same reason decision 46's four
//! end-to-end tests are: the fixture's `build_command` is a POSIX shell
//! script. A Windows reader of a green run meets this in
//! `embarch-api/open.md`'s smoke-harness bullet and in this module's own
//! doc comment — nothing here runs on Windows, and nothing elsewhere in this
//! crate's test suite covers what it would have.

#![cfg(unix)]

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use support::{Behavior, MockCore};

/// One temp directory, removed on drop. Same pattern `src/main.rs`'s own
/// `#[cfg(test)] mod tests` already uses.
struct TempDir(PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tempdir(tag: &str) -> TempDir {
    let mut base = std::env::temp_dir();
    base.push(format!(
        "embarch-api-smoke-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    TempDir(base)
}

/// Writes a synthetic fixture repo: one `discovery = "static"` project
/// (`fixture-fw`) whose build command is a shell one-liner writing a fake
/// artifact — no toolchain, no `west`, no real firmware. `core.base_url`
/// points at the throwaway `MockCore` this test started.
fn write_fixture_config(dir: &std::path::Path, core_base_url: &str) -> PathBuf {
    let artifact = dir.join("build/fixture-fw.hex");
    let config_path = dir.join("embarch-api.toml");
    let config = format!(
        r#"
[core]
base_url = "{core_base_url}"

[[projects]]
name = "fixture-fw"
source_path = "{source_path}"
build_command = ["sh", "-c", "mkdir -p build && printf ':00000001FF\\n' > {artifact}"]
artifact_path = "{artifact}"
chip = "fixture-chip"
flash_format = "hex"
"#,
        core_base_url = core_base_url,
        source_path = dir.display(),
        artifact = artifact.display(),
    );
    fs::write(&config_path, config).expect("failed to write fixture embarch-api.toml");
    config_path
}

/// Runs the compiled `embarch-api` binary as a real subprocess, in `dir`,
/// against `config_path`, with `--json` plus whatever subcommand args are
/// given. Returns the parsed JSON body and the process exit code.
///
/// `spawn_blocking`, not a direct blocking call: the subprocess's own
/// `status` request has to reach the throwaway `MockCore`, whose accept
/// loop is a task on this *same* test's runtime — a bare blocking
/// `Command::output()` on a single-threaded runtime starves that task and
/// the request times out waiting for a server that is running, just
/// un-polled. Caught by this test itself before this fix: a 10s hang ending
/// in `operation timed out`, not a hang forever, which is what made it easy
/// to mistake for a real client bug on first read.
async fn run_json(config_path: &std::path::Path, dir: &std::path::Path, args: &[&str]) -> (i32, Value) {
    let bin = env!("CARGO_BIN_EXE_embarch-api");
    let config_path = config_path.to_path_buf();
    let dir = dir.to_path_buf();
    let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(bin)
            .arg("--config")
            .arg(&config_path)
            .arg("--json")
            .args(&owned_args)
            .current_dir(&dir)
            .output()
    })
    .await
    .expect("spawn_blocking task panicked")
    .unwrap_or_else(|e| panic!("failed to spawn embarch-api subprocess: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let value: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "embarch-api {args:?} did not print a single JSON object on stdout \
             (err={e}); stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(-1), value)
}

/// The fixed sequence decision 30 asks for: `list-projects`, `list-targets`,
/// `status`, `build`, `build` again — each a real subprocess call against
/// the same fixture repo and the same throwaway Core instance, run in this
/// order because a real session would.
#[tokio::test]
async fn smoke_sequence_against_a_throwaway_core_and_a_fixture_repo() {
    let mock = MockCore::start(Behavior::json_ok(serde_json::json!({
        "status": "ok",
        "probes": [],
    })))
    .await;

    let dir = tempdir("sequence");
    let config_path = write_fixture_config(dir.path(), mock.base_url());

    let (code, list_projects) = run_json(&config_path, dir.path(), &["list-projects"]).await;
    assert_eq!(code, 0, "list-projects exited nonzero: {list_projects}");
    assert_eq!(list_projects["success"], Value::Bool(true));
    let projects = list_projects["projects"].as_array().expect("projects is an array");
    assert_eq!(projects.len(), 1, "expected exactly the one fixture project: {list_projects}");
    assert_eq!(projects[0]["name"], "fixture-fw");

    let (code, list_targets) = run_json(&config_path, dir.path(), &["list-targets", "fixture-fw"]).await;
    assert_eq!(code, 0, "list-targets exited nonzero: {list_targets}");
    assert_eq!(list_targets["success"], Value::Bool(true));

    let (code, status) = run_json(&config_path, dir.path(), &["status"]).await;
    assert_eq!(code, 0, "status exited nonzero: {status}");
    assert_eq!(status["success"], Value::Bool(true));
    assert_eq!(status["status"], "ok");

    let (code, first_build) = run_json(&config_path, dir.path(), &["build", "fixture-fw"]).await;
    assert_eq!(code, 0, "first build exited nonzero: {first_build}");
    assert_eq!(first_build["success"], Value::Bool(true), "first build: {first_build}");
    assert_eq!(first_build["artifact_fresh"], Value::Bool(true), "first build: {first_build}");

    // Re-running the identical command is part of the fixed sequence, not a
    // repeat of the assertion above: a real session builds more than once
    // against the same config, and this is the case decision 46's own
    // criterion 6 (an untouched pre-existing artifact must not count as
    // fresh) exists to catch — here exercised through the real binary
    // rather than directly against `embarch_api::build`.
    let (code, second_build) = run_json(&config_path, dir.path(), &["build", "fixture-fw"]).await;
    assert_eq!(code, 0, "second build exited nonzero: {second_build}");
    assert_eq!(second_build["success"], Value::Bool(true), "second build: {second_build}");
    assert_eq!(
        second_build["artifact_fresh"], Value::Bool(true),
        "the fixture's build_command rewrites the artifact every run, so the \
         second build must still see it as fresh: {second_build}"
    );

    assert_eq!(mock.requests().len(), 1, "expected exactly one call to reach the throwaway Core (status)");
}

/// `status` against a Core that answers but reports itself unreachable in a
/// way `CoreClient` treats as an error (a non-2xx, plain-text body — the
/// exact shape decision 46 pins Core's real error responses as) must fail
/// the CLI call rather than the whole harness — this tier's fixed sequence
/// includes the unhappy path once, not only the all-green run above.
#[tokio::test]
async fn smoke_status_reports_a_core_error_rather_than_panicking() {
    let mock = MockCore::start(Behavior::plain_text_error(503, "Service Unavailable", "core is busy")).await;

    let dir = tempdir("status-error");
    let config_path = write_fixture_config(dir.path(), mock.base_url());

    let (code, status) = run_json(&config_path, dir.path(), &["status"]).await;
    assert_ne!(code, 0, "status against an erroring Core must exit nonzero: {status}");
    assert_eq!(status["success"], Value::Bool(false));
}
