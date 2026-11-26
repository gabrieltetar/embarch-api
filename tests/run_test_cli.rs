//! `run-test` and `test-status` (`embarch-api` decision 135), against a mock Core.
//!
//! The MCP tools call the same `hil_run` functions, so this is also their wire test;
//! `tool_subcommand_parity` holds the names together.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use support::{Behavior, MockCore};

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/hil-tests/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn reply(status: u16, body: Value) -> Behavior {
    Behavior::Reply { status, reason: "x".into(), content_type: "application/json".into(), body: body.to_string() }
}

fn core(card: Value, status: Value) -> Behavior {
    Behavior::Router {
        routes: vec![
            ("/card".into(), reply(200, card)),
            ("/definitions".into(), reply(201, json!({ "hash": "ab".repeat(32), "kind": "test", "kept": true, "confirmation": "unconfirmed" }))),
            ("/test-runs".into(), reply(201, json!({ "run_id": "x", "run_hash": "cd".repeat(32), "lease_holder": null, "duration_bound_ms": 90000 }))),
            ("/run-1".into(), reply(200, status)),
        ],
        otherwise: Box::new(reply(404, json!("no"))),
    }
}

struct Rig {
    dir: PathBuf,
    cfg: PathBuf,
    repo: PathBuf,
}

impl Rig {
    fn new(mock: &MockCore) -> Rig {
        let dir = support::unique_dir("runtest");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = dir.join("api.toml");
        std::fs::write(
            &cfg,
            format!(
                "[core]\nbase_url = \"{}\"\ntoken = \"t\"\n\n[[projects]]\nname = \"p\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nflash_format = \"hex\"\nchip = \"nRF54L15\"\n",
                mock.base_url(),
                repo.display()
            ),
        )
        .unwrap();
        Rig { dir, cfg, repo }
    }

