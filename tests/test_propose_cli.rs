//! `test-propose` and `test-plan` (`embarch-api` decision 134), against a mock Core.
//!
//! The MCP tools call the same `hil_repo` functions, so this is also their wire test;
//! `tool_subcommand_parity` holds the two names together. The fixtures are the
//! worked example of `embarch-study-designer`'s `interfaces/hil-tests.md`.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/hil-tests/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

/// A scratch firmware repo and config pointing at the mock.
struct Rig {
    dir: PathBuf,
    cfg: PathBuf,
    repo: PathBuf,
}

impl Rig {
    fn new(mock: &MockCore) -> Rig {
        let dir = support::unique_dir("testpropose");
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

    fn put(&self, name: &str, v: &Value) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, serde_json::to_vec(v).unwrap()).unwrap();
        p
    }

    fn run(&self, args: &[&str]) -> (bool, Value) {
        let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
            .arg("--config")
            .arg(&self.cfg)
            .arg("--json")
            .args(args)
            .args(["--project", "p"])
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

fn kept_reply() -> Behavior {
    Behavior::Reply {
        status: 201,
        reason: "Created".into(),
        content_type: "application/json".into(),
        body: json!({ "hash": "ab".repeat(32), "kind": "test", "kept": true, "checked": "definition", "confirmation": "unconfirmed" }).to_string(),
    }
}

fn measured() -> Value {
    json!({
        "vbus_mean": { "by": "bench", "source": "dmm", "range_min": 0.0, "range_max": 20.0, "resolution": 0.001, "u": 0.01 },
        "vbus_ripple": { "by": "bench", "source": "dmm", "range_min": 0.0, "range_max": 0.5, "resolution": 0.0001, "u": 0.005 },
        "led_state": { "by": "person", "source": "the operator" }
    })
}

/// The worked example's test with the study's hash left out, to be filled.
fn definition() -> Value {
    let mut d = fixture("test");
    d["study"].as_object_mut().unwrap().remove("hash");
    d
}

fn bench() -> Value {
    fixture("suite")["benches"][0].clone()
}

fn posts(reqs: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    reqs.iter().filter(|r| r.method == "POST").collect()
}

fn propose_args<'a>(rig: &'a Rig, extra: &[&'a str]) -> (Vec<String>, PathBuf, PathBuf, PathBuf, PathBuf) {
    let d = rig.put("definition.json", &definition());
    let s = rig.put("study.json", &fixture("study"));
    let b = rig.put("bench.json", &bench());
    let m = rig.put("measured.json", &measured());
    let mut a = vec!["test-propose".to_string(), "test".to_string()];
    a.extend(extra.iter().map(|s| s.to_string()));
    (a, d, s, b, m)
}

fn run_propose(rig: &Rig, extra: &[&str]) -> (bool, Value) {
    let (a, d, s, b, m) = propose_args(rig, extra);
    let mut args: Vec<&str> = a.iter().map(String::as_str).collect();
    for (flag, p) in [("--definition-file", &d), ("--study-file", &s), ("--bench-file", &b), ("--measured-file", &m)] {
        args.push(flag);
        args.push(p.to_str().unwrap());
    }
    rig.run(&args)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_custom_test_arrives_with_its_hash_and_waits_for_confirmation() {
    let mock = MockCore::start(kept_reply()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = run_propose(&rig, &["--suite", "mine"]);
    assert!(ok, "{v}");
    assert_eq!(v["hash"], "ab".repeat(32));
    assert_eq!(v["waits_for_confirmation"], true);
    assert!(v["message"].as_str().unwrap().contains("waits for the engineer to confirm"), "{v}");

    let reqs = mock.requests();
    let sent = posts(&reqs);
    assert_eq!(sent.len(), 1, "{reqs:?}");
    assert_eq!(sent[0].path(), "/definitions");
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["kind"], "test");
    assert_eq!(body["definition"]["test_id"], "vbus-5v-source-voltage");
    assert_eq!(body["definition"]["study"]["hash"].as_str().unwrap().len(), 64, "the study's hash is filled");
    assert!(body["study"].is_object() && body["bench"].is_object() && body["measured"].is_object());

    let suite = rig.repo.join("embarch/tests/suites/mine");
    assert!(suite.join("vbus-5v-source-voltage/test.json").is_file());
    assert!(suite.join("vbus-5v-source-voltage/vbus-5v.study.json").is_file());
    let manifest: Value = serde_json::from_slice(&std::fs::read(suite.join("suite.json")).unwrap()).unwrap();
    assert_eq!(manifest["id"], "mine");
    assert_eq!(manifest["tests"][0]["file"], "vbus-5v-source-voltage/test.json");
    assert_eq!(manifest["benches"][0]["name"], "pd-sink-bench");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_the_crate_refuses_never_reaches_core_and_says_why() {
    let mock = MockCore::start(kept_reply()).await;
    let rig = Rig::new(&mock);
    // A band 0.02 V wide cannot be judged by a meter with U = 0.01 V.
    let mut d = definition();
    d["limits"][0]["limit"] = json!({ "kind": "band", "lo": 4.99, "hi": 5.01 });
    let (_, s, b, m) = {
        let (_, _, s, b, m) = propose_args(&rig, &[]);
        ((), s, b, m)
    };
    let dp = rig.put("narrow.json", &d);
    let (ok, v) = rig.run(&[
        "test-propose", "test", "--definition-file", dp.to_str().unwrap(), "--study-file", s.to_str().unwrap(),
        "--bench-file", b.to_str().unwrap(), "--measured-file", m.to_str().unwrap(), "--suite", "mine",
    ]);
    assert!(!ok);
    assert_eq!(v["refused_before_core"], true);
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.starts_with("vbus-5v-source-voltage: limits[0] (vbus_mean): "), "{reason}");
    assert!(mock.requests().is_empty(), "Core must not be called: {:?}", mock.requests());
    assert!(!rig.repo.join("embarch").exists(), "nothing is written for a refused proposal");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capability_goes_with_its_provenance_and_a_bad_provenance_is_refused() {
    let mock = MockCore::start(kept_reply()).await;
    let rig = Rig::new(&mock);
    let ok_def = rig.put("cap.json", &json!({ "name": "role", "value": "source", "provenance": "declared" }));
    let (ok, v) = rig.run(&["test-propose", "capability", "--definition-file", ok_def.to_str().unwrap()]);
    assert!(ok, "{v}");
    let reqs = mock.requests();
    let body: Value = serde_json::from_slice(&posts(&reqs)[0].body).unwrap();
    assert_eq!(body["kind"], "capability");
    assert_eq!(body["definition"]["provenance"], "declared");

    let bad = rig.put("bad.json", &json!({ "name": "role", "value": "source", "provenance": "guessed" }));
    let (ok, v) = rig.run(&["test-propose", "capability", "--definition-file", bad.to_str().unwrap()]);
    assert!(!ok);
    assert_eq!(v["refused_before_core"], true);
    assert_eq!(posts(&mock.requests()).len(), 1, "the refused one was not sent");
}

fn read_bench(repo: &Path) -> String {
    std::fs::read_to_string(repo.join("embarch/topologies/pd-sink-bench.toml")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn selecting_a_suite_copies_its_bench_and_never_overwrites_a_changed_one() {
    let mock = MockCore::start(kept_reply()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = run_propose(&rig, &["--suite", "mine"]);
    assert!(ok, "{v}");

    let mut plan = fixture("plan");
    plan["suite"] = json!({ "id": "mine", "version": "0.1.0", "commit": "0123456789abcdef0123456789abcdef01234567" });
    let pp = rig.put("plan-in.json", &plan);

    let (ok, v) = rig.run(&["test-plan", "--plan", "regression", "--definition-file", pp.to_str().unwrap()]);
    assert!(ok, "{v}");
    assert_eq!(v["selection"]["copied"][0]["path"], "embarch/topologies/pd-sink-bench.toml");
    assert!(rig.repo.join("embarch/tests/plans/regression.json").is_file());
    let first = read_bench(&rig.repo);
    assert!(first.contains("pd-sink-bench") && first.contains("role = \"dut\""), "{first}");

    // Same again: the same content is reported as such.
    let (ok, v) = rig.run(&["test-plan", "--plan", "regression", "--definition-file", pp.to_str().unwrap()]);
    assert!(ok, "{v}");
    assert_eq!(v["replaced"], true);
    assert_eq!(v["selection"]["unchanged"][0]["bench"], "pd-sink-bench");

    // The saved bench is edited; a third selection leaves it and reports the diff.
    let edited = first.replace("generic-board", "my-own-board");
    assert_ne!(edited, first);
    std::fs::write(rig.repo.join("embarch/topologies/pd-sink-bench.toml"), &edited).unwrap();
    let (ok, v) = rig.run(&["test-plan", "--plan", "regression", "--definition-file", pp.to_str().unwrap()]);
    assert!(ok, "{v}");
    assert_eq!(read_bench(&rig.repo), edited, "a saved bench is never overwritten");
    let left = &v["selection"]["left_alone"][0];
    assert_eq!(left["bench"], "pd-sink-bench");
    let diff: Vec<&str> = left["diff"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert!(diff.iter().any(|l| l.starts_with("- ") && l.contains("my-own-board")), "{diff:?}");
    assert!(diff.iter().any(|l| l.starts_with("+ ") && l.contains("generic-board")), "{diff:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suite_without_a_plan_lists_its_applicable_tests_by_bench() {
    let mock = MockCore::start(kept_reply()).await;
    let rig = Rig::new(&mock);
    let (ok, v) = run_propose(&rig, &["--suite", "mine"]);
    assert!(ok, "{v}");
    let (ok, v) = rig.run(&["test-plan", "--suite", "mine"]);
    assert!(ok, "{v}");
    assert_eq!(v["applicable_by_bench"]["pd-sink-bench"][0]["test_id"], "vbus-5v-source-voltage");
    assert!(v["not_applicable"].as_array().unwrap().is_empty());
}
