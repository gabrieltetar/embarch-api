//! M1.10: `publish` turns a finished Study run into one zip, built through
//! Core's publish routes, against a mock Core (`embarch-api` decisions 112,
//! 113; `embarch-core/interfaces/publish.md`). Nothing here reaches a real Core.
#![cfg(feature = "pdf")]

mod support;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use embarch_api::publish::{self, PublishRequest, NO_FOLDER_MESSAGE};
use embarch_api::report_pdf::{render_report, RenderOptions, Staging};
use embarch_core_client::{CoreClient, CoreConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{Behavior, MockCore};

const RUN: &str = "0123456789abcdef0123456789abcdef";
const NOW: i64 = 1_763_700_000;

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn client(base: &str) -> CoreClient {
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": base })).unwrap();
    CoreClient::new(&cfg).unwrap()
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/report_pdf").join(name)).unwrap()
}

/// `extra` are more `adopted` rows (prose blocks Core recorded).
fn report_data_rev(revision: u64, extra: &[Value]) -> String {
    let mut adopted = vec![json!({ "kind": "note", "name": "notes/operator-notes.txt", "text_sha256": sha(b"the person said: ok"),
                       "scope": "agent", "attribution": "relayed by the agent", "at_utc_ms": 1 })];
    adopted.extend_from_slice(extra);
    json!({
        "schema": 1, "study_id": RUN, "kind": "device", "revision": revision, "run_status": "step_failed",
        "steps": { "total": 2, "pass": 1, "fail": 1, "timed_out": 0, "other": 0 },
        "subject": { "study_name": "GATT rate / loss", "provenance": "declared by the study" },
        "attestation": { "label": "agent-scoped only", "core_attested": false, "reason": "an unattested Core" },
        "go_by": "agent", "published_by": "agent",
        "adopted": adopted,
        "observed": { "image_status": "unverified", "declarations": [] },
        "claimed": { "build": null }, "core": { "version": "0.0.0", "keyid": "k" },
        "record_sha256": sha(b"record"),
    })
    .to_string()
}

/// Core's files for the fixture run, by the name the zip gives them.
fn core_files() -> BTreeMap<String, String> {
    core_files_rev(1, &[])
}

fn core_files_rev(revision: u64, extra: &[Value]) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("events.json".to_string(), r#"{"steps":[]}"#.to_string());
    f.insert("measurements.json".to_string(), fixture("measurements.json"));
    f.insert("study.json".to_string(), r#"{"name":"GATT rate / loss"}"#.to_string());
    f.insert("record.dsse.json".to_string(), r#"{"payloadType":"x"}"#.to_string());
    f.insert("streams/rate.bin".to_string(), "raw-bytes-of-the-rate-tap".to_string());
    f.insert("streams/rate.csv".to_string(), fixture("capture.csv"));
    f.insert("streams/console.txt".to_string(), "boot\nconnected\n".to_string());
    f.insert("report-data.json".to_string(), report_data_rev(revision, extra));
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

fn router(files: &BTreeMap<String, String>, measurements: Behavior) -> Behavior {
    router_rev(1, files, measurements)
}

fn router_rev(revision: u64, files: &BTreeMap<String, String>, measurements: Behavior) -> Behavior {
    let rd = &files["report-data.json"];
    let mut routes: Vec<(String, Behavior)> = vec![
        ("/adopt".into(), Behavior::json_ok(json!({ "entry": {}, "hash": "h" }))),
        (
            "/report-data".into(),
            Behavior::json_ok(json!({
                "revision": revision, "report_data_sha256": sha(rd.as_bytes()), "keyid": "k",
                "report_data": serde_json::from_str::<Value>(rd).unwrap(),
            })),
        ),
        ("/publish".into(), Behavior::json_ok(json!({ "revision": revision, "entry": {}, "keyid": "k" }))),
        (
            "/streams".into(),
            Behavior::json_ok(json!({ "streams": [
                { "id": 0, "name": "rate", "encoding": "Raw", "rendered": true },
                { "id": 1, "name": "console", "encoding": "Text", "rendered": true },
            ] })),
        ),
        ("/stream/rate".into(), ok_text(&files["streams/rate.csv"])),
        ("/stream/console".into(), ok_text(&files["streams/console.txt"])),
        (
            "/steps".into(),
            Behavior::json_ok(json!({ "study_name": "GATT rate / loss", "timed": true, "steps": [
                { "index": 0, "step_name": "connect", "outcome": "Pass", "started_utc_ms": 1000, "ended_utc_ms": 2000 },
                { "index": 1, "step_name": "read", "outcome": "Fail", "reason": "no reply" },
            ] })),
        ),
        ("/files/manifest.dsse.json".into(), ok_text(&manifest(files))),
        ("/files/measurements.json".into(), measurements),
    ];
    for (n, b) in files {
        if n != "measurements.json" {
            routes.push((format!("/files/{n}"), ok_text(b)));
        }
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

fn study_report() -> String {
    fixture("report.typ").replace("capture.csv", "streams/rate.csv")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("embarch-publish-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const EVERY_FILE_A_FIXTURE_RUN_HOLDS: &[&str] = &[
    "report.pdf", "study.json", "events.json", "measurements.json", "record.dsse.json", "report-data.json",
    "report-data.dsse.json", "manifest.dsse.json", "run-log-segment.jsonl", "streams/rate.bin", "streams/rate.csv",
    "streams/console.txt", "exports/junit.xml", "exports/ocp-tv.jsonl", "report.typ", "appendix.typ",
    "notes/operator-notes.txt", "typst-packages/preview/lilaq/0.6.0/typst.toml",
];

#[tokio::test]
async fn a_fixture_run_publishes_one_zip_and_every_api_hash_matches_what_core_was_told() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("full");
    let report = dir.join("study-report.typ");
    std::fs::write(&report, study_report()).unwrap();
    let out = publish::run(
        &client(mock.base_url()),
        Some(&dir.join("zips")),
        &PublishRequest { run_id: RUN, notes: Some("the person said: ok"), report_typ: Some(&report), prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.zip.file_name().unwrap(), "2025-11-21-gatt-rate-loss-01234567.zip");
    let zip = unzip(&out.zip);
    for name in EVERY_FILE_A_FIXTURE_RUN_HOLDS {
        assert!(zip.contains_key(*name), "the zip lacks {name}: {:?}", zip.keys().collect::<Vec<_>>());
    }

    // The hashes sent to Core's publish route are the zip's own.
    let reqs = mock.requests();
    let publish_req = reqs.iter().find(|r| r.method == "POST" && r.path().ends_with("/publish")).unwrap();
    let body: Value = serde_json::from_slice(&publish_req.body).unwrap();
    let sent = body["files"].as_array().unwrap();
    assert!(sent.len() > 6);
    for f in sent {
        let name = f["name"].as_str().unwrap();
        assert_eq!(sha(&zip[name]), f["sha256"].as_str().unwrap(), "{name}");
    }
    // And none of Core's files is claimed as embarch-api's.
    for core_name in ["events.json", "measurements.json", "report-data.json", "streams/rate.csv"] {
        assert!(!sent.iter().any(|f| f["name"] == core_name), "{core_name}");
    }
    // The note went to Core before report-data, as text, under its name.
    let adopt = reqs.iter().position(|r| r.path().ends_with("/adopt")).unwrap();
    let rdata = reqs.iter().position(|r| r.path().ends_with("/report-data")).unwrap();
    assert!(adopt < rdata);
    let a: Value = serde_json::from_slice(&reqs[adopt].body).unwrap();
    assert_eq!(a, json!({ "kind": "note", "name": "notes/operator-notes.txt", "text": "the person said: ok" }));
    // Core's bytes are carried untouched.
    assert_eq!(zip["events.json"], files["events.json"].as_bytes());
    assert_eq!(zip["streams/rate.bin"], files["streams/rate.bin"].as_bytes());
}

#[tokio::test]
async fn the_pdf_rebuilds_byte_for_byte_from_the_zip_alone() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("rebuild");
    let report = dir.join("r.typ");
    std::fs::write(&report, study_report()).unwrap();
    let out = publish::run(
        &client(mock.base_url()),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: None, report_typ: Some(&report), prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    let zip = unzip(&out.zip);
    let mut s = Staging::new().unwrap();
    for name in ["report.typ", "appendix.typ", "report-data.json", "measurements.json", "streams/rate.csv", "streams/console.txt"] {
        s.stage_bytes(name, &zip[name]).unwrap();
    }
    let again = render_report(&s, &RenderOptions { ident: format!("{RUN}-r1"), timestamp_unix: NOW }).unwrap();
    assert_eq!(again, zip["report.pdf"]);
}

#[tokio::test]
async fn a_study_with_no_report_typ_publishes_with_the_appendix_and_a_data_section() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("noreport");
    let out = publish::run(
        &client(mock.base_url()),
        Some(&dir),
        &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] },
        NOW,
    )
    .await
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let zip = unzip(&out.zip);
    assert!(zip["report.pdf"].starts_with(b"%PDF-"));
    let typ = String::from_utf8(zip["report.typ"].clone()).unwrap();
    assert!(typ.contains("\"Data\"") && typ.contains("streams/rate.csv") && typ.contains("rate"), "{typ}");
    let appendix = String::from_utf8(zip["appendix.typ"].clone()).unwrap();
    assert!(appendix.contains("Audit appendix") && appendix.contains("agent-scoped only"), "{appendix}");
    // No report.typ, so no Typst package is carried; no note, so no note file.
    assert!(!zip.keys().any(|k| k.starts_with("typst-packages/")));
    assert!(!zip.contains_key("notes/operator-notes.txt"));
    // The text of the PDF holds the appendix and the data section.
    let mut s = Staging::new().unwrap();
    for name in ["report.typ", "appendix.typ", "report-data.json", "measurements.json", "streams/rate.csv", "streams/console.txt"] {
        s.stage_bytes(name, &zip[name]).unwrap();
    }
    let pages = embarch_api::report_pdf::report_text(&s, &RenderOptions { ident: format!("{RUN}-r1"), timestamp_unix: NOW }).unwrap();
    let all = pages.iter().flatten().cloned().collect::<Vec<_>>().join(" ");
    assert!(all.contains("Audit appendix") && all.contains("Data") && all.contains("Captures"), "{all}");
}

#[tokio::test]
async fn publish_refuses_with_no_folder_and_says_how_to_set_one() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let err = publish::run(&client(mock.base_url()), None, &PublishRequest { run_id: RUN, notes: Some("n"), report_typ: None, prose: &[] }, NOW)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert_eq!(msg, NO_FOLDER_MESSAGE);
    assert!(msg.contains("[publish]") && msg.contains("folder ="), "{msg}");
    // Refused before Core was asked anything, so no note was adopted.
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn a_publish_is_never_overwritten() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("again");
    let req = PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] };
    let first = publish::run(&client(mock.base_url()), Some(&dir), &req, NOW).await.unwrap();
    // The same revision again (the mock answers 1 twice): never overwritten.
    let err = publish::run(&client(mock.base_url()), Some(&dir), &req, NOW).await.unwrap_err();
    assert!(format!("{err:#}").contains("never overwritten"), "{err:#}");
    assert!(first.zip.exists());
}

#[tokio::test]
async fn measurements_core_does_not_serve_before_publish_are_a_warning_not_a_silent_gap() {
    let files = core_files();
    // 404 the first time (before the manifest exists), served after.
    let late = Behavior::Sequence(vec![Behavior::plain_text_error(404, "Not Found", "no file yet"), ok_text(&files["measurements.json"])]);
    let mock = MockCore::start(router(&files, late)).await;
    let dir = tmp("warn");
    let out = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] }, NOW)
        .await
        .unwrap();
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.warnings[0].contains("measurements.json"));
    assert!(!publish::outcome_json(&out)["success"].as_bool().unwrap());
    assert!(unzip(&out.zip).contains_key("measurements.json"));
}

