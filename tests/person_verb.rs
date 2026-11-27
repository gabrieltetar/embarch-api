//! `embarch-api person ...` (decision 141) against a mock Core: the person's token, the
//! card's nonce, a typed `yes` before anything is sent, and no MCP tool.

mod support;

use std::io::Write;
use std::process::Stdio;

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

const AGENT: &str = "agent-t";
const PERSON: &str = "person-t";

fn reply(status: u16, body: Value) -> Behavior {
    Behavior::Reply { status, reason: "x".into(), content_type: "application/json".into(), body: body.to_string() }
}

fn core() -> Behavior {
    Behavior::Router {
        routes: vec![
            ("/confirmations/h1/card".into(), reply(200, json!({ "card": { "name": { "agent_text": "Plug it in" } }, "confirmation": "unconfirmed", "nonce": "n-confirm", "expires_utc_ms": 9 }))),
            ("/confirmations".into(), reply(200, json!({ "confirmed": true }))),
            ("/test-runs/run-1/card".into(), reply(200, json!({ "run_id": "run-1", "run_hash": "cd".repeat(32), "card": { "duration": 5 }, "nonce": "n-go", "expires_utc_ms": 9 }))),
            ("/test-runs/run-1/go".into(), reply(200, json!({ "started": true }))),
            ("/test-runs/run-1".into(), reply(200, json!({ "run_id": "run-1", "waiting_prompt": { "prompt_id": 3, "kind": "observation", "text": { "agent_text": "Is the LED green?" } } }))),
            ("/study/run-1/prompt".into(), reply(200, json!({ "study_id": "run-1", "prompt_id": 3, "answer": "confirmed", "answered_by": "cli", "status": "recorded" }))),
            ("/units/DUT-1".into(), reply(200, json!({ "ok": true }))),
            ("/person".into(), reply(200, json!({ "ok": true }))),
        ],
        otherwise: Box::new(reply(404, json!("no"))),
    }
}

struct Run {
    ok: bool,
    stderr: String,
    requests: Vec<RecordedRequest>,
}

impl Run {
    fn writes(&self) -> Vec<&RecordedRequest> {
        self.requests.iter().filter(|r| r.method != "GET").collect()
    }
}

async fn person(args: &[&str], stdin: &str) -> Run {
    let mock = MockCore::start(core()).await;
    let dir = support::unique_dir("person");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"{AGENT}\"\nperson_token = \"{PERSON}\"\n", mock.base_url())).unwrap();
    let mut child = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .arg("person")
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
    Run { ok: out.status.success(), stderr: String::from_utf8_lossy(&out.stderr).to_string(), requests: mock.requests() }
}

fn body(r: &RecordedRequest) -> Value {
    serde_json::from_str(&r.body_text()).unwrap()
}

fn bearer(r: &RecordedRequest) -> String {
    r.header("authorization").unwrap().to_string()
}

#[tokio::test]
async fn confirm_sends_the_persons_token_and_the_cards_nonce_after_a_yes() {
    let run = person(&["confirm", "h1"], "yes\n").await;
    assert!(run.ok, "{}", run.stderr);
    assert!(run.requests.iter().all(|r| bearer(r) == format!("Bearer {PERSON}")), "{:?}", run.requests);
    let w = run.writes();
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].method.as_str(), w[0].path()), ("POST", "/confirmations"));
    assert_eq!(body(w[0]), json!({ "hash": "h1", "nonce": "n-confirm", "surface": "cli" }));
    assert!(run.stderr.contains("name: \"Plug it in\""), "{}", run.stderr);
    assert!(!run.stderr.contains("n-confirm"), "the nonce is not shown");
}

