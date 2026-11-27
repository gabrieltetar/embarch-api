//! `verify` of a Test run's zip (`embarch-api` decision 146). The Core side is built here, signed
//! with a real Ed25519 key, from my reading of `embarch-core`'s `test_publish.rs`,
//! `test_run_record.rs` and `test_executor.rs` on `origin/main` (2026-11-26) and the shared
//! crate's `judge_run`, `judge_test` and `Rules`; `publish` then writes the zip against a mock Core
//! and its manifest is signed over the files embarch-api made. Nothing here reaches a real Core.
#![cfg(feature = "pdf")]
#![recursion_limit = "256"]

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use embarch_api::publish::{self, PublishRequest};
use embarch_api::verify::{self, Options, Report, Status};
use embarch_core_client::{CoreClient, CoreConfig};
use embarch_study_designer::hil_expand::RunJson;
use embarch_study_designer::verdicts::{Judged, Qualifier, Reason, Rules, Verdict};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{Behavior, MockCore};

const RUN: &str = "fedcba9876543210fedcba9876543210";
const NOW: i64 = 1_763_700_000;
const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const UNIT: &str = "DUT-1";
const STATEMENT: &str = "Pre-compliance evidence. Not a certification by any standards organisation or certification programme, and not an accredited test report.";

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn pub_hex(k: &SigningKey) -> String {
    k.verifying_key().as_bytes().iter().map(|x| format!("{x:02x}")).collect()
}

fn envelope(k: &SigningKey, statement: &Value) -> String {
    let payload = serde_json::to_vec(statement).unwrap();
    let ty = "application/vnd.in-toto+json";
    let mut signed = format!("DSSEv1 {} {} {} ", ty.len(), ty, payload.len()).into_bytes();
    signed.extend_from_slice(&payload);
    let b64 = base64::engine::general_purpose::STANDARD;
    json!({ "payloadType": ty, "payload": b64.encode(&payload),
            "signatures": [{ "keyid": sha(k.verifying_key().as_bytes()), "sig": b64.encode(k.sign(&signed).to_bytes()) }] })
    .to_string()
}

fn subjects(files: &BTreeMap<String, String>) -> Vec<Value> {
    files.iter().map(|(n, b)| json!({ "name": n, "digest": { "sha256": sha(b.as_bytes()) } })).collect()
}

fn log_line(entry: Value) -> (String, String) {
    let hash = sha(&serde_json::to_vec(&entry).unwrap());
    (json!({ "entry": entry, "hash": hash }).to_string(), hash)
}

/// What the Core-side build is told to get wrong. A signed fault: Core signed it, so only the
/// recomputation can find it.
#[derive(Clone, Copy, Default)]
struct Knobs {
    /// The limit `run.json` states for `vbus`; the verdicts are judged against (4500, 5500).
    run_limit: Option<(f64, f64)>,
    /// `report-data.json` says t1 failed.
    flip_report_verdict: bool,
    /// The signed Test run's record has no entry for t2.
    drop_record_entry: bool,
    /// `result.json` says t1's run failed (the study's own record and events still say pass).
    flip_result_run: bool,
    /// `report-data.json` says t1's limit was judged with another `w` than its declared source gives.
    alter_limit_w: bool,
}

fn t1_judged() -> Judged {
    Judged { verdict: Verdict::Pass, reason: None, qualifiers: vec![Qualifier::DeclaredByAgent, Qualifier::RestsOnDutReport] }
}

fn unreached() -> Judged {
    Judged { verdict: Verdict::Skipped, reason: Some(Reason::StoppedByError), qualifiers: vec![] }
}

