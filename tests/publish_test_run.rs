//! `publish` of a Test run: one zip over the whole run, built through Core's
//! three Test routes against a mock Core (`embarch-api` decision 144;
//! `embarch-core` decisions 148 to 150). Nothing here reaches a real Core.
#![cfg(feature = "pdf")]

mod support;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use embarch_api::publish::{self, PublishRequest};
use embarch_api::report_pdf::{report_text, RenderOptions, Staging};
use embarch_core_client::{CoreClient, CoreConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{Behavior, MockCore};

const RUN: &str = "fedcba9876543210fedcba9876543210";
const NOW: i64 = 1_763_700_000;
const STATEMENT: &str = "This report is pre-compliance evidence. It is not a certification and not an accredited test report.";

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn client(base: &str) -> CoreClient {
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": base })).unwrap();
    CoreClient::new(&cfg).unwrap()
}

/// R1 is `not_verified` because T2 was skipped for a gap; R2 passes with T1,
/// which failed once on the unit before it passed.
fn report_data() -> String {
    json!({
        "schema": 1, "run_id": RUN, "kind": "test_run", "revision": 1, "run_status": "completed", "reason": null,
        "resumes": null, "suite": { "id": "usb-pd", "version": "1.2", "commit": "abc123" },
        "standard": { "name": "USB PD", "revision": "R3.2", "requirement_count": null, "atlas": { "joined": false, "note": "none" } },
        "unit": { "unit_key": "hw-42", "identity_read": true, "declared_serial": "SN7", "board_revision": "B", "declared": true },
        "person": "A. Person", "go_by": "person", "published_by": "agent", "statement": STATEMENT,
        "attestation": { "label": "agent-scoped only", "core_attested": false, "reason": "no person act" },
        "summary": { "verdict": "not_verified", "counts": { "pass": 1 }, "qualifiers": [] },
        "requirements": {
            "R1": { "verdict": "not_verified", "counts": {}, "qualifiers": [], "n_of_m": null, "tests": ["T2"],
                    "skipped_tests": [{ "test_id": "T2", "reason": "requires_not_met" }] },
            "R2": { "verdict": "pass", "counts": { "pass": 1 }, "qualifiers": ["bench_firmware_unreleased", "n_of_m_run"], "n_of_m": { "n": 3, "m": 5 }, "tests": ["T1"], "skipped_tests": [] },
        },
        "tests": [
            { "test_id": "T1", "verdict": "pass", "reason": null, "qualifiers": [], "study_ids": ["s1"], "dut_image": "img1" },
            { "test_id": "T2", "verdict": "skipped", "reason": "requires_not_met", "qualifiers": [], "study_ids": [], "dut_image": "img1" },
        ],
        "manual_verdicts": [],
        "not_run": [{ "source": "run", "test_id": "T2", "reason": "requires_not_met" }],
        "coverage": { "tests_in_run": 2 },
        "runs_on_unit": [{ "test_id": "T1", "requirements": ["R2"], "dut_image": "img1", "definition_hash": "dh",
            "runs": [{ "run_id": "earlier-failed-run", "verdict": "fail", "log_index": 4 }, { "run_id": RUN, "verdict": "pass", "log_index": 9 }] }],
        "rules": [{ "name": "worst_of", "behaviour": "the worst verdict among the tests that ran" }],
        "core": { "version": "0.0.0", "keyid": "k" }, "record_sha256": sha(b"record"), "study_ids": ["s1"],
    })
    .to_string()
}

fn core_files() -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("run.json".to_string(), r#"{"plan_hash":"plan-h1"}"#.to_string());
    f.insert("result.json".to_string(), r#"{"tests":[]}"#.to_string());
    f.insert("test-run.dsse.json".to_string(), r#"{"payloadType":"x"}"#.to_string());
    f.insert("studies/s1/events.json".to_string(), r#"{"steps":[]}"#.to_string());
    f.insert("studies/s1/study.json".to_string(), r#"{"name":"t1"}"#.to_string());
    f.insert("studies/s1/streams/rate.bin".to_string(), "raw".to_string());
    f.insert("report-data.json".to_string(), report_data());
    f.insert("report-data.dsse.json".to_string(), r#"{"payloadType":"rd"}"#.to_string());
    f.insert("run-log-segment.jsonl".to_string(), "{\"n\":0}\n".to_string());
    f
}

fn manifest(files: &BTreeMap<String, String>) -> String {
    let subject: Vec<Value> =
        files.iter().map(|(n, b)| json!({ "name": n, "digest": { "sha256": sha(b.as_bytes()) } })).collect();
    let statement = json!({ "subject": subject, "predicateType": "urn:embarch:predicate:publish-manifest:v1" });
    let payload = base64::engine::general_purpose::STANDARD.encode(statement.to_string());
    json!({ "payloadType": "application/vnd.in-toto+json", "payload": payload, "signatures": [] }).to_string()
}

fn ok_text(body: &str) -> Behavior {
    Behavior::Reply { status: 200, reason: "OK".into(), content_type: "application/octet-stream".into(), body: body.into() }
}

fn router(files: &BTreeMap<String, String>) -> Behavior {
    let rd = &files["report-data.json"];
    let mut routes: Vec<(String, Behavior)> = vec![
        (format!("/test-runs/{RUN}"), Behavior::json_ok(json!({ "run_id": RUN, "state": "ended" }))),
        (
            "/report-data".into(),
            Behavior::json_ok(json!({
                "revision": 1, "report_data_sha256": sha(rd.as_bytes()), "keyid": "k",
                "report_data": serde_json::from_str::<Value>(rd).unwrap(),
            })),
        ),
        ("/publish".into(), Behavior::json_ok(json!({ "revision": 1, "entry": {}, "keyid": "k" }))),
        ("/files/manifest.dsse.json".into(), ok_text(&manifest(files))),
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

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("embarch-publish-test-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn publish_fixture(tag: &str) -> (publish::PublishOutcome, BTreeMap<String, Vec<u8>>, BTreeMap<String, String>, MockCore) {
    let files = core_files();
    let mock = MockCore::start(router(&files)).await;
    let dir = tmp(tag);
    let out = publish::run(
        &client(mock.base_url()),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    let zip = unzip(&out.zip);
    (out, zip, files, mock)
}

#[tokio::test]
async fn a_fixture_test_run_publishes_one_zip_holding_every_study_runs_files_and_the_signed_records() {
    let (out, zip, files, mock) = publish_fixture("zip").await;
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.zip.file_name().unwrap(), "2025-11-21-usb-pd-fedcba98.zip");
    for name in files.keys() {
        assert_eq!(zip[name], files[name].as_bytes(), "{name}");
    }
    for name in ["manifest.dsse.json", "report.pdf", "report.typ", "appendix.typ"] {
        assert!(zip.contains_key(name), "the zip lacks {name}: {:?}", zip.keys().collect::<Vec<_>>());
    }
    // The hashes sent to Core are the zip's own, and none of Core's files is claimed as embarch-api's.
    let reqs = mock.requests();
    let p = reqs.iter().find(|r| r.method == "POST" && r.path().ends_with("/publish")).unwrap();
    assert!(p.path().starts_with(&format!("/test-runs/{RUN}/")), "{}", p.path());
    let body: Value = serde_json::from_slice(&p.body).unwrap();
    let sent = body["files"].as_array().unwrap();
    assert_eq!(sent.len(), 3);
    for f in sent {
        let name = f["name"].as_str().unwrap();
        assert_eq!(sha(&zip[name]), f["sha256"].as_str().unwrap(), "{name}");
    }
}

#[tokio::test]
async fn the_pdf_names_a_not_verified_requirement_the_gap_behind_it_the_statement_and_an_earlier_failed_run() {
    let (out, zip, _files, _mock) = publish_fixture("pdf").await;
    assert!(zip["report.pdf"].starts_with(b"%PDF-"));
    let mut s = Staging::new().unwrap();
    for name in ["report.typ", "appendix.typ", "report-data.json"] {
        s.stage_bytes(name, &zip[name]).unwrap();
    }
    let text = report_text(&s, &RenderOptions { ident: format!("{RUN}-r{}", out.revision), timestamp_unix: NOW })
        .unwrap()
        .concat()
        .concat();
    for needle in [
        "not_verified", "requires_not_met", "T2",
        "pass, bench firmware unreleased, 3 of 5 tests run",
        "pre-compliance evidence", "not a certification", "not an accredited test report",
        "earlier-failed-run", "fail", "hw-42", "plan-h1", "Audit appendix",
    ] {
        assert!(text.contains(needle), "the PDF lacks {needle:?}: {text}");
    }
}

#[tokio::test]
async fn a_test_run_takes_no_notes_prose_or_report_and_is_refused_before_core_hears_of_it() {
    let files = core_files();
    let mock = MockCore::start(router(&files)).await;
    let dir = tmp("refuse");
    let err = publish::run(
        &client(mock.base_url()),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: Some("words"), report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("takes no notes"), "{err:#}");
    assert!(!mock.requests().iter().any(|r| r.method == "POST"));
}
