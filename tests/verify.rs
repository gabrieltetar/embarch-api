//! M1.11, `verify` (`embarch-api` decisions 118 to 121): a zip `publish` wrote
//! against a mock Core is re-signed with a real Ed25519 key (the mock cannot
//! sign, and `verify` needs a manifest naming embarch-api's hashes), then
//! checked, and tampered one way at a time.
#![cfg(feature = "pdf")]

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use embarch_api::publish::{self, PublishRequest};
use embarch_api::verify::{self, Options, Report, Status};
use embarch_core_client::{CoreClient, CoreConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{Behavior, MockCore};

const RUN: &str = "0123456789abcdef0123456789abcdef";
const NOW: i64 = 1_763_700_000;
const NOTE: &str = "the person said: ok";
const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const CSV: &str = "t_s,value,core_rx_utc_ms\n0,1.0,1000\n0.01,3.0,1010\n";

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

/// Core's run files, signed by `k`, with a run log of two entries. The manifest
/// is the placeholder `publish` is served; `resign` replaces it.
fn core_files(k: &SigningKey) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("study.json".into(), json!({
        "name": "GATT rate / loss",
        "streams": [{ "id": 0, "name": "rate", "encoding": { "Samples": { "unit": "Volts" } } }],
        "measurements": [{ "name": "avg", "reducer": "mean", "tap": 0 }],
    }).to_string());
    f.insert("events.json".into(), json!({ "study_name": "GATT rate / loss", "steps": [
        { "step_name": "connect", "outcome": "Pass", "started_utc_ms": 1000, "ended_utc_ms": 2000 },
        { "step_name": "read", "outcome": { "Fail": { "reason": "no reply" } } },
    ] }).to_string());
    f.insert("streams/rate.bin".into(), "raw-bytes".into());
    f.insert("streams/rate.csv".into(), CSV.into());
    f.insert("measurements.json".into(), json!({
        "schema": 1, "run_id": RUN, "behaviours": { "mean": 1 },
        "measurements": [{ "name": "avg", "value": 2.0, "unit": "V", "at_utc_ms": 1010,
            "source": { "file": "streams/rate.csv", "rows": [0, 1] }, "by": "bench", "instrument": null,
            "bench_image": "unverified",
            "method": { "kind": "reducer", "name": "mean", "behaviour": 1, "window": {} },
            "resolution": null, "uncertainty": null, "u_source": "undeclared", "declared_by": [], "incomplete": [] }],
    }).to_string());
    let record = json!({ "_type": "https://in-toto.io/Statement/v1", "subject": subjects(&f),
        "predicateType": "urn:embarch:predicate:study-run:v1",
        "predicate": { "schema": 1, "study_id": RUN, "observed": { "behaviours": { "mean": 1 },
            "declarations": { "witnessed_since_utc_ms": 0, "entries": [] } } } });
    f.insert("record.dsse.json".into(), envelope(k, &record));

    let (run_line, run_hash) = log_line(json!({ "type": "run", "study_id": RUN, "kind": "device", "status": "completed",
        "clean": false, "record_sha256": sha(f["record.dsse.json"].as_bytes()), "go_by": "agent", "prev": GENESIS, "seq": 0,
        "captures": [{ "name": "rate.bin", "sha256": sha(b"raw-bytes") }, { "name": "rate.csv", "sha256": sha(CSV.as_bytes()) }] }));
    let (note_line, _) = log_line(json!({ "type": "adopted", "study_id": RUN, "kind": "note",
        "name": "notes/operator-notes.txt", "text_sha256": sha(NOTE.as_bytes()), "scope": "agent", "at_utc_ms": 1,
        "prev": run_hash, "seq": 1 }));
    f.insert("run-log-segment.jsonl".into(), format!("{run_line}\n{note_line}\n"));

    let rd = json!({
        "schema": 1, "study_id": RUN, "kind": "device", "revision": 1, "run_status": "step_failed",
        "steps": { "total": 2, "pass": 1, "fail": 1, "timed_out": 0, "other": 0 },
        "subject": { "study_name": "GATT rate / loss", "provenance": "declared by the study" },
        "attestation": { "label": "agent-scoped only", "core_attested": false, "reason": "x" },
        "go_by": "agent", "published_by": "agent",
        "adopted": [{ "kind": "note", "name": "notes/operator-notes.txt", "text_sha256": sha(NOTE.as_bytes()),
                      "scope": "agent", "attribution": "relayed by the agent", "at_utc_ms": 1 }],
        "observed": { "image_status": "unverified", "declarations": { "witnessed_since_utc_ms": 0, "entries": [] } },
        "claimed": { "build": null }, "core": { "version": "0.0.0", "keyid": sha(k.verifying_key().as_bytes()) },
        "record_sha256": sha(f["record.dsse.json"].as_bytes()),
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
        "predicate": { "schema": 1, "study_id": RUN, "revision": 1, "supersedes": null, "published_by": "agent",
            "published_utc_ms": 1, "core": { "version": "0.0.0" }, "run_log": { "head": head, "length": 2 },
            "record_sha256": sha(core["record.dsse.json"].as_bytes()),
            "report_data_sha256": sha(core["report-data.json"].as_bytes()), "embarch_api_files": listed } });
    envelope(k, &statement)
}

fn ok_text(b: &str) -> Behavior {
    Behavior::Reply { status: 200, reason: "OK".into(), content_type: "application/octet-stream".into(), body: b.into() }
}

