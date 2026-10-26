//! `signal-exchange`'s exit codes and `--clean`, against a mock Core
//! (`tasks/api/123`). A reply that never showed `--until`, zero bytes, and a
//! truncated reply each exit with their own code and one stderr line; a
//! complete reply exits 0 with stdout unchanged.

mod support;

use std::process::{Command, Output};

use serde_json::json;
use support::{Behavior, MockCore};

fn reply(text: &str, until_seen: bool, truncated: bool) -> serde_json::Value {
    json!({
        "signal": "console", "port": "/dev/ttyUSB0", "baud": 115200,
        "text": text, "until_seen": until_seen, "truncated": truncated, "elapsed_ms": 500,
    })
}

async fn run(body: serde_json::Value, extra: &[&str]) -> Output {
    let mock = MockCore::start(Behavior::json_ok(body)).await;
    let dir = std::env::temp_dir().join(format!(
        "embarch-api-sigex-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(if extra.contains(&"--json") { &["--json"][..] } else { &[][..] })
        .args(["signal-exchange", "--name", "console", "--write", "help"])
        .args(extra.iter().filter(|a| **a != "--json"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn complete_reply_exits_zero_with_text_unchanged() {
    let o = run(reply("help\r\nfoo\r\nuart:~$ ", true, false), &["--until", "uart:~$ "]).await;
    assert_eq!(code(&o), 0);
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("help\r\nfoo"));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("signal-exchange:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn until_not_seen_exits_3() {
    let o = run(reply("help\r\npartial", false, false), &["--until", "uart:~$ "]).await;
    assert_eq!(code(&o), 3);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("no `uart:~$ ` within 500 ms: 13 bytes read, echo seen"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_bytes_exits_4() {
    let o = run(reply("", false, false), &["--until", "uart:~$ "]).await;
    assert_eq!(code(&o), 4);
    assert!(String::from_utf8_lossy(&o.stderr).contains("0 bytes read"));
    let o = run(reply("", false, false), &[]).await;
    assert_eq!(code(&o), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_exits_5() {
    let o = run(reply("help\r\nlots", true, true), &["--until", "uart:~$ "]).await;
    assert_eq!(code(&o), 5);
    assert!(String::from_utf8_lossy(&o.stderr).contains("truncated"));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_until_complete_reply_exits_zero() {
    let o = run(reply("help\r\nfoo", false, false), &[]).await;
    assert_eq!(code(&o), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_drops_escapes_echo_and_prompt() {
    let o = run(reply("help\r\na\x1b[3Cb\r\n\x1b[1;32muart:~$ \x1b[m", true, false), &["--until", "uart:~$ ", "--clean"]).await;
    assert_eq!(code(&o), 0);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "a   b\n\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_is_unchanged_and_exit_code_still_marks_the_problem() {
    let o = run(reply("help\r\nx", false, false), &["--until", "uart:~$ ", "--json"]).await;
    assert_eq!(code(&o), 3);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["exchange"]["until_seen"], false);
    assert_eq!(v["exchange"]["text"], "help\r\nx");
}
