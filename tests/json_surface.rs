//! Every `--json` object the real binary prints carries `schema_version`.
//!
//! The behavioural half of `embarch-doc/embarch-api/decisions.md` decision
//! 50. `json_out`'s own unit tests pin the stamper; `cli.rs`'s
//! `no_json_reaches_stdout_except_through_json_out` pins that nothing
//! bypasses it. Neither of those runs the binary, and the failure this task
//! exists to close was not a broken stamper — it was a documented field
//! that no code path ever wrote. So this drives **every subcommand** as a
//! subprocess and reads what actually landed on stdout.
//!
//! # Why no embarch-core, and why that is enough
//!
//! Core is addressed at a closed loopback port, so every Core-backed
//! subcommand takes its failure path. That is deliberate rather than a
//! compromise: the failure object is the one a scripted caller was told to
//! read `error_kind` off of, it is the shape that reaches a caller on a
//! fresh machine, and it is the one an emitter is most likely to build by
//! hand. Two success shapes are covered too — `list-projects` and
//! `list-targets`, the only two that need nothing but config — and
//! `list-targets`' is the awkward one, since it merges `success` into a
//! value `resolve` built rather than constructing a fresh object.
//!
//! Not covered here: an NDJSON *event* line, which needs a Core emitting
//! SSE frames. `study-status --follow` against a dead Core still exercises
//! the path (its `transport` items and its error object), and
//! `json_out::line` is unit-tested; a live event line is pinned in
//! `tests/study_events_sse.rs`'s territory, against the mock, at the client
//! layer.

use embarch_api::json_out::{SCHEMA_VERSION, SCHEMA_VERSION_FIELD};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every subcommand, with arguments chosen so that **nothing executes**: an
/// unknown project name or study id, a closed Core port, no `[dev_bench]`
/// table. A real build or flash has no place in a host-side test.
///
/// Keep this exhaustive. `cli.rs`'s
/// `every_subcommand_is_covered_by_the_json_surface_test` fails when the
/// subcommand surface grows and this list has not.
const EVERY_SUBCOMMAND: &[&[&str]] = &[
    &["list-projects"],
    &["list-targets", "demo"],
    &["status"],
    &["build", "no-such-project"],
    &["flash", "no-such-project"],
    &["build-and-flash", "no-such-project"],
    &["reset", "no-such-project"],
    &["serial-log", "no-such-project"],
    &["run-study", "--study-file", "no-such-study.json"],
    &["study-status", "no-such-study"],
    &["study-status", "no-such-study", "--follow", "--follow-timeout", "1"],
    &["study-power-data", "no-such-study"],
    &["study-waveform-data", "no-such-study"],
    &["study-gatt-data", "no-such-study"],
    &["study-stream-data", "no-such-study", "--name", "some-tap"],
    &["list-study-streams", "no-such-study"],
    &["build-dev-bench"],
    &["flash-dev-bench"],
    &["build-and-flash-dev-bench"],
    &["reset-dev-bench"],
    &["enroll-probe", "--role", "some-role", "--chip", "nRF54L15"],
    &["validate", "--role", "some-role"],
    &["alerts"],
];

struct TempDir(PathBuf);

impl TempDir {
    /// Minimal tempdir, matching the pattern `config.rs`/`zephyr.rs`/
    /// `main.rs` already use rather than adding a `tempfile` dependency for
    /// one directory.
    fn new() -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "embarch-api-json-surface-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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

/// A config that loads, names one static project, and points Core at a port
/// nothing listens on.
///
/// Port 1 rather than a random high port: it is privileged, so no test
/// process on the machine can be holding it, and a loopback connection to a
/// closed port is refused immediately rather than timing out. `base_url` is
/// explicit rather than `"auto"` for the same reason — `"auto"` would probe
/// loopback, the WSL2 host gateway and `host`, and could find a **real**
/// Core on the developer's own machine.
fn write_config(dir: &Path) -> PathBuf {
    let source_path = dir.join("demo-src");
    std::fs::create_dir_all(&source_path).unwrap();
    let config_path = dir.join("api.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[core]
base_url = "http://127.0.0.1:1"
token = "json-surface-test-token"

[[projects]]
name = "demo"
source_path = "{source}"
build_command = ["true"]
artifact_path = "build/zephyr/zephyr.hex"
chip = "nRF54L15"
flash_format = "hex"

[[projects.targets]]
name = "demo-board"
chip = "nRF54L15"
"#,
            source = source_path.display().to_string().replace('\\', "\\\\"),
        ),
    )
    .unwrap();
    config_path
}

