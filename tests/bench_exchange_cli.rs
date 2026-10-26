//! `bench-exchange` against a mock Core (`tasks/api/131`, decision 89): the
//! one-call sequence, the merged timeline and its filters, the reply that
//! must not appear twice, the declared-signal check that runs before any
//! write, and snapshots from config. The mock is scripted, not stateful: what
//! is under test is the client. Two real consoles are the hardware debt.

mod support;

use std::process::{Command, Output};

use serde_json::{json, Value};
use support::{Behavior, MockCore};

fn signals() -> Value {
    json!([
        { "name": "bench", "origin_role": "dut", "direction": "bidirectional", "route": { "kind": "direct", "port_serial": "A" } },
        { "name": "dut", "origin_role": "dut", "direction": "bidirectional", "route": { "kind": "direct", "port_serial": "B" } },
    ])
}

fn cap(state: &str, text: &str, offset: u64, next: u64, total: u64) -> Value {
    json!({ "id": "cap-1", "signal": "dut", "port": "COM9", "baud": 115200, "state": state,
            "running": state == "running", "offset": offset, "next_offset": next, "total_bytes": total,
            "text": text, "elapsed_ms": 10, "gaps": [] })
}

fn exchange(text: &str) -> Value {
    json!({ "signal": "bench", "port": "COM8", "baud": 115200, "text": text, "until_seen": true,
            "truncated": false, "elapsed_ms": 40, "written": true })
}

fn router(extra: Vec<(&str, Behavior)>, capture_script: Vec<Behavior>) -> Behavior {
    let mut routes: Vec<(String, Behavior)> = extra.into_iter().map(|(p, b)| (p.to_string(), b)).collect();
    routes.push(("/signals/dut/capture".into(), Behavior::Sequence(capture_script)));
    routes.push(("/signals".into(), Behavior::json_ok(signals())));
    Behavior::Router { routes, otherwise: Box::new(Behavior::plain_text_error(404, "Not Found", "unscripted")) }
}

fn run(mock: &MockCore, config_extra: &str, args: &[&str]) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "embarch-api-bex-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let extra = config_extra.replace("{SRC}", src.to_str().unwrap());
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n{extra}", mock.base_url())).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(["--json"])
        .arg("bench-exchange")
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn json_out(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout)))
}

fn has(mock: &MockCore, method: &str, path: &str) -> usize {
    mock.requests().iter().filter(|r| r.method == method && r.path() == path).count()
}

const LOG: &str = "<inf> pe: attached\r\n<wrn> pe: hard reset\r\nuart:~$ ";

