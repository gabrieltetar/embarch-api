//! `serial-log` carries Core's `truncated` flag (`tasks/api/135`), against a
//! mock Core: `true` is marked, an absent key is "this Core did not say",
//! never `false` (decision 58's shape).

mod support;

use std::process::{Command, Output};

use embarch_core_client::{CoreClient, CoreConfig, SerialLogResponse};
use serde_json::json;
use support::{Behavior, MockCore};

fn client_for(m: &MockCore) -> CoreClient {
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": m.base_url() })).unwrap();
    CoreClient::new(&cfg).unwrap()
}

async fn cli(body: serde_json::Value, json_flag: bool) -> Output {
    let mock = MockCore::start(Behavior::json_ok(body)).await;
    let dir = std::env::temp_dir().join(format!(
        "embarch-api-slog-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(
        &cfg,
        format!(
            "[core]\nbase_url = \"{}\"\ntoken = \"t\"\n\n[[projects]]\nname = \"p\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nchip = \"nRF54L15\"\nflash_format = \"hex\"\n",
            mock.base_url(),
            dir.display()
        ),
    )
    .unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_embarch-api"));
    c.arg("--config").arg(&cfg);
    if json_flag {
        c.arg("--json");
    }
    let out = c
        .args(["serial-log", "p", "--port", "COM7"])
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_true_is_marked_in_human_output() {
    let o = cli(json!({"port":"COM7","lines":["a"],"truncated":true}), false).await;
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("[TRUNCATED"), "{s}{}", String::from_utf8_lossy(&o.stderr));
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_true_reaches_json_output() {
    let o = cli(json!({"port":"COM7","lines":["a"],"truncated":true}), true).await;
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["truncated"], json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_without_the_key_says_unreported_not_false() {
    let o = cli(json!({"port":"COM7","lines":["a"]}), false).await;
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("did not report"), "{s}");
    let o = cli(json!({"port":"COM7","lines":["a"]}), true).await;
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["truncated"].is_null(), "{v}");
    assert!(v["truncation_note"].as_str().unwrap().contains("did not report"));
}

#[tokio::test(flavor = "multi_thread")]
async fn complete_capture_has_no_marker() {
    let o = cli(json!({"port":"COM7","lines":["a"],"truncated":false}), false).await;
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(!s.contains("TRUNCATED") && !s.contains("did not report"), "{s}");
}

/// The MCP tool emits `SerialLogResponse::to_json`, so this is its JSON.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_parses_the_flag_and_the_mcp_json_carries_it() {
    let mock = MockCore::start(Behavior::json_ok(json!({"port":"COM7","lines":["a"],"truncated":true}))).await;
    let client = client_for(&mock);
    let r: SerialLogResponse = client.serial_log("COM7", 115_200, 250).await.unwrap();
    assert_eq!(r.truncated, Some(true));
    assert_eq!(r.to_json()["truncated"], json!(true));
    let old = MockCore::start(Behavior::json_ok(json!({"port":"COM7","lines":[]}))).await;
    let r = client_for(&old).serial_log("COM7", 115_200, 250).await.unwrap();
    assert_eq!(r.truncated, None);
    assert!(r.to_json()["truncated"].is_null());
}