#[tokio::test]
async fn a_manifest_naming_a_file_core_will_not_serve_writes_no_zip() {
    let files = core_files();
    let never = Behavior::plain_text_error(404, "Not Found", "no file");
    let mock = MockCore::start(router(&files, never)).await;
    let dir = tmp("contradiction");
    let out = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] }, NOW).await;
    assert!(format!("{:#}", out.unwrap_err()).contains("measurements.json"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn a_report_that_sets_its_own_footer_is_refused_before_core_is_asked() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("footer");
    let report = dir.join("r.typ");
    std::fs::write(&report, "#set page(footer: [mine])\nhi").unwrap();
    let err = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: Some(&report), prose: &[] }, NOW)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("footer"), "{err:#}");
    assert!(mock.requests().is_empty());
}

fn pdf_text_of(zip: &BTreeMap<String, Vec<u8>>, revision: u64) -> String {
    let mut s = Staging::new().unwrap();
    for name in zip.keys().filter(|n| matches!(n.as_str(), "report.typ" | "appendix.typ" | "report-data.json" | "measurements.json") || n.starts_with("streams/") && (n.ends_with(".csv") || n.ends_with(".txt"))) {
        s.stage_bytes(name, &zip[name]).unwrap();
    }
    let pages = embarch_api::report_pdf::report_text(&s, &RenderOptions { ident: format!("{RUN}-r{revision}"), timestamp_unix: NOW }).unwrap();
    pages.iter().flatten().cloned().collect::<Vec<_>>().join(" ")
}