fn test_json(id: &str, requirements: &[&str], limit: (f64, f64)) -> Value {
    json!({
        "test_id": id, "title": id, "definition_hash": id.repeat(32),
        "study": { "kind": "host", "hash": "0".repeat(64), "file": format!("{id}.json") },
        "requirements": requirements, "revision_range": { "from": "3.0", "to": null },
        "bench": { "name": "A", "config": "default" },
        "precondition": "reset", "requires": [], "run_count": 1,
        "decision_rule": if id == "t1" { json!({ "kind": "guard_band", "w_multiple": 1.0 }) } else { json!({ "kind": "simple_acceptance", "w_multiple": 0.0 }) },
        "limits": if id == "t1" { json!([{ "measurement": "vbus", "unit": "mV", "lower": limit.0, "upper": limit.1 }]) } else { json!([]) },
        "setup_terms": [],
        "sources": if id == "t1" { json!([{ "measurement": "vbus", "unit": "mV", "range_min": 0.0, "range_max": 10000.0, "resolution": 1.0, "U": 10.0, "k": 2.0 }]) } else { json!([]) },
        "prompt_kinds": [], "hooks": [], "step_budget_ms": 1000
    })
}

fn run_json(limit: (f64, f64)) -> String {
    let run: RunJson = serde_json::from_value(json!({
        "schema": 1, "run_id": RUN, "suite": { "id": "usb-pd", "version": "1" }, "plan_hash": null,
        "standard": { "name": "usb-pd", "revision": "3.2" },
        "dut": { "hardware_id": UNIT, "declared_serial": "u1", "board_revision": "A", "capabilities": [], "hooks": [] },
        "options": { "stop_on_fail": false },
        "benches": [ { "name": "A", "tests": ["t1", "t2"], "boards": [ { "part": "pd", "idle": "idle", "hello": "hello" } ] } ],
        "tests": [ test_json("t1", &["R1"], limit), test_json("t2", &["R2"], limit) ],
    }))
    .unwrap();
    run.canonical()
}

const COUNTS: &str = r#"{"pass":1,"fail":0,"error":0,"inconclusive":0,"skipped":0,"cancelled":0}"#;

