//! `test-show`, `test-pending` and `test-add` (`embarch-api` decision 142) against a mock
//! Core: pending read from the run log, the mirror rewritten to it, a manual verdict sent
//! with the agent's token and read back as relayed, and the legacy ledger left alone.

mod support;

use std::path::PathBuf;

use serde_json::{json, Value};
use support::{Behavior, MockCore};

fn reply(status: u16, body: Value) -> Behavior {
    Behavior::Reply { status, reason: "x".into(), content_type: "application/json".into(), body: body.to_string() }
}

fn line(entry: Value, hash: &str) -> Value {
    json!({ "entry": entry, "hash": hash })
}

fn log() -> Value {
    json!({ "verified": true, "error": null, "length": 3, "head": "h3", "entries": [
        line(json!({ "type": "test", "seq": 0, "run_id": "r1", "test_id": "passes", "definition_hash": "0".repeat(64), "unit_key": "U1", "dut_image": { "claim": "aa" }, "verdict": "pass" }), "h1"),
        line(json!({ "type": "test", "seq": 1, "run_id": "r1", "test_id": "fails", "definition_hash": "1".repeat(64), "unit_key": "U1", "dut_image": null, "verdict": "fail" }), "h2"),
        line(json!({ "type": "manual_verdict", "seq": 2, "test_id": "fails", "definition_hash": "1".repeat(64), "unit_key": "U1", "dut_image": null, "verdict": "pass", "reason": "looked", "note": null, "scope": "agent", "relayed": true, "at_utc_ms": 5 }), "h3"),
    ]})
}

fn core() -> Behavior {
    Behavior::Router {
        routes: vec![
            ("/run-log".into(), reply(200, log())),
            ("/manual-verdicts".into(), reply(200, json!({ "entry": { "type": "manual_verdict", "scope": "agent", "relayed": true }, "hash": "h4" }))),
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
        let dir = support::unique_dir("tledger");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = dir.join("api.toml");
        std::fs::write(
            &cfg,
            format!(
                "[core]\nbase_url = \"{}\"\ntoken = \"agent-t\"\n\n[[projects]]\nname = \"p\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nflash_format = \"hex\"\nchip = \"nRF54L15\"\n",
                mock.base_url(),
                repo.display()
            ),
        )
        .unwrap();
        Rig { dir, cfg, repo }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_reads_the_log_and_a_relayed_pass_clears_nothing() {
    let mock = MockCore::start(core()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = rig.run(&["test-pending", "--project", "p"]);
    assert!(ok, "{v}");
    let all: Vec<String> = v["by_bench"].as_object().unwrap().values().flat_map(|b| b.as_array().unwrap().iter().map(|i| format!("{}:{}", i["id"].as_str().unwrap(), i["reason"].as_str().unwrap()))).collect();
    assert_eq!(all, ["fails:fail"], "the pass is done; the relayed pass does not clear the failure");
    assert_eq!(v["by_bench"]["(unknown bench)"][0]["relayed"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_differing_mirror_row_is_rewritten_to_the_logs() {
    let mock = MockCore::start(core()).await;
    let rig = Rig::new(&mock);
    let tests = rig.repo.join("embarch/tests");
    std::fs::create_dir_all(&tests).unwrap();
    let stale = json!({ "hash": "h1", "seq": 0, "type": "test", "test_id": "passes", "verdict": "fail", "run_id": null, "unit_key": null, "definition_hash": null, "dut_image": null, "relayed": false, "reason": null, "at_utc_ms": null, "branch": "main", "commit": "c1" });
    std::fs::write(tests.join("results.jsonl"), format!("{stale}\n")).unwrap();
    let (ok, v) = rig.run(&["test-show", "--project", "p"]);
    assert!(ok, "{v}");
    assert_eq!(v["mirror"]["rewritten"], 1);
    assert_eq!(v["mirror"]["added"], 2);
    let rows: Vec<Value> = std::fs::read_to_string(tests.join("results.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0]["verdict"].as_str(), rows[0]["commit"].as_str()), (Some("pass"), Some("c1")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_row_is_listed_apart_and_the_folder_is_untouched() {
    let mock = MockCore::start(core()).await;
    let rig = Rig::new(&mock);
    let legacy = rig.repo.join("embarch/ledger");
    std::fs::create_dir_all(&legacy).unwrap();
    let rows = "{\"when\":\"2026-10-28T00:00:00Z\",\"test\":\"pd-5v\",\"result\":\"pass\"}\n";
    std::fs::write(legacy.join("rows.jsonl"), rows).unwrap();
    let (ok, v) = rig.run(&["test-show", "--project", "p"]);
    assert!(ok, "{v}");
    assert_eq!(v["legacy"]["rows"][0]["test"], "pd-5v");
    assert_eq!(v["legacy"]["note"], "before Test, not in Core's log");
    assert!(v["entries"].as_array().unwrap().iter().all(|e| e["test_id"] != "pd-5v"));
    assert_eq!(std::fs::read_to_string(legacy.join("rows.jsonl")).unwrap(), rows);
    assert_eq!(std::fs::read_dir(&legacy).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_add_with_the_agents_token_reaches_the_manual_verdict_route_and_reads_relayed() {
    let mock = MockCore::start(core()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = rig.run(&["test-add", "fails", "pass", "--reason", "looked", "--unit-key", "U1", "--definition-hash", &"1".repeat(64), "--project", "p"]);
    assert!(ok, "{v}");
    assert_eq!(v["relayed"], true);
    assert_eq!(v["attribution"], "relayed by the agent");
    let posts: Vec<_> = mock.requests().into_iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].path().ends_with("/manual-verdicts"));
    assert_eq!(posts[0].header("authorization"), Some("Bearer agent-t"));
    let body: Value = serde_json::from_str(&posts[0].body_text()).unwrap();
    assert_eq!((body["test_id"].as_str(), body["verdict"].as_str(), body["unit_key"].as_str()), (Some("fails"), Some("pass"), Some("U1")));
    assert!(rig.repo.join("embarch/tests/results.jsonl").exists(), "the mirror follows the log");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_add_refuses_a_bad_verdict_before_core() {
    let mock = MockCore::start(core()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = rig.run(&["test-add", "fails", "maybe", "--reason", "r", "--unit-key", "U1", "--definition-hash", &"1".repeat(64), "--project", "p"]);
    assert!(!ok, "{v}");
    assert!(mock.requests().iter().all(|r| r.method != "POST"));
}