fn prose_adopted(name: &str, text: &str, scope: &str, attribution: &str) -> Value {
    json!({ "kind": "prose", "name": name, "text_sha256": sha(text.as_bytes()), "scope": scope,
            "attribution": attribution, "at_utc_ms": 2 })
}

#[tokio::test]
async fn the_appendix_lists_each_cited_tagged_unbound_untagged_and_hardcoded_number() {
    let prose = "The rate was 49.8 Hz [rate]. A margin of 12 (derived). It also read 49.8 Hz. There were 17 retries.";
    let adopted = prose_adopted("prose/findings.md", prose, "agent", "entered with the agent's token");
    let files = core_files_rev(1, &[adopted]);
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("oracle");
    let report = dir.join("r.typ");
    std::fs::write(&report, format!("{}\n#let hand = (3.5, 9)\nAbout 7 widgets (spec).\n", study_report())).unwrap();
    let blocks = [embarch_api::publish_report::ProseBlock {
        name: "findings.md".into(),
        text: prose.into(),
        author: Some("claude-sonnet-5-5".into()),
        reviewed_by: None,
    }];
    let out = publish::run(
        &client(mock.base_url()),
        Some(&dir.join("zips")),
        &PublishRequest { run_id: RUN, notes: None, report_typ: Some(&report), prose: &blocks },
        NOW,
    )
    .await
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let zip = unzip(&out.zip);
    assert_eq!(zip["prose/findings.md"], prose.as_bytes());
    let reqs = mock.requests();
    let adopt = reqs.iter().filter(|r| r.path().ends_with("/adopt")).map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()).collect::<Vec<_>>();
    assert!(adopt.contains(&json!({ "kind": "prose", "name": "prose/findings.md", "text": prose })));
    // The prose is among embarch-api's files, hashed.
    let publish_req = reqs.iter().find(|r| r.method == "POST" && r.path().ends_with("/publish")).unwrap();
    let body: Value = serde_json::from_slice(&publish_req.body).unwrap();
    assert!(body["files"].as_array().unwrap().iter().any(|f| f["name"] == "prose/findings.md" && f["sha256"] == sha(prose.as_bytes())));

    let appendix = String::from_utf8(zip["appendix.typ"].clone()).unwrap();
    for (num, kind) in [("49.8", "cited"), ("12", "tagged"), ("17", "untagged"), ("9", "hardcoded"), ("3.5", "hardcoded"), ("7", "tagged")] {
        assert!(appendix.contains(&format!("\"{num}\"")) && appendix.contains(&format!("\"{kind}\"")), "{num} {kind}: {appendix}");
    }
    // 49.8 with no citation is unbound, and report.typ's own #m call is cited.
    assert!(appendix.contains("\"unbound\"") && appendix.contains("#m(\\\"rate\\\")"), "{appendix}");
    // The appendix is in the PDF and the PDF still rebuilds from the zip.
    let text = pdf_text_of(&zip, 1);
    assert!(text.contains("Numbers in the prose and in report.typ") && text.contains("hardcoded"), "{text}");
}