/// Core's files for the run, signed by `k`.
fn core_files(k: &SigningKey, knobs: Knobs) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    let run = run_json(knobs.run_limit.unwrap_or((4500.0, 5500.0)));
    // The one study run, a host study: its step reads the supply and parses it.
    let study = json!({ "name": "t1", "steps": [{ "name": "read", "timeout_ms": 1000, "action": { "Exchange": {
        "signal": "console", "line": "vbus", "until": "OK", "refused_on": "ERR",
        "parse": { "name": "vbus", "pattern": "vbus: (\\S+) mV", "unit": "mV" } } } }] });
    let events = json!({ "study_name": "t1", "provenance": "declared by the study", "steps": [{
        "step_name": "read", "outcome": "Pass", "detail": "marker_seen", "list": "Steps", "index": 0, "reply": "vbus: 5000 mV\nOK\n",
        "reply_truncated": false, "reply_bytes": 17, "until_seen": true, "port_lost": false, "parsed": { "name": "vbus", "value": 5000.0 } }] });
    let measurements = json!({ "schema": 1, "run_id": "s1", "behaviours": {}, "measurements": [{
        "name": "vbus", "value": 5000.0, "unit": "mV", "at_utc_ms": 1500, "source": { "file": "events.json", "step": "read" },
        "by": "dut", "instrument": null, "bench_image": "unverified", "method": { "kind": "parse", "pattern": "vbus: (\\S+) mV" },
        "resolution": null, "uncertainty": null, "u_source": "undeclared", "declared_by": [], "incomplete": [] }] });
    let mut s1 = BTreeMap::new();
    s1.insert("events.json".to_string(), events.to_string());
    s1.insert("measurements.json".to_string(), measurements.to_string());
    s1.insert("study.json".to_string(), study.to_string());
    s1.insert("run.json".to_string(), run.clone());
    let j = t1_judged();
    let judged_json = json!({ "verdict": j.verdict, "reason": j.reason, "qualifiers": j.qualifiers });
    let record = json!({ "_type": "https://in-toto.io/Statement/v1", "subject": subjects(&s1),
        "predicateType": "urn:embarch:predicate:study-run:v1",
        "predicate": { "schema": 1, "study_id": "s1", "kind": "host",
            "observed": { "behaviours": {}, "image_status": "unverified", "declarations": { "witnessed_since_utc_ms": 0, "entries": [] } },
            "claimed": { "told_by": "embarch-api", "build": null },
            "test": { "run_id": RUN, "test_id": "t1", "bench": "A", "definition_hash": "t1".repeat(32), "run_index": 1, "run_count": 1,
                      "run": judged_json, "verdict": judged_json, "note": null, "unit_key": UNIT, "prompts": [] } } });
    for (n, b) in &s1 {
        if n != "run.json" {
            f.insert(format!("studies/s1/{n}"), b.clone());
        }
    }
    f.insert("studies/s1/record.dsse.json".into(), envelope(k, &record));
    let study_record_sha = sha(f["studies/s1/record.dsse.json"].as_bytes());

    let run_result_verdict = if knobs.flip_result_run { "fail" } else { "pass" };
    let result = json!({ "run_id": RUN, "status": "completed", "ended_utc_ms": 5, "smoke_checks": [], "results": [{
        "test_id": "t1", "bench": "A", "verdict": run_result_verdict, "reason": if knobs.flip_result_run { json!("limit") } else { Value::Null },
        "qualifiers": j.qualifiers, "counts": serde_json::from_str::<Value>(COUNTS).unwrap(),
        "runs": [{ "verdict": run_result_verdict, "reason": if knobs.flip_result_run { json!("limit") } else { Value::Null } }],
        "study_ids": ["s1"], "note": null }] });
    f.insert("run.json".into(), run.clone());
    f.insert("result.json".into(), result.to_string());

    let mut tests = vec![
        json!({ "test_id": "t1", "bench": "A", "definition_hash": "t1".repeat(32), "requirements": ["R1"], "verdict": "pass", "reason": null,
                "note": null, "dut_image": "released", "study_records": [{ "study_id": "s1", "record_sha256": study_record_sha }] }),
        json!({ "test_id": "t2", "bench": "A", "definition_hash": "t2".repeat(32), "requirements": ["R2"], "verdict": null, "reason": "not_run",
                "note": null, "dut_image": null, "study_records": [] }),
    ];
    if knobs.drop_record_entry {
        tests.pop();
    }
    let test_record = json!({ "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "run.json", "digest": { "sha256": sha(run.as_bytes()) } },
                    { "name": "result.json", "digest": { "sha256": sha(f["result.json"].as_bytes()) } }],
        "predicateType": "urn:embarch:predicate:test-run:v1",
        "predicate": { "schema": 1, "run_id": RUN, "status": "completed", "reason": null, "ended_utc_ms": 5, "unit_key": UNIT,
            "selection": { "entries": [{ "test_id": "t1", "status": "selected", "reason": null }, { "test_id": "t2", "status": "selected", "reason": null },
                                       { "test_id": "t3", "status": "not_selected", "reason": "outside the plan" }] },
            "tests": tests, "observed": { "core": { "version": "0.0.0" }, "call_log": null } } });
    f.insert("test-run.dsse.json".into(), envelope(k, &test_record));
    let record_sha = sha(f["test-run.dsse.json"].as_bytes());

    // The run log: started by a person, both tests entered, the run ended.
    let (l0, h0) = log_line(json!({ "type": "test_run_started", "run_id": RUN, "run_hash": sha(run.as_bytes()), "scope": "person", "session": "x", "prev": GENESIS, "seq": 0 }));
    let (l1, h1) = log_line(json!({ "type": "test", "run_id": RUN, "test_id": "t1", "definition_hash": "t1".repeat(32), "requirements": ["R1"],
        "unit_key": UNIT, "dut_image": "released", "verdict": "pass", "prev": h0, "seq": 1 }));
    let (l2, h2) = log_line(json!({ "type": "test", "run_id": RUN, "test_id": "t2", "definition_hash": "t2".repeat(32), "requirements": ["R2"],
        "unit_key": UNIT, "dut_image": null, "verdict": null, "prev": h1, "seq": 2 }));
    let (l3, h3) = log_line(json!({ "type": "test_run", "run_id": RUN, "status": "completed", "record_sha256": record_sha,
        "study_records": [{ "study_id": "s1", "record_sha256": study_record_sha }], "prev": h2, "seq": 3 }));
    f.insert("run-log-segment.jsonl".into(), format!("{l0}\n{l1}\n{l2}\n{l3}\n"));
    let _ = h3;

    // report-data.json, as Core's `test_report_data` writes it.
    let rules = Rules::latest();
    let (jt1, jt2) = (t1_judged(), unreached());
    let mut r1 = serde_json::to_value(rules.requirement(std::slice::from_ref(&jt1)).unwrap()).unwrap();
    r1["tests"] = json!(["t1"]);
    r1["skipped_tests"] = json!([]);
    let mut r2 = serde_json::to_value(rules.requirement(std::slice::from_ref(&jt2)).unwrap()).unwrap();
    r2["tests"] = json!(["t2"]);
    r2["skipped_tests"] = json!([{ "test_id": "t2", "reason": "not_run" }]);
    let summary = rules.combine(&[jt1.clone(), jt2]).unwrap();
    let t1_verdict = if knobs.flip_report_verdict { "fail" } else { "pass" };
    let rd = json!({
        "schema": 1, "kind": "test_run", "run_id": RUN, "revision": 1, "run_status": "completed", "reason": null, "resumes": null,
        "suite": { "id": "usb-pd", "version": "1", "commit": null },
        "standard": { "name": "usb-pd", "revision": "3.2", "requirement_count": null,
                      "atlas": { "joined": false, "note": "no atlas was joined: the standard's own requirement count is absent" } },
        "unit": { "unit_key": UNIT, "identity_read": true, "declared_serial": "u1", "board_revision": "A", "declared": null },
        "person": null, "go_by": "person", "published_by": "agent", "statement": STATEMENT,
        "attestation": { "label": "person's acts not protected", "core_attested": false, "reason": "milestone 1" },
        "summary": summary, "requirements": { "R1": r1, "R2": r2 },
        "tests": [
            { "test_id": "t1", "bench": "A", "definition_hash": "t1".repeat(32), "requirements": ["R1"], "verdict": t1_verdict, "reason": null,
              "qualifiers": jt1.qualifiers, "counts": serde_json::from_str::<Value>(COUNTS).unwrap(), "runs": [{ "verdict": "pass", "reason": null }],
              "note": null, "dut_image": "released", "study_ids": ["s1"],
              "decision_rule": { "kind": "guard_band", "w_multiple": 1.0 },
              "limits": [{ "measurement": "vbus", "unit": "mV", "lower": 4500.0, "upper": 5500.0, "expect": null, "rule": "guard_band", "w_multiple": 1.0,
                           "u": 10.0, "k": 2.0, "u_declared": true, "w": if knobs.alter_limit_w { 11.0 } else { 10.0 } }] },
            { "test_id": "t2", "bench": "A", "definition_hash": "t2".repeat(32), "requirements": ["R2"], "verdict": null, "reason": "not_run",
              "qualifiers": [], "counts": null, "runs": [], "note": null, "dut_image": null, "study_ids": [],
              "decision_rule": { "kind": "simple_acceptance", "w_multiple": 0.0 }, "limits": [] },
        ],
        "manual_verdicts": [],
        "not_run": [{ "source": "run", "test_id": "t2", "requirements": ["R2"], "reason": "not_run" },
                    { "source": "selection", "test_id": "t3", "status": "not_selected", "reason": "outside the plan" }],
        "coverage": { "tests_in_run": 2, "tests_judged": 1, "tests_skipped": 0, "tests_not_reached": 1, "selection": { "not_selected": 1, "selected": 2 } },
        "runs_on_unit": [
            { "test_id": "t1", "requirements": ["R1"], "dut_image": "released", "definition_hash": "t1".repeat(32), "runs": [{ "run_id": RUN, "verdict": "pass", "log_index": 1 }] },
            { "test_id": "t2", "requirements": ["R2"], "dut_image": null, "definition_hash": "t2".repeat(32), "runs": [{ "run_id": RUN, "verdict": null, "log_index": 2 }] },
        ],
        "rules": rules.behaviours().iter().map(|(n, b)| json!({ "name": n, "behaviour": b })).collect::<Vec<_>>(),
        "core": { "version": "0.0.0", "keyid": sha(k.verifying_key().as_bytes()) },
        "record_sha256": record_sha, "study_ids": ["s1"],
    });
    f.insert("report-data.json".into(), rd.to_string());
    let st = json!({ "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "report-data.json", "digest": { "sha256": sha(rd.to_string().as_bytes()) } }],
        "predicateType": "urn:embarch:predicate:report-data:v1", "predicate": rd });
    f.insert("report-data.dsse.json".into(), envelope(k, &st));
    f
}