#[tokio::test]
async fn go_sends_the_run_hash_and_nonce_of_the_card() {
    let run = person(&["go", "run-1"], "yes\n").await;
    assert!(run.ok, "{}", run.stderr);
    assert!(run.requests.iter().all(|r| bearer(r) == format!("Bearer {PERSON}")));
    let w = run.writes();
    assert_eq!((w.len(), w[0].path()), (1, "/test-runs/run-1/go"));
    assert_eq!(body(w[0]), json!({ "run_id": "run-1", "run_hash": "cd".repeat(32), "nonce": "n-go" }));
}

#[tokio::test]
async fn answer_sends_an_observations_value_as_the_persons_cli_answer() {
    let run = person(&["answer", "run-1", "3", "5.2"], "yes\n").await;
    assert!(run.ok, "{}", run.stderr);
    assert!(run.stderr.contains("\"Is the LED green?\""), "{}", run.stderr);
    let w = run.writes();
    assert_eq!((w.len(), w[0].path()), (1, "/study/run-1/prompt"));
    assert_eq!(bearer(w[0]), format!("Bearer {PERSON}"));
    assert_eq!(body(w[0]), json!({ "prompt_id": 3, "answer": "confirmed", "answered_by": "cli", "value": 5.2 }));

    let cancelled = person(&["answer", "run-1", "3", "--cancel"], "yes\n").await;
    assert_eq!(body(cancelled.writes()[0]), json!({ "prompt_id": 3, "answer": "cancelled", "answered_by": "cli" }));
}

#[tokio::test]
async fn an_answer_for_another_prompt_sends_nothing() {
    let run = person(&["answer", "run-1", "9"], "yes\n").await;
    assert!(!run.ok);
    assert!(run.writes().is_empty());
}

#[tokio::test]
async fn unit_and_name_are_the_persons_declarations() {
    let unit = person(&["unit", "DUT-1", "--serial", "SN7", "--revision", "B"], "yes\n").await;
    assert!(unit.ok, "{}", unit.stderr);
    let w = unit.writes();
    assert_eq!((w[0].method.as_str(), w[0].path(), bearer(w[0])), ("PUT", "/units/DUT-1", format!("Bearer {PERSON}")));
    assert_eq!(body(w[0]), json!({ "serial": "SN7", "board_revision": "B" }));
    let name = person(&["name", "Ada"], "yes\n").await;
    let w = name.writes();
    assert_eq!((w[0].method.as_str(), w[0].path(), bearer(w[0])), ("PUT", "/person", format!("Bearer {PERSON}")));
    assert_eq!(body(w[0]), json!({ "name": "Ada" }));
}

#[tokio::test]
async fn anything_but_a_typed_yes_sends_nothing() {
    let cases: [&[&str]; 5] = [&["confirm", "h1"], &["go", "run-1"], &["answer", "run-1", "3", "green"], &["unit", "DUT-1", "--serial", "S", "--revision", "A"], &["name", "Ada"]];
    for args in cases {
        for input in ["", "\n", "no\n", "y\n", "YES\n", "yes please\n"] {
            let run = person(args, input).await;
            assert!(!run.ok, "{args:?} {input:?}");
            assert!(run.writes().is_empty(), "{args:?} {input:?}: {:?}", run.writes());
        }
    }
}

/// The MCP server registers no tool for any of these acts and never builds the person's client.
#[test]
fn the_mcp_server_registers_none_of_them() {
    let tools = include_str!("../src/tools.rs");
    for needle in ["person_cli", "TokenScope::Person", "declare_unit", "declare_person", "confirmation_card", ".confirm(", "test_run_go"] {
        assert!(!tools.contains(needle), "src/tools.rs names {needle}");
    }
    let banned = ["person", "confirm", "go", "answer", "unit", "name", "declare_unit", "declare_person", "confirm_definition", "give_go"];
    let mut lines = tools.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("#[tool(description") {
            for next in lines.by_ref() {
                if let Some(name) = next.trim_start().strip_prefix("async fn ") {
                    let name = name.split(['(', '<']).next().unwrap();
                    assert!(!banned.contains(&name), "the MCP server registers {name}");
                    break;
                }
            }
        }
    }
}