#[tokio::test]
async fn an_agents_block_names_its_model_and_an_adopted_one_its_reviewer_with_core_s_scope() {
    let a = prose_adopted("prose/a.md", "alpha", "agent", "entered with the agent's token");
    let b1 = prose_adopted("prose/b.md", "beta", "agent", "entered with the agent's token");
    let b2 = prose_adopted("prose/b.md", "beta", "person", "the person's token");
    let files = core_files_rev(1, &[a, b1, b2]);
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("authors");
    let blocks = [
        embarch_api::publish_report::ProseBlock { name: "a.md".into(), text: "alpha".into(), author: Some("claude-sonnet-5-5".into()), reviewed_by: None },
        embarch_api::publish_report::ProseBlock { name: "b.md".into(), text: "beta".into(), author: Some("claude-sonnet-5-5".into()), reviewed_by: Some("Ada Lovelace".into()) },
    ];
    let out = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &blocks }, NOW)
        .await
        .unwrap();
    let appendix = String::from_utf8(unzip(&out.zip)["appendix.typ"].clone()).unwrap();
    assert!(appendix.contains("\"agent (claude-sonnet-5-5)\""), "{appendix}");
    assert!(appendix.contains("\"agent, reviewed by Ada Lovelace\""), "{appendix}");
    assert!(appendix.contains("\"person\"") && appendix.contains("\"agent\""), "the token scope Core recorded: {appendix}");
}