fn run(config_path: &Path, log_dir: &Path, args: &[&str]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_embarch-api"));
    command
        .arg("--config")
        .arg(config_path)
        .arg("--json")
        .args(args)
        // Keep this run's rolling logfile out of the developer's real
        // per-user data dir (`embarch_core_client::user_dirs`).
        .env("XDG_DATA_HOME", log_dir)
        .env("LOCALAPPDATA", log_dir);
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("failed to run the binary for {args:?}: {e}"));
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Parses stdout as a sequence of JSON values, which covers both `--json`
/// shapes at once: one pretty object, or NDJSON records (and, for a
/// `--follow` that loses its stream, records *followed by* an error
/// object).
fn json_values(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::Deserializer::from_str(stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("stdout was not a JSON value sequence: {e}\n--- stdout ---\n{stdout}"))
}

#[test]
fn every_subcommands_json_output_carries_the_schema_version() {
    let dir = TempDir::new();
    let config_path = write_config(dir.path());
    let log_dir = dir.path().join("data");

    for args in EVERY_SUBCOMMAND {
        let (stdout, stderr) = run(&config_path, &log_dir, args);
        let values = json_values(&stdout);

        assert!(
            !values.is_empty(),
            "`{}` printed no JSON object at all under --json. Every failure is \
             promised as an object on stdout (interfaces/tools.md), so a script \
             only has to check the exit code.\n--- stderr ---\n{stderr}",
            args.join(" ")
        );

        for value in &values {
            assert!(
                value.is_object(),
                "`{}` printed a non-object under --json: {value}",
                args.join(" ")
            );
            assert_eq!(
                value.get(SCHEMA_VERSION_FIELD),
                Some(&serde_json::json!(SCHEMA_VERSION)),
                "`{}` printed a --json object without `{SCHEMA_VERSION_FIELD}`: {value}",
                args.join(" ")
            );
        }
    }
}

#[test]
fn a_startup_failure_is_a_json_object_on_stdout_not_a_rust_error_on_stderr() {
    // The gap found while building decision 50: a config that will not load
    // used to escape as `main`'s `Err`, so `--json` printed nothing and a
    // script checking stdout saw an empty result rather than a failure.
    let dir = TempDir::new();
    let log_dir = dir.path().join("data");
    let bad_config = dir.path().join("broken.toml");
    std::fs::write(&bad_config, "this is not valid toml = = =\n").unwrap();

    let (stdout, stderr) = run(&bad_config, &log_dir, &["list-projects"]);
    let values = json_values(&stdout);
    assert_eq!(
        values.len(),
        1,
        "expected exactly one failure object on stdout\n--- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
    assert_eq!(values[0]["success"], serde_json::json!(false));
    assert_eq!(
        values[0][SCHEMA_VERSION_FIELD],
        serde_json::json!(SCHEMA_VERSION)
    );
    assert!(
        values[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("failed to parse config file")),
        "the failure object should name what went wrong: {}",
        values[0]
    );
}

#[test]
fn a_missing_config_is_also_a_json_object() {
    let dir = TempDir::new();
    let log_dir = dir.path().join("data");
    let (stdout, stderr) = run(&dir.path().join("absent.toml"), &log_dir, &["status"]);
    let values = json_values(&stdout);
    assert_eq!(
        values.len(),
        1,
        "expected exactly one failure object on stdout\n--- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
    assert_eq!(values[0]["success"], serde_json::json!(false));
    assert_eq!(
        values[0][SCHEMA_VERSION_FIELD],
        serde_json::json!(SCHEMA_VERSION)
    );
}
