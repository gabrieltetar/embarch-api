//! `dev-bench-hello --json`'s **success** path, against a mock Core.
//!
//! `tests/json_surface.rs` deliberately points every subcommand at a closed
//! loopback port, so `dev-bench-hello` there only ever takes its *failure*
//! path and never builds the success object this file exists to check.
//! That gap is exactly how `tasks/api/049`'s bug shipped: `cli.rs`'s success
//! object stamped `info.schema_version` — the dev-bench handshake's own
//! schema-compat number — under the literal key `"schema_version"`, and
//! `json_out::stamped()` unconditionally overwrites any existing
//! `schema_version` key with the envelope's own constant
//! (`json_out::SCHEMA_VERSION`, `1`) before printing. The failure-path test
//! passed unchanged whether or not that collision existed, because the
//! failure object never carried the colliding key in the first place.
//!
//! This drives the real subprocess (like `json_surface.rs`) but against a
//! `support::MockCore` answering `GET /dev-bench/hello` with a `200` whose
//! `schema_version` is deliberately **not** `json_out::SCHEMA_VERSION`
//! (`1`), so a reintroduced collision is visible as "the printed field
//! equals the envelope's `1`" rather than accidentally matching by
//! coincidence.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use support::{Behavior, MockCore};

use embarch_api::json_out::{SCHEMA_VERSION, SCHEMA_VERSION_FIELD};

/// The dev-bench handshake's own schema-compat number, as Core would report
/// it. Distinct from `json_out::SCHEMA_VERSION` (`1`) on purpose: if the
/// renamed field ever collided with the envelope stamp again, this test
/// would see `1` here instead of `7` and fail loudly rather than by luck.
const DEV_BENCH_SCHEMA_VERSION: u32 = 7;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "embarch-api-dev-bench-hello-success-{}-{}",
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

/// A config whose `[core]` points at the mock rather than a closed port.
/// No `[dev_bench]` table: `dev_bench_hello()` (`src/cli.rs`) calls
/// `core.dev_bench_hello()` directly and never resolves dev-bench config.
fn write_config(dir: &Path, base_url: &str) -> PathBuf {
    let config_path = dir.join("api.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[core]
base_url = "{base_url}"
token = "dev-bench-hello-success-test-token"
"#,
        ),
    )
    .unwrap();
    config_path
}

// Multi-thread flavor, not the default current-thread one: the assertion
// below blocks the test's own async task on `Command::output()`, a
// synchronous call that runs the whole subprocess to completion. On a
// current-thread runtime that starves `MockCore`'s accept loop of any
// chance to run at all, and the subprocess's request to it times out —
// which is exactly what happened the first time this test was written.
#[tokio::test(flavor = "multi_thread")]
async fn dev_bench_hello_json_carries_the_real_dev_bench_schema_version_not_the_envelopes() {
    let mock = MockCore::start(Behavior::json_ok(json!({
        "schema_version": DEV_BENCH_SCHEMA_VERSION,
        "compatible": true,
        "firmware_version": "1.2.3",
        "self_reported_hardware_id": "abc123",
        "link_identity": "match",
        "probe_hardware_id": "abc123",
    })))
    .await;

    let dir = TempDir::new();
    let config_path = write_config(dir.path(), mock.base_url());
    let log_dir = dir.path().join("data");

    let output = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .arg("dev-bench-hello")
        .env("XDG_DATA_HOME", &log_dir)
        .env("LOCALAPPDATA", &log_dir)
        .output()
        .expect("failed to run the binary for dev-bench-hello");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let values: Vec<serde_json::Value> = serde_json::Deserializer::from_str(&stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("stdout was not a JSON value sequence: {e}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"));

    assert_eq!(
        values.len(),
        1,
        "expected exactly one JSON object on stdout\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let value = &values[0];
    assert_eq!(
        value["success"],
        json!(true),
        "dev-bench-hello did not take its success path against the mock: {value}\n--- stderr ---\n{stderr}"
    );

    // The bug this test exists to catch: `stamped()` overwrites any
    // existing `schema_version` key with the envelope's own constant, so a
    // reintroduced collision would print `1` here instead of `7`.
    assert_eq!(
        value["dev_bench_schema_version"],
        json!(DEV_BENCH_SCHEMA_VERSION),
        "dev-bench-hello's success object did not carry the dev-bench handshake's own \
         schema_version under its renamed key: {value}"
    );

    // The envelope stamp is still present, and still its own constant — the
    // two numbers must never be read off the same key.
    assert_eq!(value[SCHEMA_VERSION_FIELD], json!(SCHEMA_VERSION));
    assert_ne!(
        value[SCHEMA_VERSION_FIELD], value["dev_bench_schema_version"],
        "the envelope's schema_version and the dev-bench handshake's schema_version happened \
         to collide in this test's own fixtures, which would hide a real collision bug — pick \
         fixture constants that differ"
    );
}