#[tokio::test]
async fn a_report_that_draws_an_embedded_image_or_svg_is_refused_before_core_is_asked() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("picture");
    for (i, src) in ["#image(\"logo.png\")", "#svg(\"a.svg\")", "#figure(image.decode(\"<svg/>\"))"].iter().enumerate() {
        let report = dir.join(format!("r{i}.typ"));
        std::fs::write(&report, src).unwrap();
        let err = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: Some(&report), prose: &[] }, NOW)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("draws a picture"), "{err:#}");
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn publishing_again_writes_r2_keeps_r1_and_names_it_and_its_manifest_hash() {
    let dir = tmp("revisions");
    let req = PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] };
    let files1 = core_files_rev(1, &[]);
    let mock1 = MockCore::start(router_rev(1, &files1, ok_text(&files1["measurements.json"]))).await;
    let first = publish::run(&client(mock1.base_url()), Some(&dir), &req, NOW).await.unwrap();
    let r1_bytes = std::fs::read(&first.zip).unwrap();
    assert!(first.supersedes.is_none());

    // Core's second revision. Its capture routes answer 404: the captures are swept,
    // so the revision must come from the earlier zip.
    let files2 = core_files_rev(2, &[]);
    let swept = Behavior::plain_text_error(404, "Not Found", "swept");
    let mut routes = match router_rev(2, &files2, ok_text(&files2["measurements.json"])) {
        Behavior::Router { routes, .. } => routes,
        _ => unreachable!(),
    };
    routes.retain(|(p, _)| !p.starts_with("/stream") && p != "/files/measurements.json");
    routes.insert(0, ("/stream".into(), swept.clone()));
    routes.insert(0, ("/files/measurements.json".into(), ok_text(&files2["measurements.json"])));
    let mock2 = MockCore::start(Behavior::Router { routes, otherwise: Box::new(swept) }).await;
    let later = publish::run(&client(mock2.base_url()), Some(&dir), &req, NOW + 86_400 * 3).await.unwrap();
    assert!(later.warnings.is_empty(), "{:?}", later.warnings);
    assert!(later.zip.file_name().unwrap().to_string_lossy().ends_with("-01234567-r2.zip"), "{:?}", later.zip);
    assert_eq!(std::fs::read(&first.zip).unwrap(), r1_bytes, "r1 is kept byte for byte");
    let s = later.supersedes.as_ref().unwrap();
    assert_eq!((s.revision, s.from_zip), (1, true));
    assert_eq!(s.manifest_sha256.as_deref(), Some(first.manifest_sha256.as_str()));
    // No capture was asked of Core.
    assert!(!mock2.requests().iter().any(|r| r.method == "GET" && (r.path().ends_with("/streams") || r.path().contains("/stream/"))), "{:?}", mock2.requests().iter().map(|r| r.path().to_string()).collect::<Vec<_>>());
    // The captures are the earlier zip's, and the appendix names r1 and its manifest hash.
    let zip = unzip(&later.zip);
    assert_eq!(zip["streams/rate.csv"], files2["streams/rate.csv"].as_bytes());
    let appendix = String::from_utf8(zip["appendix.typ"].clone()).unwrap();
    assert!(appendix.contains("supersedes revision 1") && appendix.contains(&first.manifest_sha256), "{appendix}");
    // And the PDF of r2 rebuilds from r2's zip alone.
    let mut st = Staging::new().unwrap();
    for name in ["report.typ", "appendix.typ", "report-data.json", "measurements.json", "streams/rate.csv", "streams/console.txt"] {
        st.stage_bytes(name, &zip[name]).unwrap();
    }
    let again = render_report(&st, &RenderOptions { ident: format!("{RUN}-r2"), timestamp_unix: NOW + 86_400 * 3 }).unwrap();
    assert_eq!(again, zip["report.pdf"]);
}

