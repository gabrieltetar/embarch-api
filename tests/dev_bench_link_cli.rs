//! `dev-bench-link` carries `control_port` and `clear_control_port` to Core's
//! `POST /dev-bench/link` (`embarch-core` decision 124), against a mock Core.
//! The MCP tool calls the same `dev_bench_link::apply`, so this is also its
//! wire test; `tool_subcommand_parity` holds the two names together.

mod support;

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

async fn run(core_status: u16, args: &[&str]) -> (std::process::Output, Vec<RecordedRequest>) {
    let mock = MockCore::start(Behavior::Reply {
        status: core_status,
        reason: if core_status == 204 { "No Content" } else { "Bad Request" }.into(),
        content_type: "text/plain".into(),
        body: String::new(),
    })
    .await;
    let dir = support::unique_dir("devbenchlink");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .arg("--json")
        .arg("dev-bench-link")
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out, mock.requests())
}

fn posted(reqs: &[RecordedRequest]) -> Value {
    let r = reqs.iter().find(|r| r.method == "POST").unwrap_or_else(|| panic!("no POST: {reqs:?}"));
    assert_eq!(r.path(), "/dev-bench/link");
    serde_json::from_slice(&r.body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_control_port_goes_to_core_as_numeric_vid_and_pid() {
    let (out, reqs) = run(
        204,
        &["--control-vid", "0x2FE3", "--control-pid", "0100", "--control-serial", "SN1", "--control-interface", "1"],
    )
    .await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        posted(&reqs),
        json!({"control_port": {"vid": 0x2fe3, "pid": 0x0100, "serial": "SN1", "interface": 1}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_control_port_with_only_vid_and_pid_sends_no_serial_or_interface() {
    let (out, reqs) = run(204, &["--control-vid", "2fe3", "--control-pid", "0100", "--serial", "BR1"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(posted(&reqs), json!({"serial": "BR1", "control_port": {"vid": 0x2fe3, "pid": 0x0100}}));
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_control_port_is_sent_alone() {
    let (out, reqs) = run(204, &["--clear-control-port"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(posted(&reqs), json!({"clear_control_port": true}));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decimal_looking_non_hex_vid_is_refused_before_any_request() {
    let (out, reqs) = run(204, &["--control-vid", "12g4", "--control-pid", "0100"]).await;
    assert!(!out.status.success());
    assert!(reqs.is_empty(), "a bad VID must not reach Core: {reqs:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hexadecimal"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_control_port_without_its_pid_or_with_a_clear_is_refused_by_the_parser() {
    let (out, reqs) = run(204, &["--control-vid", "2fe3"]).await;
    assert!(!out.status.success() && reqs.is_empty());
    let (out, reqs) = run(204, &["--control-vid", "2fe3", "--control-pid", "0100", "--clear-control-port"]).await;
    assert!(!out.status.success() && reqs.is_empty());
    let (out, reqs) = run(204, &[]).await;
    assert!(!out.status.success() && reqs.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_refusal_is_reported() {
    let (out, _) = run(400, &["--clear-control-port"]).await;
    assert!(!out.status.success());
}