fn router(files: &BTreeMap<String, String>, placeholder_manifest: &str) -> Behavior {
    let rd = &files["report-data.json"];
    let mut routes: Vec<(String, Behavior)> = vec![
        ("/adopt".into(), Behavior::json_ok(json!({ "entry": {}, "hash": "h" }))),
        ("/report-data".into(), Behavior::json_ok(json!({ "revision": 1, "report_data_sha256": sha(rd.as_bytes()),
            "keyid": "k", "report_data": serde_json::from_str::<Value>(rd).unwrap() }))),
        ("/publish".into(), Behavior::json_ok(json!({ "revision": 1, "entry": {}, "keyid": "k" }))),
        ("/streams".into(), Behavior::json_ok(json!({ "streams": [{ "id": 0, "name": "rate", "encoding": "Raw", "rendered": true }] }))),
        ("/stream/rate".into(), ok_text(CSV)),
        ("/steps".into(), Behavior::json_ok(json!({ "study_name": "GATT rate / loss", "timed": true, "steps": [
            { "index": 0, "step_name": "connect", "outcome": "Pass", "started_utc_ms": 1000, "ended_utc_ms": 2000 },
            { "index": 1, "step_name": "read", "outcome": "Fail", "reason": "no reply" } ] }))),
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
    let d = std::env::temp_dir().join(format!("embarch-verify-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A zip `publish` wrote, its manifest then signed over the hashes of the
/// embarch-api files it holds. Returns the files.
async fn published(k: &SigningKey) -> BTreeMap<String, Vec<u8>> {
    let core = core_files(k);
    let mock = MockCore::start(router(&core, &manifest_for(k, &core, &[]))).await;
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": mock.base_url() })).unwrap();
    let dir = tmp("pub");
    let out = publish::run(
        &CoreClient::new(&cfg).unwrap(),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: Some(NOTE), report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let mut files = unzip(&out.zip);
    let api: Vec<(String, String)> =
        files.iter().filter(|(n, _)| !core.contains_key(*n) && *n != "manifest.dsse.json").map(|(n, b)| (n.clone(), sha(b))).collect();
    files.insert("manifest.dsse.json".into(), manifest_for(k, &core, &api).into_bytes());
    files
}

fn check(files: &BTreeMap<String, Vec<u8>>, tag: &str, public_key: &str, attested: Option<&str>) -> Report {
    let path = tmp(tag).join("z.zip");
    write_zip(&path, files);
    verify::run(&path, &Options { public_key_hex: public_key, attested_fingerprint: attested }).unwrap()
}

fn status(r: &Report, id: u8) -> Status {
    r.checks.iter().find(|c| c.id == id).unwrap().status
}

#[tokio::test]
async fn a_published_zip_verifies_and_a_tampered_copy_fails_the_check_that_names_it() {
    let k = key(7);
    let files = published(&k).await;
    let fp = sha(k.verifying_key().as_bytes());

    // The untampered zip, unattested then attested.
    let r = check(&files, "ok", &pub_hex(&k), None);
    assert!(r.ok(), "{}", r.to_text());
    for id in [0, 1, 5, 7, 8] {
        assert_eq!(status(&r, id), Status::Pass, "check {id}\n{}", r.to_text());
    }
    assert_eq!(status(&r, 2), Status::Partial, "step outcomes are not re-evaluated");
    assert_eq!(status(&r, 3), Status::NotApplicable);
    for id in [4, 6, 9] {
        assert_eq!(status(&r, id), Status::NotChecked);
    }
    assert!(!r.complete());
    assert!(!r.attested && r.to_text().contains("origin unattested") && r.to_text().contains("integrity only"));
    assert!(r.to_text().contains("recomputes to 2.0"), "{}", r.to_text());
    assert!(r.produced_by.is_some());
    assert!(check(&files, "att", &pub_hex(&k), Some(&fp)).attested);

    // A key that is not the signer's.
    let r = check(&files, "key", &pub_hex(&key(9)), None);
    assert_eq!(status(&r, 0), Status::Fail);
    assert!(!r.ok());

    // A changed number in measurements.json.
    let mut f = files.clone();
    let m = String::from_utf8(f["measurements.json"].clone()).unwrap().replace("2.0", "2.5");
    f.insert("measurements.json".into(), m.into_bytes());
    let r = check(&f, "num", &pub_hex(&k), None);
    assert!(!r.ok());
    assert_eq!(status(&r, 1), Status::Fail);
    assert_eq!(status(&r, 2), Status::Fail);

    // A forged attestation label.
    let mut f = files.clone();
    let rd = String::from_utf8(f["report-data.json"].clone()).unwrap().replace("agent-scoped only", "passkey");
    f.insert("report-data.json".into(), rd.into_bytes());
    let r = check(&f, "label", &pub_hex(&k), None);
    assert_eq!(status(&r, 5), Status::Fail, "{}", r.to_text());

    // An edited note, and an edited prose file nobody adopted.
    let mut f = files.clone();
    f.insert("notes/operator-notes.txt".into(), b"the person said: no".to_vec());
    let r = check(&f, "note", &pub_hex(&k), None);
    assert_eq!(status(&r, 5), Status::Fail, "{}", r.to_text());
    let mut f = files.clone();
    f.insert("prose/extra.md".into(), b"hello".to_vec());
    assert_eq!(status(&check(&f, "prose", &pub_hex(&k), None), 5), Status::Fail);

    // A dropped run-log entry.
    let mut f = files.clone();
    let seg = String::from_utf8(f["run-log-segment.jsonl"].clone()).unwrap();
    let kept: String = seg.lines().take(1).map(|l| format!("{l}\n")).collect();
    f.insert("run-log-segment.jsonl".into(), kept.into_bytes());
    let r = check(&f, "log", &pub_hex(&k), None);
    assert_eq!(status(&r, 8), Status::Fail, "{}", r.to_text());
}