/// Core's answer for a run whose directory the retention sweep took (`embarch-core`
/// decision 120): `report-data` answers 409 until every Core file is handed back.
fn swept_then_restored_router(revision: u64, files: &BTreeMap<String, String>) -> Behavior {
    let mut routes = match router_rev(revision, files, ok_text(&files["measurements.json"])) {
        Behavior::Router { routes, .. } => routes,
        _ => unreachable!(),
    };
    let rd_ok = routes.iter().find(|(p, _)| p == "/report-data").unwrap().1.clone();
    for (p, b) in routes.iter_mut() {
        if p == "/report-data" {
            *b = Behavior::Sequence(vec![
                Behavior::plain_text_error(409, "Conflict", "the run's files were swept; hand back manifest.dsse.json"),
                rd_ok.clone(),
            ]);
        }
    }
    // The captures are gone: only the restore routes and the revision's own files answer.
    routes.retain(|(p, _)| !p.starts_with("/stream"));
    routes.insert(0, ("/restore/manifest.dsse.json".into(), Behavior::json_ok(json!({ "restored": "manifest.dsse.json", "sha256": "h" }))));
    for n in files.keys().filter(|n| !n.starts_with("report-data.") && *n != "run-log-segment.jsonl") {
        routes.insert(0, (format!("/restore/{n}"), Behavior::json_ok(json!({ "restored": n, "sha256": "h" }))));
    }
    let gone = Behavior::plain_text_error(404, "Not Found", "swept");
    routes.insert(0, ("/stream".into(), gone.clone()));
    Behavior::Router { routes, otherwise: Box::new(gone) }
}

