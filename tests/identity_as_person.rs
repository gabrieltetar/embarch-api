//! Decision 143: on a person-scope `403` every identity tool names the person's CLI command and
//! the UI tab; `--as-person` sends the person's token after a typed `yes`; the MCP server never
//! builds a person-scope client.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

const AGENT: &str = "agent-t";
const PERSON: &str = "person-t";

fn reply(status: u16, body: Value) -> Behavior {
    Behavior::Reply { status, reason: "x".into(), content_type: "application/json".into(), body: body.to_string() }
}

fn forbidden() -> Behavior {
    Behavior::Reply { status: 403, reason: "Forbidden".into(), content_type: "text/plain".into(), body: "person scope required".into() }
}

fn config(mock: &MockCore, dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"{AGENT}\"\nperson_token = \"{PERSON}\"\n", mock.base_url())).unwrap();
    cfg
}

fn bearer(r: &RecordedRequest) -> String {
    r.header("authorization").unwrap().to_string()
}

/// Every identity tool, with arguments that get past this side's own checks to Core.
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("enroll_probe", json!({ "chip": "nRF52840_xxAA", "role": "dut" })),
        ("unenroll_probe", json!({ "role": "dut" })),
        ("set_role_board", json!({ "role": "dut", "board": "b", "chip": "c" })),
        ("declare_instrument", json!({ "name": "load", "port": "1a86:7523", "preset": "et54" })),
        ("remove_instrument", json!({ "name": "load" })),
        ("declare_signal", json!({ "name": "s", "origin_role": "dut", "direction": "dut-to-host", "route_kind": "direct", "port_serial": "X1" })),
        ("remove_signal", json!({ "name": "s" })),
        ("declare_bootload_ports", json!({ "bootloader": "2fe3:000c" })),
        ("clear_bootload_ports", json!({})),
        ("dev_bench_link", json!({ "serial": "S1" })),
    ]
}

#[tokio::test]
async fn every_identity_tool_names_the_cli_command_and_the_ui_tab_on_a_403() {
    let mock = MockCore::start(forbidden()).await;
    let dir = support::unique_dir("identity-mcp");
    let cfg = config(&mock, &dir);
    let mut child = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .env_remove("EMBARCH_PERSON_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut send = move |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    let mut read_id = move |id: u64| -> Value {
        loop {
            let mut line = String::new();
            assert!(out.read_line(&mut line).unwrap() > 0, "the server closed before answering {id}");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    };
    let results = tokio::task::spawn_blocking(move || {
        send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } } }));
        read_id(1);
        send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        let mut results = Vec::new();
        for (i, (tool, args)) in calls().into_iter().enumerate() {
            let id = i as u64 + 2;
            send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": tool, "arguments": args } }));
            let r = read_id(id);
            results.push((tool, r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()));
        }
        send(json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list" }));
        let list = read_id(99);
        (results, list)
    })
    .await
    .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let (results, list) = results;
    for (tool, text) in &results {
        let command = format!("embarch-api {} --as-person", tool.replace('_', "-"));
        assert!(text.contains(&command), "{tool}: {text}");
        assert!(text.contains("embarch-ui's Topology tab"), "{tool}: {text}");
    }
    for (tool, _) in &results {
        let t = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == *tool).unwrap();
        let d = t["description"].as_str().unwrap();
        assert!(d.contains("Your own act only on an unattested Core"), "{tool}: {d}");
    }
    assert!(mock.requests().iter().all(|r| bearer(r) == format!("Bearer {AGENT}")), "the MCP server sent a token that is not the agent's");
}

async fn as_person(args: &[&str], stdin: &str) -> (bool, String, Vec<RecordedRequest>) {
    let mock = MockCore::start(reply(200, json!({}))).await;
    let dir = support::unique_dir("identity-cli");
    let cfg = config(&mock, &dir);
    let mut child = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .env_remove("EMBARCH_PERSON_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string(), mock.requests())
}

#[tokio::test]
async fn as_person_sends_the_persons_token_after_a_yes() {
    let (ok, stderr, requests) = as_person(&["remove-signal", "s", "--as-person"], "yes\n").await;
    assert!(ok, "{stderr}");
    let writes: Vec<_> = requests.iter().filter(|r| r.method != "GET").collect();
    assert_eq!((writes.len(), writes[0].method.as_str(), writes[0].path()), (1, "DELETE", "/signals/s"));
    assert!(requests.iter().all(|r| bearer(r) == format!("Bearer {PERSON}")), "{requests:?}");
}

#[tokio::test]
async fn as_person_without_a_typed_yes_sends_nothing() {
    for input in ["", "\n", "no\n", "y\n", "YES\n"] {
        let (ok, _, requests) = as_person(&["remove-signal", "s", "--as-person"], input).await;
        assert!(!ok && requests.is_empty(), "{input:?}: {requests:?}");
    }
}

#[tokio::test]
async fn as_person_is_refused_for_every_other_subcommand() {
    let (ok, stderr, requests) = as_person(&["status", "--as-person"], "yes\n").await;
    assert!(!ok && requests.is_empty(), "{stderr}");
    assert!(stderr.contains("identity acts"), "{stderr}");
}

/// The MCP server's code names no person-scope client: only the CLI's two modules do.
#[test]
fn only_the_cli_builds_a_person_scope_client() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in walk(&src) {
        let name = entry.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
        if matches!(name.as_str(), "cli.rs" | "person_cli.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap();
        for needle in ["TokenScope::Person", "new_with_stores"] {
            let own_test = name == "identity_act.rs";
            assert!(own_test || !text.contains(needle), "src/{name} names {needle}");
        }
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