fn manifest_for(k: &SigningKey, core: &BTreeMap<String, String>, api: &[(String, String)]) -> String {
    let log = &core["run-log-segment.jsonl"];
    let head = serde_json::from_str::<Value>(log.lines().last().unwrap()).unwrap()["hash"].clone();
    let listed: Vec<Value> = api.iter().map(|(n, h)| json!({ "name": n, "sha256": h })).collect();
    let statement = json!({ "_type": "https://in-toto.io/Statement/v1", "subject": subjects(core),
        "predicateType": "urn:embarch:predicate:publish-manifest:v1",
        "predicate": { "schema": 1, "run_id": RUN, "revision": 1, "supersedes": null, "published_by": "agent",
            "published_utc_ms": 1, "core": { "version": "0.0.0" }, "run_log": { "head": head, "length": 4 },
            "record_sha256": sha(core["test-run.dsse.json"].as_bytes()),
            "report_data_sha256": sha(core["report-data.json"].as_bytes()), "embarch_api_files": listed } });
    envelope(k, &statement)
}

fn ok_text(b: &str) -> Behavior {
    Behavior::Reply { status: 200, reason: "OK".into(), content_type: "application/octet-stream".into(), body: b.into() }
}

fn router(files: &BTreeMap<String, String>, placeholder_manifest: &str) -> Behavior {
    let rd = &files["report-data.json"];
    let mut routes: Vec<(String, Behavior)> = vec![
        (format!("/test-runs/{RUN}"), Behavior::json_ok(json!({ "run_id": RUN, "state": "completed" }))),
        ("/report-data".into(), Behavior::json_ok(json!({ "revision": 1, "report_data_sha256": sha(rd.as_bytes()), "keyid": "k",
            "report_data": serde_json::from_str::<Value>(rd).unwrap() }))),
        ("/publish".into(), Behavior::json_ok(json!({ "revision": 1, "entry": {}, "keyid": "k" }))),
        ("/files/manifest.dsse.json".into(), ok_text(placeholder_manifest)),
    ];
    for (n, b) in files {
        routes.push((format!("/files/{n}"), ok_text(b)));
    }
    Behavior::Router { routes, otherwise: Box::new(Behavior::plain_text_error(404, "Not Found", "no such route in the mock")) }
}