#[tokio::test]
async fn a_swept_run_republishes_to_r2_from_the_earlier_zip_which_is_unchanged() {
    let dir = tmp("swept");
    let req = PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] };
    let files1 = core_files_rev(1, &[]);
    let mock1 = MockCore::start(router_rev(1, &files1, ok_text(&files1["measurements.json"]))).await;
    let first = publish::run(&client(mock1.base_url()), Some(&dir), &req, NOW).await.unwrap();
    let r1_bytes = std::fs::read(&first.zip).unwrap();
    let r1 = unzip(&first.zip);

    let files2 = core_files_rev(2, &[]);
    let mock2 = MockCore::start(swept_then_restored_router(2, &files2)).await;
    let later = publish::run(&client(mock2.base_url()), Some(&dir), &req, NOW + 86_400 * 3).await.unwrap();
    assert!(later.zip.file_name().unwrap().to_string_lossy().ends_with("-01234567-r2.zip"), "{:?}", later.zip);
    assert_eq!(std::fs::read(&first.zip).unwrap(), r1_bytes, "r1 is kept byte for byte");

    let reqs = mock2.requests();
    let puts: Vec<_> = reqs.iter().filter(|r| r.method == "PUT").collect();
    assert!(puts[0].path().ends_with("/restore/manifest.dsse.json"), "the manifest goes first: {:?}", puts.iter().map(|r| r.path()).collect::<Vec<_>>());
    assert_eq!(puts[0].body_text().as_bytes(), r1["manifest.dsse.json"].as_slice(), "the zip's own manifest is what is handed back");
    let rate = puts.iter().find(|r| r.path().ends_with("/restore/streams/rate.csv")).expect("a capture is restored");
    assert_eq!(rate.body_text().as_bytes(), r1["streams/rate.csv"].as_slice());
    // Core's revision outputs and embarch-api's own files are not handed back.
    assert!(!puts.iter().any(|r| r.path().contains("/restore/report-data") || r.path().ends_with("/restore/report.pdf")));
    // The restore came before the second report-data, which is what signs the revision.
    let last_rd = reqs.iter().rposition(|r| r.path().ends_with("/report-data")).unwrap();
    assert!(reqs.iter().rposition(|r| r.method == "PUT").unwrap() < last_rd);
    let zip = unzip(&later.zip);
    assert_eq!(zip["streams/rate.csv"], r1["streams/rate.csv"]);
    let appendix = String::from_utf8(zip["appendix.typ"].clone()).unwrap();
    assert!(appendix.contains("supersedes revision 1") && appendix.contains(&first.manifest_sha256), "{appendix}");
}

#[tokio::test]
async fn a_swept_run_with_no_earlier_zip_refuses_saying_what_it_looked_for_and_writes_nothing() {
    let dir = tmp("swept-none");
    let req = PublishRequest { run_id: RUN, notes: None, report_typ: None, prose: &[] };
    let files = core_files_rev(1, &[]);
    let mock = MockCore::start(swept_then_restored_router(1, &files)).await;
    let err = publish::run(&client(mock.base_url()), Some(&dir), &req, NOW).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("swept") && text.contains("01234567") && text.contains("earlier zip") && text.contains(&dir.display().to_string()), "{text}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    assert!(!mock.requests().iter().any(|r| r.method == "PUT"));
}

#[tokio::test]
async fn a_report_the_syntax_tree_whitelist_refuses_is_refused_before_core_is_asked() {
    let files = core_files();
    let mock = MockCore::start(router(&files, ok_text(&files["measurements.json"]))).await;
    let dir = tmp("whitelist");
    let h = "#import \"@embarch/report:0.1.0\": minmax-plot\n";
    for (i, src) in [
        "#eval(\"1 + 1\")".to_string(),
        "#let d = read(\"/etc/passwd\")".to_string(),
        format!("{h}#let p = minmax-plot\n#p(path(\"streams/rate.csv\"))"),
        "#import \"other.typ\": x".to_string(),
        format!("{h}#minmax-plot(path(\"mine.csv\"))"),
    ]
    .iter()
    .enumerate()
    {
        let report = dir.join(format!("w{i}.typ"));
        std::fs::write(&report, src).unwrap();
        let err = publish::run(&client(mock.base_url()), Some(&dir), &PublishRequest { run_id: RUN, notes: None, report_typ: Some(&report), prose: &[] }, NOW)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("publish refuses this report.typ"), "{src}: {err:#}");
    }
    assert!(mock.requests().is_empty());
}