    /// A custom suite `mine` with the worked example's one test (no `requires`), its study,
    /// and its bench saved under `embarch/topologies/`.
    fn suite(&self, save_bench: bool) {
        let s = self.repo.join("embarch/tests/suites/mine");
        let t = s.join("vbus-5v-source-voltage");
        std::fs::create_dir_all(&t).unwrap();
        let mut def = fixture("test");
        def["requires"] = json!([]);
        let study = serde_json::to_vec_pretty(&fixture("study")).unwrap();
        let host: embarch_study_designer::HostStudy = serde_json::from_value(fixture("study")).unwrap();
        def["study"]["hash"] = json!(embarch_study_designer::hil_test::study_hash(&host));
        std::fs::write(t.join("test.json"), serde_json::to_vec(&def).unwrap()).unwrap();
        std::fs::write(t.join("vbus-5v.study.json"), study).unwrap();
        let mut m = fixture("suite");
        m["id"] = json!("mine");
        m["tests"][0]["file"] = json!("vbus-5v-source-voltage/test.json");
        std::fs::write(s.join("suite.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        if save_bench {
            std::fs::create_dir_all(self.repo.join("embarch/topologies")).unwrap();
            std::fs::write(self.repo.join("embarch/topologies/pd-sink-bench.toml"), "slug = \"pd-sink-bench\"\n").unwrap();
        }
    }

    fn run(&self, args: &[&str]) -> (bool, Value) {
        let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
            .arg("--config")
            .arg(&self.cfg)
            .arg("--json")
            .args(args)
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("LOCALAPPDATA", self.dir.join("data"))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let v = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}\n{}", String::from_utf8_lossy(&out.stderr)));
        (out.status.success(), v)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn card() -> Value {
    json!({
        "run_id": "x", "run_hash": "cd".repeat(32), "lease_holder": null, "prepared_utc_ms": 1,
        "card": {
            "benches": [{ "name": "pd-sink-bench", "tests": ["vbus-5v-source-voltage"] }],
            "duration_bound_ms": 90000,
            "unconfirmed": [{ "kind": "capability", "name": "role", "hash": "ef".repeat(32) }],
            "tests": [{
                "test_id": "vbus-5v-source-voltage",
                "prompts": [{ "list": "steps", "step": "plug", "text": { "agent_text": "Plug the cable in" }, "kind": "setup", "bench_config": null, "timeout_ms": 1000 }],
                "energising": [{ "list": "steps", "step": "on", "target": "psu", "line": { "agent_text": "OUTP ON" } }]
            }]
        },
        "nonce": "must-not-surface", "expires_utc_ms": 5
    })
}

const IDENT: &[&str] = &[
    "--standard-name", "USB PD", "--standard-revision", "R3.2", "--declared-serial", "SN1", "--board-revision", "A",
    "--project", "p",
];

fn run_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["run-test"];
    a.extend_from_slice(extra);
    a.extend_from_slice(IDENT);
    a
}

fn find<'a>(reqs: &'a [support::RecordedRequest], method: &str, path: &str) -> &'a support::RecordedRequest {
    reqs.iter().find(|r| r.method == method && r.path() == path).unwrap_or_else(|| panic!("no {method} {path} in {reqs:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn run_test_sends_the_run_the_crate_gives_and_shows_the_card_with_an_unconfirmed_input() {
    let mock = MockCore::start(core(card(), json!({}))).await;
    let rig = Rig::new(&mock);
    rig.suite(true);
    let caps = rig.dir.join("caps.json");
    std::fs::write(&caps, serde_json::to_vec(&json!([{ "name": "role", "value": "source", "provenance": "declared" }])).unwrap()).unwrap();
    let (ok, v) = rig.run(&run_args(&["--suite", "mine", "--test", "vbus-5v-source-voltage", "--capabilities-file", caps.to_str().unwrap()]));
    assert!(ok, "{v}");
    assert_eq!(v["started"], false);
    assert_eq!(v["card"]["card"]["unconfirmed"][0]["name"], "role");
    assert!(v["card"].get("nonce").is_none(), "no nonce reaches the agent: {v}");
    assert!(v["message"].as_str().unwrap().contains("not started"), "{v}");

    let reqs = mock.requests();
    let sent = find(&reqs, "POST", "/test-runs");
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    let run = &body["run"];
    assert_eq!(run["suite"]["id"], "mine");
    assert_eq!(run["standard"]["name"], "USB PD");
    assert_eq!(run["dut"]["declared_serial"], "SN1");
    assert_eq!(run["tests"][0]["test_id"], "vbus-5v-source-voltage");
    assert_eq!(run["benches"][0]["name"], "pd-sink-bench");
    assert_eq!(body["plan"].as_str().unwrap().len(), 64);
    assert_eq!(body["suite"].as_str().unwrap().len(), 64, "a repo suite's manifest hash goes with it");
    assert!(body["studies"]["vbus-5v.study.json"].is_object(), "{body}");
    // The definitions went first.
    let kinds: Vec<String> = reqs
        .iter()
        .filter(|r| r.path() == "/definitions")
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["test", "suite", "plan"]);
    // No call goes near the go.
    assert!(reqs.iter().all(|r| !r.path().ends_with("/go")), "{reqs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suite_with_no_selection_lists_and_prepares_nothing() {
    let mock = MockCore::start(core(card(), json!({}))).await;
    let rig = Rig::new(&mock);
    rig.suite(true);
    let (ok, v) = rig.run(&["run-test", "--suite", "mine", "--project", "p"]);
    assert!(ok, "{v}");
    assert_eq!(v["prepared"], false);
    assert!(v["ask"].as_str().unwrap().contains("Ask the engineer"));
    assert_eq!(v["applicable_by_bench"]["pd-sink-bench"][0]["test_id"], "vbus-5v-source-voltage");
    assert!(mock.requests().iter().all(|r| r.method != "POST"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bench_the_project_has_not_saved_leaves_the_test_out_and_core_is_not_called() {
    let mock = MockCore::start(core(card(), json!({}))).await;
    let rig = Rig::new(&mock);
    rig.suite(false);
    let caps = rig.dir.join("c.json");
    std::fs::write(&caps, "[]").unwrap();
    let (ok, v) = rig.run(&run_args(&["--suite", "mine", "--all", "--capabilities-file", caps.to_str().unwrap()]));
    assert!(!ok, "{v}");
    assert!(v["error"].as_str().unwrap().contains("bench pd-sink-bench does not exist"), "{v}");
    assert!(mock.requests().iter().all(|r| r.method != "POST"), "{:?}", mock.requests());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_standard_and_serial_are_asked_for_not_guessed() {
    let mock = MockCore::start(core(card(), json!({}))).await;
    let rig = Rig::new(&mock);
    rig.suite(true);
    let (ok, v) = rig.run(&["run-test", "--suite", "mine", "--all", "--project", "p"]);
    assert!(!ok);
    assert!(v["error"].as_str().unwrap().contains("ask the engineer"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_status_gives_bench_test_verdicts_and_a_waiting_prompt_with_the_relay_rule() {
    let status = json!({
        "run_id": "run-1", "run_hash": "cd".repeat(32), "state": "running", "bench": "pd-sink-bench", "test": "t2",
        "verdicts": [{ "test_id": "t1", "bench": "pd-sink-bench", "verdict": "pass", "reason": null, "study_ids": [] }],
        "waiting_prompt": { "prompt_id": 3, "step": "look", "text": { "agent_text": "Is the LED green?" }, "bench_config": null, "deadline_utc_ms": 9, "kind": "observation", "observation": null, "switched_on": [] }
    });
    let mock = MockCore::start(core(card(), status)).await;
    let rig = Rig::new(&mock);
    let (ok, v) = rig.run(&["test-status", "run-1"]);
    assert!(ok, "{v}");
    assert_eq!(v["run"]["bench"], "pd-sink-bench");
    assert_eq!(v["run"]["test"], "t2");
    assert_eq!(v["run"]["verdicts"][0]["verdict"], "pass");
    assert_eq!(v["run"]["waiting_prompt"]["text"]["agent_text"], "Is the LED green?");
    assert_eq!(v["run"]["prompt_relay"]["relayable"], false);
}

/// The go takes the person's token (core decision 135); nothing in the agent's surfaces may
/// name its route or the client's `test_run_go`. The client crate carries that method for the UI.
#[test]
fn no_code_path_in_the_server_or_the_cli_names_the_go() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in ["src/tools.rs", "src/cli.rs", "src/hil_run.rs"] {
        let src = std::fs::read_to_string(root.join(rel)).unwrap();
        for (n, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            assert!(!line.contains("test_run_go"), "{rel}:{}: {line}", n + 1);
            assert!(!line.contains("/go\"") && !line.contains("/go}") && !line.contains("}/go"), "{rel}:{}: {line}", n + 1);
        }
    }
}