fn unzip(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(f.name().unwrap().to_string(), b);
    }
    out
}

fn write_zip(path: &Path, files: &BTreeMap<String, Vec<u8>>) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (n, b) in files {
        w.start_file(n.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b).unwrap();
    }
    w.finish().unwrap();
}

fn tmp(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("embarch-verify-test-run-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A Test run's zip as `publish` writes it, its manifest then signed over the embarch-api files.
async fn published(k: &SigningKey, knobs: Knobs) -> BTreeMap<String, Vec<u8>> {
    let core = core_files(k, knobs);
    let mock = MockCore::start(router(&core, &manifest_for(k, &core, &[]))).await;
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": mock.base_url() })).unwrap();
    let dir = tmp("pub");
    let out = publish::run(
        &CoreClient::new(&cfg).unwrap(),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    let mut files = unzip(&out.zip);
    resign(k, &core, &mut files);
    files
}

/// The manifest signed again over the files as they now stand.
fn resign(k: &SigningKey, core: &BTreeMap<String, String>, files: &mut BTreeMap<String, Vec<u8>>) {
    let api: Vec<(String, String)> =
        files.iter().filter(|(n, _)| !core.contains_key(*n) && *n != "manifest.dsse.json").map(|(n, b)| (n.clone(), sha(b))).collect();
    files.insert("manifest.dsse.json".into(), manifest_for(k, core, &api).into_bytes());
}

fn check(files: &BTreeMap<String, Vec<u8>>, k: &SigningKey) -> Report {
    let path = tmp("check").join("z.zip");
    write_zip(&path, files);
    verify::run(&path, &Options { public_key_hex: &pub_hex(k), attested_fingerprint: None }).unwrap()
}

fn status(r: &Report, id: u8) -> Status {
    r.checks.iter().find(|c| c.id == id).unwrap().status
}

fn dump(r: &Report) -> String {
    r.to_text()
}

fn problems(r: &Report, id: u8) -> String {
    r.checks.iter().find(|c| c.id == id).unwrap().problems.join(" | ")
}

#[tokio::test]
async fn a_fixture_test_zip_passes_every_check() {
    let k = key(7);
    let files = published(&k, Knobs::default()).await;
    let r = check(&files, &k);
    assert!(r.ok(), "{}", dump(&r));
    assert!(r.complete(), "{}", dump(&r));
    // Check 3 recomputed through the shared crate; the exports and the report's claims are regenerated.
    for id in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        assert_eq!(status(&r, id), Status::Pass, "check {id}\n{}", dump(&r));
    }
    let c3 = r.checks.iter().find(|c| c.id == 3).unwrap();
    assert!(c3.compared.iter().any(|l| l.contains("1 runs recomputed through judge_run")), "{:?}", c3.compared);
    assert!(c3.compared.iter().any(|l| l.contains("rest on Core's signature")), "{:?}", c3.compared);
}

#[tokio::test]
async fn a_limit_whose_w_report_data_alters_fails_check_6_naming_the_test() {
    let k = key(7);
    let files = published(&k, Knobs { alter_limit_w: true, ..Knobs::default() }).await;
    let r = check(&files, &k);
    assert_eq!(status(&r, 6), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 6).contains("test t1: report-data.json's limits"), "{}", problems(&r, 6));
    assert_eq!(status(&r, 3), Status::Pass, "{}", dump(&r));
}

#[tokio::test]
async fn the_test_pdf_prints_each_limits_rule_w_and_u() {
    let k = key(7);
    let files = published(&k, Knobs::default()).await;
    let typ = String::from_utf8_lossy(&files["report.typ"]).into_owned();
    assert!(typ.contains("guard_band, w multiple 1"), "{typ}");
    assert!(typ.contains("4500 to 5500 mV"), "{typ}");
    assert!(typ.contains("Test t2 has no limit."), "{typ}");
    assert!(typ.contains("\"20\""), "U is k times u: {typ}");
    assert!(!typ.contains("not carried"), "{typ}");
}

#[tokio::test]
async fn a_verdict_flipped_in_report_data_fails_check_3_naming_the_test() {
    let k = key(7);
    let files = published(&k, Knobs { flip_report_verdict: true, ..Knobs::default() }).await;
    let r = check(&files, &k);
    assert_eq!(status(&r, 3), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 3).contains("test t1: report-data.json's verdict"), "{}", problems(&r, 3));
    assert_eq!(status(&r, 6), Status::Fail, "{}", dump(&r));
}

