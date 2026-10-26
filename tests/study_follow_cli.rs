//! `study-status --follow` on an unknown or interrupted study, against a mock
//! Core (`tasks/api/137`): neither may follow forever.

mod support;

use std::process::{Command, Output};
use std::time::Duration;

use serde_json::json;
use support::{sse_keep_alive, Behavior, MockCore, StreamTail};

async fn run(poll: Behavior, json_mode: bool) -> Output {
    let mock = MockCore::start(Behavior::Router {
        routes: vec![(
            "/events".to_string(),
            Behavior::EventStream {
                chunks: vec![sse_keep_alive()],
                gap: Duration::from_millis(1),
                then: StreamTail::Hold,
            },
        )],
        otherwise: Box::new(poll),
    })
    .await;
    let dir = std::env::temp_dir().join(format!(
        "embarch-api-follow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(
        &cfg,
        format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url()),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_embarch-api"));
    cmd.arg("--config").arg(&cfg);
    if json_mode {
        cmd.arg("--json");
    }
    let out = cmd
        .args(["study-status", "abc123", "--follow"])
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_study_exits_nonzero_naming_the_id() {
    let o = run(Behavior::plain_text_error(404, "Not Found", "nope"), false).await;
    assert_ne!(o.status.code().unwrap(), 0);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("abc123") && err.contains("no record"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_study_json_says_so() {
    let o = run(Behavior::plain_text_error(404, "Not Found", "nope"), true).await;
    assert_ne!(o.status.code().unwrap(), 0);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(all.contains("abc123") && all.contains("no record"), "{all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_study_exits_1_reporting_interrupted() {
    let body = json!({"status":"interrupted","current_step":1,"total_steps":3,"result":null,"reason":null});
    let o = run(Behavior::json_ok(body.clone()), false).await;
    assert_eq!(o.status.code().unwrap(), 1);
    assert!(String::from_utf8_lossy(&o.stderr).contains("interrupted"));
    let o = run(Behavior::json_ok(body), true).await;
    assert_eq!(o.status.code().unwrap(), 1);
    let out = String::from_utf8_lossy(&o.stdout);
    let summary: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(summary["status"], "interrupted");
    assert_eq!(summary["success"], false);
}
