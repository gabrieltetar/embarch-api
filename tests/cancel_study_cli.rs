//! `cancel-study` against a mock Core's `POST /study/{id}/cancel` (EMB-487,
//! `tasks/api/198`): Core's 202 reported, its refusals reported as its own text.
//! `tool_subcommand_parity` holds the MCP `cancel_study` to the same name.

mod support;

use serde_json::{json, Value};
use support::{Behavior, MockCore};

fn run(base_url: &str, args: &[&str]) -> (std::process::Output, Value) {
    let dir = support::unique_dir("cancel-study");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{base_url}\"\ntoken = \"t\"\n")).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let v = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out, v)
}

#[tokio::test]
async fn a_running_study_is_cancelled_through_the_cancel_route() {
    let reply = Behavior::Reply {
        status: 202,
        reason: "Accepted".into(),
        content_type: "application/json".into(),
        body: json!({"study_id": "s-1", "status": "cancelling"}).to_string(),
    };
    let mock = MockCore::start(reply).await;
    let base = mock.base_url().to_string();
    let (out, v) = tokio::task::spawn_blocking(move || run(&base, &["--json", "cancel-study", "s-1"])).await.unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(v["success"], true);
    assert_eq!(v["cancel"]["status"], "cancelling");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].target, "/study/s-1/cancel");
}

#[tokio::test]
async fn an_ended_or_unknown_study_reports_cores_refusal() {
    for (status, reason, body) in [
        (409, "Conflict", "study 's-2' is not running (status: completed); nothing to cancel"),
        (404, "Not Found", "no study 's-3' is known to this Core"),
    ] {
        let mock = MockCore::start(Behavior::plain_text_error(status, reason, body)).await;
        let base = mock.base_url().to_string();
        let (out, v) = tokio::task::spawn_blocking(move || run(&base, &["--json", "cancel-study", "s-2"])).await.unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert_eq!(v["success"], false);
        let err = v["error"].to_string();
        assert!(err.contains(body), "{err}");
        assert!(err.contains(&status.to_string()), "{err}");
    }
}