#[tokio::test]
async fn a_test_the_record_dropped_fails_check_3_naming_it() {
    let k = key(7);
    let files = published(&k, Knobs { drop_record_entry: true, ..Knobs::default() }).await;
    let r = check(&files, &k);
    assert_eq!(status(&r, 3), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 3).contains("test t2: run.json selected it and the signed Test run's record has no entry for it"), "{}", problems(&r, 3));
}

#[tokio::test]
async fn a_limit_in_run_json_that_the_verdicts_were_not_judged_against_fails_check_3_naming_the_test() {
    let k = key(7);
    let files = published(&k, Knobs { run_limit: Some((5100.0, 5500.0)), ..Knobs::default() }).await;
    let r = check(&files, &k);
    assert_eq!(status(&r, 3), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 3).contains("test t1: study s1's run recomputes to fail"), "{}", problems(&r, 3));
}

#[tokio::test]
async fn a_run_verdict_result_json_changed_against_its_study_record_fails_check_3() {
    let k = key(7);
    let files = published(&k, Knobs { flip_result_run: true, ..Knobs::default() }).await;
    let r = check(&files, &k);
    assert_eq!(status(&r, 3), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 3).contains("test t1: study s1's record says its run was"), "{}", problems(&r, 3));
}

#[tokio::test]
async fn a_zip_altered_after_signing_fails_the_check_that_names_it() {
    let k = key(7);
    let files = published(&k, Knobs::default()).await;

    let mut f = files.clone();
    let text = String::from_utf8(f["run.json"].clone()).unwrap().replace("4500", "5100");
    f.insert("run.json".into(), text.into_bytes());
    let r = check(&f, &k);
    assert_eq!(status(&r, 0), Status::Fail, "{}", dump(&r));
    assert!(problems(&r, 0).contains("run.json does not match the digest"), "{}", problems(&r, 0));

    let mut f = files.clone();
    f.remove("studies/s1/events.json");
    let r = check(&f, &k);
    assert_eq!(status(&r, 0), Status::Fail, "{}", dump(&r));

    let mut f = files.clone();
    let junit = String::from_utf8(f["exports/junit.xml"].clone()).unwrap().replace("t1", "tX");
    f.insert("exports/junit.xml".into(), junit.into_bytes());
    let core = core_files(&k, Knobs::default());
    resign(&k, &core, &mut f);
    let r = check(&f, &k);
    assert_eq!(status(&r, 7), Status::Fail, "{}", dump(&r));
}

#[tokio::test]
async fn an_interrupted_run_is_named_not_checked_by_this_version_and_not_failed() {
    let k = key(7);
    let mut core = core_files(&k, Knobs::default());
    // A record of an interrupted run: rebuilt signed, so only the shape check can speak.
    let env: Value = serde_json::from_str(&core["test-run.dsse.json"]).unwrap();
    let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap()).unwrap();
    let mut st: Value = serde_json::from_slice(&payload).unwrap();
    st["predicate"]["status"] = json!("interrupted");
    core.insert("test-run.dsse.json".into(), envelope(&k, &st));
    let mock = MockCore::start(router(&core, &manifest_for(&k, &core, &[]))).await;
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": mock.base_url() })).unwrap();
    let out = publish::run(
        &CoreClient::new(&cfg).unwrap(),
        Some(&tmp("pub")),
        &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    let mut files = unzip(&out.zip);
    resign(&k, &core, &mut files);
    let r = check(&files, &k);
    for id in [3u8, 5, 6] {
        assert_eq!(status(&r, id), Status::NotChecked, "check {id}\n{}", dump(&r));
    }
    assert!(problems(&r, 3).contains("interrupted"), "{}", problems(&r, 3));
}