#[tokio::test(flavor = "multi_thread")]
async fn one_call_merges_a_command_and_the_other_console_s_log_with_the_level_floor() {
    let script = vec![
        Behavior::json_ok(cap("running", "", 0, 0, 0)),
        Behavior::json_ok(cap("running", LOG, 0, LOG.len() as u64, LOG.len() as u64)),
        Behavior::json_ok(cap("running", "", LOG.len() as u64, LOG.len() as u64, LOG.len() as u64)),
    ];
    let mock = MockCore::start(router(vec![("/signals/bench/exchange", Behavior::json_ok(exchange("pd status\r\nrole: src\r\nuart:~$ ")))], script)).await;
    let steps = r#"[{"signal":"bench","write":"pd status","until":"uart:~$ "}]"#;
    let o = run(&mock, "", &["--steps", steps, "--listen", "dut", "--tail-ms", "250", "--poll-ms", "20", "--filters", r#"{"dut":{"level":"wrn"}}"#]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_out(&o);
    let t = v["timeline"].as_str().unwrap();
    let lines: Vec<&str> = t.lines().collect();
    assert!(lines[0].ends_with(" bench> pd status"), "{t}");
    assert!(t.contains(" bench| role: src"), "{t}");
    assert!(t.contains(" dut| <wrn> pe: hard reset"), "{t}");
    assert!(t.contains(" dut| uart:~$ "), "unterminated tail flushed at stop: {t}");
    assert!(!t.contains("attached"), "level floor drops <inf>: {t}");
    assert_eq!(v["dropped"]["dut"], json!({ "filtered": 1, "capped": 0 }));
    assert!(v["time_basis"].as_str().unwrap().contains("Core stamps no per-line time"));
    // One capture, one exchange, and the capture was stopped.
    assert_eq!((has(&mock, "POST", "/signals/dut/capture"), has(&mock, "DELETE", "/signals/dut/capture"), has(&mock, "POST", "/signals/bench/exchange")), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_on_a_listened_signal_is_not_shown_twice() {
    let text = "help\r\nCommands: foo\r\n";
    let script = vec![
        Behavior::json_ok(cap("running", "", 0, 0, 0)),
        Behavior::json_ok(cap("running", text, 0, text.len() as u64, text.len() as u64)),
        Behavior::json_ok(cap("running", "", text.len() as u64, text.len() as u64, text.len() as u64)),
    ];
    let mock = MockCore::start(router(vec![("/signals/dut/exchange", Behavior::json_ok(exchange(text)))], script)).await;
    let o = run(&mock, "", &["--steps", r#"[{"signal":"dut","write":"help"}]"#, "--listen", "dut", "--tail-ms", "100", "--poll-ms", "20"]);
    let v = json_out(&o);
    let t = v["timeline"].as_str().unwrap();
    assert_eq!(t.matches("Commands: foo").count(), 1, "{t}");
    assert_eq!(v["steps"][0]["reply_in"], "capture rows");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_signal_is_refused_before_any_write_or_capture() {
    let mock = MockCore::start(router(vec![], vec![Behavior::json_ok(cap("running", "", 0, 0, 0))])).await;
    let o = run(&mock, "", &["--steps", r#"[{"signal":"nope","write":"x"}]"#, "--listen", "dut"]);
    assert_eq!(o.status.code(), Some(1));
    let e = json_out(&o)["error"].as_str().unwrap().to_string();
    assert!(e.contains("not declared: nope") && e.contains("bench, dut"), "{e}");
    assert_eq!(mock.requests().iter().filter(|r| r.method != "GET").count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capture_409_stops_before_the_first_write() {
    let mock = MockCore::start(router(vec![], vec![Behavior::plain_text_error(409, "Conflict", "signal dut already has a running capture cap-0")])).await;
    let o = run(&mock, "", &["--steps", r#"[{"signal":"bench","write":"x"}]"#, "--listen", "dut"]);
    assert_eq!(o.status.code(), Some(1));
    let e = json_out(&o)["error"].as_str().unwrap().to_string();
    assert!(e.contains("nothing was written") && e.contains("already has a running capture"), "{e}");
    assert_eq!(has(&mock, "POST", "/signals/bench/exchange"), 0);
}

const SNAP: &str = r#"
[[projects]]
name = "pd"
source_path = "{SRC}"
build_command = ["true"]
artifact_path = "a.hex"
chip = "nRF54L15"
flash_format = "hex"

[projects.snapshots.stacks]
steps = [{ signal = "bench", write = "pd status", until = "uart:~$ " }, { signal = "bench", write = "sense", timeout_ms = 500 }]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_runs_its_steps_after_the_explicit_ones() {
    let mock = MockCore::start(router(vec![("/signals/bench/exchange", Behavior::json_ok(exchange("ok\r\n")))], vec![Behavior::json_ok(cap("running", "", 0, 0, 0))])).await;
    let o = run(&mock, SNAP, &["--snapshot", "stacks", "--steps", r#"[{"signal":"bench","write":"first"}]"#]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stdout));
    let writes: Vec<String> = mock.requests().iter().filter(|r| r.path() == "/signals/bench/exchange").map(|r| r.body_text()).collect();
    assert_eq!(writes.len(), 3);
    assert!(writes[0].contains("first") && writes[1].contains("pd status") && writes[2].contains("sense"), "{writes:?}");
    assert!(writes[2].contains("\"timeout_ms\":500"));
    let o = run(&mock, SNAP, &["--snapshot", "missing"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(json_out(&o)["error"].as_str().unwrap().contains("declared: pd/stacks"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_with_no_steps_is_refused_at_config_load() {
    let mock = MockCore::start(router(vec![], vec![])).await;
    let bad = SNAP.replace(r#"steps = [{ signal = "bench", write = "pd status", until = "uart:~$ " }, { signal = "bench", write = "sense", timeout_ms = 500 }]"#, "steps = []");
    let o = run(&mock, &bad, &["--snapshot", "stacks"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stdout).contains("has no steps"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
}
