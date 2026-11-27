//! `verify` of a Test run's zip (`embarch-api` decision 146; `notes/hil/06-report-and-publish.md`,
//! "Verify").
//!
//! A Test run's zip is told from a Study's by its own files: `test-run.dsse.json` and `run.json`.
//! Check 3 recomputes every verdict through `embarch-study-designer`'s `judge_run`, `judge_test`
//! and the roll-up, from `run.json`'s limits and each study run's `events.json`, `measurements.json`
//! and `study.json`, and compares them with `result.json`, the signed records and
//! `report-data.json`. Check 6 recomputes `report-data.json` whole. What no zip file holds (the
//! image readback, identities, `*IDN?`, a signal's topology origin) rests on Core's signature and
//! is said by name.
//!
//! **Copies of Core's code**, pinned by `tests/verify_test_run.rs` and by nothing in Core:
//! `judge_steps`, `judge_measurements` and `outcome_of` (`embarch-core/src/test_executor.rs`),
//! `manual_rows` and `runs_on_unit` and the field list of `report-data.json`
//! (`embarch-core/src/test_publish.rs`, read on `origin/main`, 2026-11-26).

use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::VerifyingKey;
use embarch_study_designer::hil_expand::{RunJson, RunTest};
use embarch_study_designer::hil_judge::{self, DeclaredBy, FirmwareState, MeasuredValue, MeasurementRecord, Origin, StepRecord};
use embarch_study_designer::host_study::PromptKind;
use embarch_study_designer::verdicts::{Evidence, Judged, Qualifier, Reason, Rules, StepOutcome, Target, Verdict};
use serde_json::{json, Value};

use crate::publish::{JUNIT_NAME, MANIFEST_NAME, MEASUREMENTS_NAME, OCP_NAME, REPORT_DATA_NAME, REPORT_PDF};
use crate::publish_report::{show, Supersedes};
use crate::publish_test::{appendix_source, report_source, RunPins, RUN_JSON_NAME};
use crate::publish_test_export::{junit_xml, ocp_jsonl, TestExportInputs};
use crate::report_pdf::{APPENDIX_FILE, REPORT_FILE};
use crate::verify::{check_call_log, check_chain, check_manifest, check_measurements, json_of, open_envelope, segment_lines, sha, subjects, Check, Status};

type Files = BTreeMap<String, Vec<u8>>;

/// A Test run's own signed record.
pub const TEST_RECORD_NAME: &str = "test-run.dsse.json";
const RESULT_NAME: &str = "result.json";
const REPORT_DATA_ENVELOPE: &str = "report-data.dsse.json";
const STUDY_RECORD: &str = "record.dsse.json";
const STUDIES: &str = "studies/";
/// The statement every Test report carries (`embarch-core`'s `FIXED_STATEMENT`); a copy.
const NO_ATLAS: &str = "no atlas was joined: the standard's own requirement count is absent";
/// More distinct signals of unknown target than this are not enumerated.
const MAX_AMBIGUOUS_SIGNALS: usize = 6;

/// A Test run's zip has the run's own record beside its `run.json`; a Study's has `record.dsse.json`.
pub fn is_test_zip(files: &Files) -> bool {
    files.contains_key(TEST_RECORD_NAME) && files.contains_key(RUN_JSON_NAME) && !files.contains_key(STUDY_RECORD)
}

/// One study run's files, with the study folder's prefix taken off.
struct Study {
    files: Files,
    record: Option<Value>,
    study: Option<Value>,
    events: Option<Value>,
    measurements: Option<Value>,
}

fn study_ids(files: &Files) -> BTreeSet<String> {
    files.keys().filter_map(|n| n.strip_prefix(STUDIES)?.split_once('/').map(|(id, _)| id.to_string())).collect()
}

fn worst(into: &mut Check, from: Check, prefix: &str) {
    for l in from.compared {
        into.compared.push(format!("{prefix}{l}"));
    }
    for p in from.problems {
        into.problems.push(format!("{prefix}{p}"));
    }
    into.status = match (into.status, from.status) {
        (Status::Fail, _) | (_, Status::Fail) => Status::Fail,
        (Status::Partial, _) | (_, Status::Partial) => Status::Partial,
        (s, _) => s,
    };
}

/// The checks of a Test run's zip, 0 to 9.
pub fn checks(files: &Files, key: &VerifyingKey, fingerprint: &str, attested: bool) -> Vec<Check> {
    let mut out = Vec::new();
    let (c0, manifest, record, study_records) = check_signatures(files, key, fingerprint, attested);
    out.push(c0);
    out.push(check_manifest(files, manifest.as_ref(), TEST_RECORD_NAME));

    let studies: BTreeMap<String, Study> = study_ids(files)
        .into_iter()
        .map(|id| {
            let prefix = format!("{STUDIES}{id}/");
            let sub: Files = files.iter().filter_map(|(n, b)| Some((n.strip_prefix(&prefix)?.to_string(), b.clone()))).collect();
            let s = Study {
                record: study_records.get(&id).cloned(),
                study: json_of(&sub, "study.json"),
                events: json_of(&sub, "events.json"),
                measurements: json_of(&sub, MEASUREMENTS_NAME),
                files: sub,
            };
            (id, s)
        })
        .collect();

    let z = Zip::new(files, manifest.as_ref(), record.as_ref(), studies, fingerprint);

    let mut c2 = Check::new(2, "the measurements");
    for (id, s) in &z.studies {
        worst(&mut c2, check_measurements(&s.files, s.record.as_ref()), &format!("studies/{id}: "));
    }
    if z.studies.is_empty() {
        c2.compared.push("the zip holds no study run".into());
    }
    out.push(c2);

    let interrupted = z.interrupted();
    let (mut c3, mut c5, mut c6) = (Check::new(3, "verdicts"), Check::new(5, "the labels"), Check::new(6, "the report's other claims"));
    if let Some(why) = &interrupted {
        for c in [&mut c3, &mut c5, &mut c6] {
            c.status = Status::NotChecked;
            c.problems.push(why.clone());
        }
    } else {
        let derived = z.derive(&mut c3);
        compare_verdict_parts(&z, &derived, &mut c3);
        check_labels(&z, &derived, &mut c5);
        check_report_claims(&z, &derived, &mut c6);
    }
    out.push(c3);
    let mut c4 = crate::verify_report::check_pdf_text(files);
    let regenerated = |what: &str| c6.compared.iter().any(|l| l.starts_with(what));
    if c4.status == Status::Partial && regenerated("report.typ is what") && regenerated("appendix.typ is what") {
        // The numbers only `appendix.typ` printed are no longer embarch-api's say-so: both Typst
        // sources regenerate byte for byte from the signed data (check 6).
        c4.status = Status::Pass;
        let notes = std::mem::take(&mut c4.problems);
        c4.compared.extend(notes.into_iter().map(|n| format!("{n}; report.typ and appendix.typ regenerate from the signed data and the manifest (check 6)")));
    }
    out.push(c4);
    out.push(c5);
    out.push(c6);
    out.push(check_exports(&z));
    out.push(check_log_segment(&z));
    out.push(crate::verify_report::check_pictures(files));
    out.sort_by_key(|c| c.id);
    out
}

// ---- check 0 -----------------------------------------------------------------

fn check_signatures(files: &Files, key: &VerifyingKey, fingerprint: &str, attested: bool) -> (Check, Option<Value>, Option<Value>, BTreeMap<String, Value>) {
    let mut c = Check::new(0, "the signatures");
    let open = |name: &str, c: &mut Check| -> Option<Value> {
        let Some(bytes) = files.get(name) else {
            c.fail(format!("{name} is not in the zip"));
            return None;
        };
        match open_envelope(bytes, key, fingerprint) {
            Ok(st) => {
                c.compared.push(format!("{name}: Ed25519 signature verifies under key {fingerprint}"));
                Some(st)
            }
            Err(why) => {
                c.fail(format!("{name} {why}"));
                None
            }
        }
    };
    let manifest = open(MANIFEST_NAME, &mut c);
    let record = open(TEST_RECORD_NAME, &mut c);
    let rd_env = open(REPORT_DATA_ENVELOPE, &mut c);
    if let Some(st) = &rd_env {
        let named = subjects(st).get(REPORT_DATA_NAME).cloned();
        if named.is_none() || named != files.get(REPORT_DATA_NAME).map(|b| sha(b)) {
            c.fail(format!("{REPORT_DATA_ENVELOPE} does not sign this {REPORT_DATA_NAME}"));
        }
        if Some(&st["predicate"]) != json_of(files, REPORT_DATA_NAME).as_ref() {
            c.fail(format!("{REPORT_DATA_ENVELOPE}'s signed predicate is not {REPORT_DATA_NAME}'s content"));
        }
    }
    if let Some(st) = &record {
        let subs = subjects(st);
        for (name, digest) in &subs {
            match files.get(name) {
                None => c.fail(format!("the Test run's record names {name}, which the zip lacks")),
                Some(b) if &sha(b) != digest => c.fail(format!("{name} does not match the digest the Test run's record holds")),
                _ => {}
            }
        }
        for name in [RUN_JSON_NAME, RESULT_NAME] {
            if files.contains_key(name) && !subs.contains_key(name) {
                c.fail(format!("{name} is not a subject of the Test run's record"));
            }
        }
        c.compared.push(format!("the Test run's record's {} subjects hash to the zip's files; run.json is one", subs.len()));
    }
    let run_sha = files.get(RUN_JSON_NAME).map(|b| sha(b));
    let mut study_records = BTreeMap::new();
    for id in study_ids(files) {
        let dir = format!("{STUDIES}{id}/");
        let rname = format!("{dir}{STUDY_RECORD}");
        let Some(st) = open(&rname, &mut c) else { continue };
        let subs = subjects(&st);
        for (name, digest) in &subs {
            if name == RUN_JSON_NAME {
                if Some(digest) != run_sha.as_ref() {
                    c.fail(format!("{rname}: its run.json is not this zip's run.json: the study run belongs to another run"));
                }
                continue;
            }
            match files.get(&format!("{dir}{name}")) {
                None => c.fail(format!("{rname} names {name}, which the zip lacks")),
                Some(b) if &sha(b) != digest => c.fail(format!("{dir}{name} does not match the digest its study record holds")),
                _ => {}
            }
        }
        if !subs.contains_key(RUN_JSON_NAME) {
            c.fail(format!("{rname} does not name run.json: it was not amended for a Test run"));
        }
        for name in files.keys().filter_map(|n| n.strip_prefix(&dir)) {
            let core_file = matches!(name, "events.json" | MEASUREMENTS_NAME | "study.json") || name.starts_with("streams/");
            if core_file && !subs.contains_key(name) {
                c.fail(format!("{dir}{name} is not a subject of its study record"));
            }
        }
        study_records.insert(id, st);
    }
    if let Some(st) = &record {
        let mut listed = BTreeSet::new();
        for t in st["predicate"]["tests"].as_array().into_iter().flatten() {
            for s in t["study_records"].as_array().into_iter().flatten() {
                let id = s["study_id"].as_str().unwrap_or("");
                listed.insert(id.to_string());
                let have = files.get(&format!("{STUDIES}{id}/{STUDY_RECORD}")).map(|b| sha(b));
                if have.is_none() || s["record_sha256"].as_str() != have.as_deref() {
                    c.fail(format!("test {}: study {id}'s record is not the file the Test run's record names", t["test_id"].as_str().unwrap_or("")));
                }
            }
        }
        for id in study_ids(files) {
            if !listed.contains(&id) {
                c.fail(format!("studies/{id}/ is in the zip and no test of the Test run's record names it"));
            }
        }
        c.compared.push(format!("{} study run records, each named by the Test run's record with its hash, each signing its folder's files and run.json", listed.len()));
    }
    if c.status != Status::Fail {
        c.compared.push(if attested { "the key is the attested fingerprint the reader gave".into() } else { crate::verify::UNATTESTED.to_string() });
    }
    (c, manifest, record, study_records)
}

// ---- the zip, read once ------------------------------------------------------

struct Zip<'a> {
    files: &'a Files,
    manifest: Option<&'a Value>,
    record: Option<&'a Value>,
    studies: BTreeMap<String, Study>,
    fingerprint: &'a str,
    run: Option<RunJson>,
    run_value: Value,
    result: Option<Value>,
    rd: Value,
    lines: Result<Vec<Value>, String>,
    rules: Result<Rules, String>,
}

impl<'a> Zip<'a> {
    fn new(files: &'a Files, manifest: Option<&'a Value>, record: Option<&'a Value>, studies: BTreeMap<String, Study>, fingerprint: &'a str) -> Self {
        let run_value = json_of(files, RUN_JSON_NAME).unwrap_or(Value::Null);
        let rd = json_of(files, REPORT_DATA_NAME).unwrap_or(Value::Null);
        let rules = (|| {
            let pairs: Vec<(String, u32)> = rd["rules"]
                .as_array()
                .ok_or("report-data.json has no rules list")?
                .iter()
                .map(|r| Some((r["name"].as_str()?.to_string(), u32::try_from(r["behaviour"].as_u64()?).ok()?)))
                .collect::<Option<_>>()
                .ok_or("report-data.json's rules are not (name, behaviour number) pairs")?;
            let named: Vec<(&str, u32)> = pairs.iter().map(|(n, b)| (n.as_str(), *b)).collect();
            Rules::pinned(&named).map_err(|e| format!("this build cannot pin the behaviours report-data.json names ({e:?}): a verify older than the Core"))
        })();
        Zip {
            files,
            manifest,
            record,
            studies,
            fingerprint,
            run: serde_json::from_value(run_value.clone()).ok(),
            run_value,
            result: json_of(files, RESULT_NAME),
            rd,
            lines: segment_lines(files).map_err(|e| format!("{e:#}")),
            rules,
        }
    }

    fn predicate(&self) -> &Value {
        self.record.map_or(&Value::Null, |r| &r["predicate"])
    }

    /// Why a run's verdicts are not this version's to check: Core's `core/178` owns the shape of an
    /// interrupted or resumed run.
    fn interrupted(&self) -> Option<String> {
        let p = self.predicate();
        let status = p["status"].as_str().unwrap_or("");
        let resumes = !p["resumes"].is_null() || self.run_value["resumes"].is_string();
        if !matches!(status, "completed" | "cancelled" | "deadline") || resumes {
            return Some(format!(
                "not checked by this version: the Test run is {} (status {status:?}); the verdicts of an interrupted or resumed run are not recomputed",
                if resumes { "a resume" } else { "interrupted" }
            ));
        }
        None
    }

    fn lines(&self) -> &[Value] {
        self.lines.as_deref().unwrap_or(&[])
    }
}

// ---- recomputing a run's verdict ---------------------------------------------

/// `embarch-core`'s `outcome_of`, a copy: the strings are its `host_study::detail`'s.
fn outcome_of(detail: &str, observation: bool) -> Option<StepOutcome> {
    Some(match detail {
        "cancelled" => return None,
        "refused" => StepOutcome::RefusedOn,
        "empty_reply" => StepOutcome::EmptyReply,
        "until_not_seen" | "unacknowledged" => StepOutcome::UntilNotSeen,
        "port_lost" | "port_unavailable" | "exchange_failed" => StepOutcome::PortLost,
        "prompt_cancelled" => StepOutcome::PromptCancelled { observation },
        "prompt_timed_out" => StepOutcome::PromptTimedOut { observation },
        _ => StepOutcome::Pass,
    })
}

fn walk_actions<'a>(steps: &'a Value, out: &mut BTreeMap<&'a str, &'a Value>) {
    for s in steps.as_array().into_iter().flatten() {
        if let Some(n) = s["name"].as_str() {
            out.insert(n, &s["action"]);
        }
        walk_actions(&s["action"]["Repeat"]["steps"], out);
    }
}

/// What `judge_steps` found beside the steps: the signals whose target no file of the zip holds,
/// and the steps whose row names another signal than the study's action does.
#[derive(Default)]
struct StepFindings {
    unknown: BTreeSet<String>,
    contradicted: Vec<String>,
}

/// `embarch-core`'s `judge_steps`, a copy over the signed study file read as JSON. An `Exchange`
/// row Core wrote since its decision 155 carries `target` (`dut` or `bench`), the one lookup the
/// study made, and is judged under it (`embarch-api` decision 149). **A row without it, from an
/// older Core, has no target in any file of the zip**: its step is given its target by
/// `bench_signals`, and the signals whose target could change the verdict (a refusal or an
/// `until` not seen) are returned in `unknown`. A row whose `signal` is not its action's is
/// returned in `contradicted`.
fn judge_steps(test: &RunTest, study: Option<&Value>, events: Option<&Value>, bench_signals: &BTreeSet<String>) -> (Vec<StepRecord>, StepFindings) {
    let mut actions = BTreeMap::new();
    if let Some(s) = study {
        walk_actions(&s["steps"], &mut actions);
        walk_actions(&s["teardown"], &mut actions);
    }
    let kinds: BTreeMap<&str, PromptKind> = test.prompt_kinds.iter().map(|k| (k.step.as_str(), k.kind)).collect();
    let mut found = StepFindings::default();
    let mut out = Vec::new();
    for s in events.map_or(&[][..], |e| e["steps"].as_array().map_or(&[][..], Vec::as_slice)) {
        let Some(name) = s["step_name"].as_str() else { continue };
        let observation = kinds.get(name) == Some(&PromptKind::Observation);
        let Some(outcome) = outcome_of(s["detail"].as_str().unwrap_or(""), observation) else { continue };
        let action = actions.get(name).copied().unwrap_or(&Value::Null);
        let target = if let Some(signal) = action["Exchange"]["signal"].as_str() {
            if s["signal"].as_str().is_some_and(|row| row != signal) {
                found.contradicted.push(name.to_string());
            }
            match s["target"].as_str() {
                Some("bench") => Target::Bench,
                Some("dut") => Target::Dut,
                _ => {
                    if matches!(outcome, StepOutcome::RefusedOn | StepOutcome::TimedOut | StepOutcome::UntilNotSeen) {
                        found.unknown.insert(signal.to_string());
                    }
                    if bench_signals.contains(signal) {
                        Target::Bench
                    } else {
                        Target::Dut
                    }
                }
            }
        } else if action.get("Instrument").is_some() {
            Target::Bench
        } else {
            Target::Dut
        };
        out.push(StepRecord { step: name.to_string(), target, outcome });
    }
    (out, found)
}

/// The UTC date `yyyy-mm-dd` of a millisecond timestamp (`embarch-core`'s, a copy).
fn utc_date(ms: u64) -> String {
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `embarch-core`'s `judge_measurements`, a copy.
/// Each limit of a test with the decision rule, `U` and `w` its verdict was judged on, as Core
/// writes them (decision 154 of Core): `u` and `k` are the test's declared source, `w` is 0
/// under simple acceptance and for a measurement with no source.
fn limits_of(t: &RunTest) -> Value {
    let guard = t.decision_rule.kind != "simple_acceptance";
    Value::Array(
        t.limits
            .iter()
            .map(|l| {
                let source = t.sources.iter().find(|s| s.measurement == l.measurement);
                let u = source.map(|s| s.u);
                json!({
                    "measurement": l.measurement, "unit": l.unit, "lower": l.lower, "upper": l.upper, "expect": l.expect,
                    "rule": t.decision_rule.kind, "w_multiple": t.decision_rule.w_multiple,
                    "u": u, "k": source.map(|s| s.k), "u_declared": source.is_some(),
                    "w": if guard { t.decision_rule.w_multiple * u.unwrap_or(0.0) } else { 0.0 },
                })
            })
            .collect(),
    )
}

fn judge_measurements(test: &RunTest, doc: &Value) -> Vec<MeasurementRecord> {
    doc["measurements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            let source = test.sources.iter().find(|s| s.measurement == name);
            let origin = match (m["by"].as_str(), m["instrument"]["kind"].as_str()) {
                (Some("person"), _) => Origin::PersonObservation,
                (Some("dut"), _) => Origin::DutReport,
                (_, Some("instrument")) => Origin::Instrument,
                _ => Origin::BenchBoard { firmware: FirmwareState::Unverified },
            };
            let incomplete: Vec<Reason> = m["incomplete"].as_array().into_iter().flatten().filter_map(|i| serde_json::from_value(i.clone()).ok()).collect();
            let calibration = &m["instrument"]["calibration"];
            let on_instrument = origin == Origin::Instrument;
            let calibration_applied = calibration["applied"] != false;
            let calibration_expired = on_instrument
                && calibration_applied
                && calibration["due"].as_str().is_some_and(|due| due < utc_date(m["at_utc_ms"].as_u64().unwrap_or(0)).as_str());
            let calibration_undeclared = if on_instrument { calibration.is_null() || !calibration_applied } else { source.is_none() };
            let by_person = m["declared_by"].as_array().is_some_and(|d| !d.is_empty() && d.iter().all(|x| x["scope"] == "person"));
            Some(MeasurementRecord {
                measurement: name.to_string(),
                value: match m["value"].as_str() {
                    Some(choice) => MeasuredValue::Choice(choice.to_string()),
                    None => MeasuredValue::Number(m["value"].as_f64().unwrap_or(0.0)),
                },
                u: source.map_or(0.0, |s| s.u),
                evidence: Evidence { incomplete, calibration_expired },
                origin,
                declared_by: if by_person { DeclaredBy::Person } else { DeclaredBy::Agent },
                calibration_undeclared,
            })
        })
        .collect()
}

fn judged_of(v: &Value, qualifiers: &Value) -> Option<Judged> {
    Some(Judged {
        verdict: serde_json::from_value(v["verdict"].clone()).ok()?,
        reason: serde_json::from_value::<Option<Reason>>(v["reason"].clone()).ok()?,
        qualifiers: serde_json::from_value(qualifiers.clone()).unwrap_or_default(),
    })
}

fn word(j: &Judged) -> String {
    format!("{}{}", serde_json::to_value(j.verdict).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(), match j.reason {
        Some(r) => format!(" ({})", serde_json::to_value(r).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
        None => String::new(),
    })
}

/// One test of the run, as the roll-up reads it.
struct Row {
    test_id: String,
    definition_hash: String,
    requirements: Vec<String>,
    judged: Judged,
    reached: bool,
    /// What the signed record says of the test's reason (a string, or null).
    reason: Value,
    counts: Value,
    runs: Value,
}

struct Derived {
    rows: Vec<Row>,
    /// What `report-data.json` should hold, field by field.
    expected: Value,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

impl Zip<'_> {
    /// Every test's verdict recomputed (naming what could not be), the roll-up over them and the
    /// whole of `report-data.json` as Core would write it. Problems go to check 3.
    fn derive(&self, c: &mut Check) -> Derived {
        let empty = Derived { rows: Vec::new(), expected: Value::Null };
        let Some(run) = &self.run else {
            c.fail("run.json is not a Test run's resolved run".into());
            return empty;
        };
        let rules = match &self.rules {
            Ok(r) => Some(r),
            Err(why) => {
                c.partial(format!("verdicts not recomputed: {why}"));
                None
            }
        };
        let p = self.predicate();
        let record_tests = p["tests"].as_array().map_or(&[][..], Vec::as_slice);
        let results = self.result.as_ref().and_then(|r| r["results"].as_array()).map_or(&[][..], Vec::as_slice);
        let (mut recomputed, mut taken, mut cancelled_or_errored, mut unreached) = (0usize, 0usize, 0usize, 0usize);
        let mut rows = Vec::new();
        for t in &run.tests {
            let id = t.test_id.as_str();
            let Some(rec) = record_tests.iter().find(|r| r["test_id"] == id) else {
                c.fail(format!("test {id}: run.json selected it and the signed Test run's record has no entry for it, and gives no reason for its absence"));
                continue;
            };
            let Some(res) = results.iter().find(|r| r["test_id"] == id) else {
                // A test the run never reached: the record's own reason says why.
                match (rec["verdict"].is_null(), rec["reason"].as_str()) {
                    (true, Some("not_run")) => unreached += 1,
                    (true, Some("interrupted")) => c.partial(format!("test {id}: interrupted, so it has no verdict to recompute")),
                    _ => c.fail(format!("test {id}: result.json holds no result for it and the signed record gives no `not_run` reason ({} / {})", rec["verdict"], rec["reason"])),
                }
                rows.push(Row {
                    test_id: id.to_string(),
                    definition_hash: t.definition_hash.clone(),
                    requirements: t.requirements.clone(),
                    judged: Judged { verdict: Verdict::Skipped, reason: Some(Reason::StoppedByError), qualifiers: vec![] },
                    reached: false,
                    reason: rec["reason"].clone(),
                    counts: Value::Null,
                    runs: json!([]),
                });
                continue;
            };
            let row = self.judge_test_row(c, t, rec, res, rules, &mut recomputed, &mut taken, &mut cancelled_or_errored);
            rows.push(row);
        }
        for r in results {
            if !run.tests.iter().any(|t| r["test_id"] == t.test_id.as_str()) {
                c.fail(format!("result.json holds a result for {}, a test run.json did not select", r["test_id"]));
            }
        }
        c.compared.push(format!(
            "{} tests of run.json: {recomputed} runs recomputed through judge_run and each test through judge_test; {taken} verdicts of a skipped or errored test taken from result.json (the executor's policy, not derivable from the files); {cancelled_or_errored} cancelled runs rest on Core's signature; {unreached} tests never reached, each with its signed `not_run` reason",
            run.tests.len()
        ));
        c.compared.push("what Core read from the hardware (the image readback, identities, `*IDN?`) and each step signal's topology origin rest on Core's signature, not on a recompute".into());
        let expected = match rules {
            Some(r) => self.expected_report_data(c, run, &rows, r),
            None => Value::Null,
        };
        Derived { rows, expected }
    }

    #[allow(clippy::too_many_arguments)]
    fn judge_test_row(
        &self,
        c: &mut Check,
        t: &RunTest,
        rec: &Value,
        res: &Value,
        rules: Option<&Rules>,
        recomputed: &mut usize,
        taken: &mut usize,
        cancelled_or_errored: &mut usize,
    ) -> Row {
        let id = t.test_id.as_str();
        let recorded_runs = res["runs"].as_array().map_or(&[][..], Vec::as_slice);
        let study_ids = strings(&res["study_ids"]);
        let executor_decided = !res["note"].is_null();
        let recorded_test = judged_of(res, &res["qualifiers"]);
        let mut runs: Vec<Judged> = Vec::new();
        let mut run_values: Vec<Value> = Vec::new();
        for (i, rr) in recorded_runs.iter().enumerate() {
            let study = study_ids.get(i).and_then(|sid| self.studies.get(sid).map(|s| (sid.as_str(), s)));
            let part_run = study.and_then(|(_, s)| s.record.as_ref()).map(|r| r["predicate"]["test"]["run"].clone());
            let qualifiers = part_run.as_ref().map_or(Value::Null, |p| p["qualifiers"].clone());
            let Some(recorded) = judged_of(rr, &qualifiers) else {
                c.fail(format!("test {id}: run {} of result.json is not a verdict", i + 1));
                continue;
            };
            run_values.push(json!({ "verdict": rr["verdict"], "reason": rr["reason"] }));
            if let Some(p) = &part_run {
                if p["verdict"] != rr["verdict"] || p["reason"] != rr["reason"] {
                    c.fail(format!("test {id}: study {}'s record says its run was {} and result.json says {}", study_ids[i], p["verdict"], rr["verdict"]));
                }
            }
            let Some((sid, s)) = study.filter(|_| !executor_decided && recorded.verdict != Verdict::Cancelled) else {
                if recorded.verdict == Verdict::Cancelled {
                    *cancelled_or_errored += 1;
                }
                runs.push(recorded);
                continue;
            };
            let Some(rules) = rules else {
                runs.push(recorded);
                continue;
            };
            let test_part = &s.record.as_ref().map_or(Value::Null, |r| r["predicate"]["test"].clone());
            if test_part["test_id"] != id || test_part["run_id"] != self.run_value["run_id"] || test_part["definition_hash"] != t.definition_hash.as_str() {
                c.fail(format!("test {id}: study {sid}'s record names another test, run or definition ({} / {} / {})", test_part["test_id"], test_part["run_id"], test_part["definition_hash"]));
            }
            if s.study.is_none() && s.events.as_ref().is_some_and(|e| e["steps"].as_array().is_some_and(|a| !a.is_empty())) {
                c.partial(format!("test {id}: studies/{sid}/study.json is not in the zip, so its steps' actions are not known and its run was not recomputed"));
                runs.push(recorded);
                continue;
            }
            let measurements = s.measurements.as_ref().map(|m| judge_measurements(t, m)).unwrap_or_default();
            let got = self.judge_with_targets(c, t, s, &measurements, rules, &recorded, id, sid);
            *recomputed += 1;
            runs.push(got.unwrap_or(recorded));
        }
        let (judged, counts) = if let (false, false, Some(rules)) = (executor_decided, runs.is_empty(), rules) {
            match hil_judge::judge_test(&runs, rules) {
                Ok(v) => (v.judged, serde_json::to_value(v.counts).unwrap_or(Value::Null)),
                Err(e) => {
                    c.fail(format!("test {id}: the test could not be judged ({e:?})"));
                    (runs[0].clone(), Value::Null)
                }
            }
        } else {
            *taken += 1;
            (recorded_test.clone().unwrap_or_else(|| Judged { verdict: Verdict::Error, reason: None, qualifiers: vec![] }), res["counts"].clone())
        };
        if let Some(rt) = &recorded_test {
            let same = rt.verdict == judged.verdict && rt.reason == judged.reason && rt.qualifiers == judged.qualifiers;
            if !same {
                c.fail(format!("test {id}: its runs recompute to {} with qualifiers {:?}, result.json says {}", word(&judged), judged.qualifiers, word(rt)));
            }
        }
        if counts != res["counts"] && !executor_decided {
            c.fail(format!("test {id}: its runs' counts recompute to {counts}, result.json says {}", res["counts"]));
        }
        let verdict_text = serde_json::to_value(judged.verdict).unwrap_or(Value::Null);
        if rec["verdict"] != verdict_text || rec["reason"] != serde_json::to_value(judged.reason).unwrap_or(Value::Null) {
            c.fail(format!("test {id}: the signed Test run's record says {} ({}), the test recomputes to {}", rec["verdict"], rec["reason"], word(&judged)));
        }
        Row {
            test_id: id.to_string(),
            definition_hash: t.definition_hash.clone(),
            requirements: t.requirements.clone(),
            reached: true,
            reason: rec["reason"].clone(),
            judged,
            counts,
            runs: Value::Array(run_values),
        }
    }

    /// `judge_run` under each reading of the signals whose target the zip does not hold (a row with
    /// a recorded `target` has none to read); the recorded verdict must be one of them, and the
    /// check is partial when they disagree. A row naming another signal than its step fails.
    #[allow(clippy::too_many_arguments)]
    fn judge_with_targets(&self, c: &mut Check, t: &RunTest, s: &Study, measurements: &[MeasurementRecord], rules: &Rules, recorded: &Judged, id: &str, sid: &str) -> Option<Judged> {
        let (_, found) = judge_steps(t, s.study.as_ref(), s.events.as_ref(), &BTreeSet::new());
        if !found.contradicted.is_empty() {
            c.fail(format!(
                "test {id}: study {sid}'s events.json rows for {} name another signal than study.json's action does; the signed files contradict each other",
                found.contradicted.join(", ")
            ));
            return None;
        }
        let unknown: Vec<String> = found.unknown.into_iter().collect();
        if unknown.len() > MAX_AMBIGUOUS_SIGNALS {
            c.partial(format!("test {id}: study {sid} has {} signals of unknown target, too many to enumerate; its run was not recomputed", unknown.len()));
            return None;
        }
        let mut results: Vec<(Judged, BTreeSet<String>)> = Vec::new();
        for mask in 0..(1u32 << unknown.len()) {
            let bench: BTreeSet<String> = unknown.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, n)| n.clone()).collect();
            let (steps, _) = judge_steps(t, s.study.as_ref(), s.events.as_ref(), &bench);
            match hil_judge::judge_run(t, &steps, measurements, rules) {
                Ok(j) => {
                    if !results.iter().any(|(r, _)| *r == j) {
                        results.push((j, bench));
                    }
                }
                Err(e) => {
                    c.fail(format!("test {id}: study {sid}'s run could not be judged ({e:?})"));
                    return None;
                }
            }
        }
        if let Some((j, _)) = results.iter().find(|(j, _)| j == recorded) {
            if results.len() > 1 {
                c.partial(format!(
                    "test {id}: study {sid}'s verdict depends on whether {} is the DUT's or the bench's, which no file of the zip records ({} readings, the signed one is among them)",
                    unknown.join(", "),
                    results.len()
                ));
            }
            return Some(j.clone());
        }
        let got = results.iter().map(|(j, _)| format!("{} {:?}", word(j), j.qualifiers)).collect::<Vec<_>>().join(" or ");
        c.fail(format!("test {id}: study {sid}'s run recomputes to {got}; the signed records say {} {:?}", word(recorded), recorded.qualifiers));
        results.into_iter().next().map(|(j, _)| j)
    }
}

// ---- the roll-up and the data's fields ---------------------------------------

struct ManualRow {
    judged: Judged,
    requirements: Vec<String>,
    matched: bool,
    counted: bool,
    relayed: bool,
}

fn latest_entry(lines: &[Value], kind: &str, unit_key: Option<&str>) -> Value {
    lines
        .iter()
        .rev()
        .find(|l| l["entry"]["type"] == kind && unit_key.is_none_or(|k| l["entry"]["unit_key"] == k))
        .map(|l| l["entry"].clone())
        .unwrap_or(Value::Null)
}

fn manual_judged(verdict: &str, relayed: bool) -> Judged {
    let v = match verdict {
        "pass" => Verdict::Pass,
        "fail" => Verdict::Fail,
        _ => Verdict::Inconclusive,
    };
    Judged { verdict: v, reason: None, qualifiers: vec![if relayed { Qualifier::Relayed } else { Qualifier::RestsOnPersonObservation }] }
}

/// `embarch-core`'s `manual_rows`, a copy.
fn manual_rows(lines: &[Value], unit_key: Option<&str>, tests: &[Row]) -> (Vec<ManualRow>, Vec<Value>) {
    let Some(unit) = unit_key else { return (vec![], vec![]) };
    let mut latest: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] == "manual_verdict" && e["unit_key"] == unit && e["relayed"] != true {
            latest.insert((e["test_id"].as_str().unwrap_or("").to_string(), e["definition_hash"].as_str().unwrap_or("").to_string()), i);
        }
    }
    let (mut rows, mut listed) = (Vec::new(), Vec::new());
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] != "manual_verdict" || e["unit_key"] != unit {
            continue;
        }
        let (id, hash) = (e["test_id"].as_str().unwrap_or(""), e["definition_hash"].as_str().unwrap_or(""));
        let test = tests.iter().find(|t| t.test_id == id && t.definition_hash == hash);
        let relayed = e["relayed"] == true;
        let counted = !relayed && test.is_some() && latest.get(&(id.to_string(), hash.to_string())) == Some(&i);
        listed.push(json!({
            "log_index": i, "test_id": id, "definition_hash": hash, "verdict": e["verdict"], "reason": e["reason"], "note": e["note"],
            "dut_image": e["dut_image"], "scope": e["scope"], "relayed": relayed, "counted": counted,
            "in_run": test.is_some(), "at_utc_ms": e["at_utc_ms"],
            "attribution": if relayed { "manual verdict, relayed by the agent" } else { "manual verdict, the person's" },
        }));
        rows.push(ManualRow {
            judged: manual_judged(e["verdict"].as_str().unwrap_or(""), relayed),
            requirements: test.map(|t| t.requirements.clone()).unwrap_or_default(),
            matched: test.is_some(),
            counted,
            relayed,
        });
    }
    (rows, listed)
}

/// `embarch-core`'s `runs_on_unit`, a copy.
fn runs_on_unit(lines: &[Value], unit_key: Option<&str>) -> Vec<Value> {
    let Some(unit) = unit_key else { return vec![] };
    type Key = (String, String, String, String);
    let mut groups: BTreeMap<Key, Vec<Value>> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] != "test" || e["unit_key"] != unit {
            continue;
        }
        let key = (
            e["test_id"].as_str().unwrap_or("").to_string(),
            strings(&e["requirements"]).join(","),
            e["dut_image"].as_str().unwrap_or("").to_string(),
            e["definition_hash"].as_str().unwrap_or("").to_string(),
        );
        groups.entry(key).or_default().push(json!({ "run_id": e["run_id"], "verdict": e["verdict"], "log_index": i }));
    }
    groups
        .into_iter()
        .map(|((test_id, reqs, image, hash), runs)| {
            json!({
                "test_id": test_id, "requirements": reqs.split(',').filter(|r| !r.is_empty()).collect::<Vec<_>>(),
                "dut_image": if image.is_empty() { Value::Null } else { Value::from(image) }, "definition_hash": hash, "runs": runs,
            })
        })
        .collect()
}

fn attestation_label(person_acted: bool) -> &'static str {
    if person_acted {
        "person's acts not protected"
    } else {
        "agent-scoped only"
    }
}

/// What the run log says of who acted and what was declared.
struct LogFacts {
    go_by: Value,
    unit_declared: Value,
    person: Value,
    published_by: String,
    person_acted: bool,
}

impl Zip<'_> {
    fn log_facts(&self, rows: &[Row]) -> LogFacts {
        let lines = self.lines();
        let unit_key = self.predicate()["unit_key"].as_str();
        let run_id = self.predicate()["run_id"].as_str().unwrap_or("");
        let go_by = lines
            .iter()
            .rev()
            .find(|l| l["entry"]["type"] == "test_run_started" && l["entry"]["run_id"] == run_id)
            .map_or(Value::Null, |l| l["entry"]["scope"].clone());
        let unit_declared = unit_key.map_or(Value::Null, |k| latest_entry(lines, "unit_declared", Some(k)));
        let person = latest_entry(lines, "person_declared", None);
        let published_by = self.manifest.and_then(|m| m["predicate"]["published_by"].as_str()).unwrap_or("agent").to_string();
        let (manual, _) = manual_rows(lines, unit_key, rows);
        let person_acted = go_by == "person" || published_by == "person" || !unit_declared.is_null() || !person.is_null() || manual.iter().any(|m| m.counted);
        LogFacts { go_by, unit_declared, person, published_by, person_acted }
    }

    fn expected_report_data(&self, c: &mut Check, run: &RunJson, rows: &[Row], rules: &Rules) -> Value {
        let p = self.predicate();
        let lines = self.lines();
        let unit_key = p["unit_key"].as_str();
        let facts = self.log_facts(rows);
        let (manual, manual_listed) = manual_rows(lines, unit_key, rows);

        let mut requirements = serde_json::Map::new();
        let req_ids: BTreeSet<&String> = rows.iter().flat_map(|t| t.requirements.iter()).collect();
        for req in req_ids {
            let mut items: Vec<Judged> = Vec::new();
            let (mut ids, mut skipped) = (Vec::new(), Vec::new());
            for t in rows.iter().filter(|t| t.requirements.contains(req)) {
                items.push(t.judged.clone());
                ids.push(t.test_id.clone());
                if t.judged.verdict == Verdict::Skipped {
                    let reason = if t.reached { serde_json::to_value(t.judged.reason).unwrap_or(Value::Null) } else { t.reason.clone() };
                    skipped.push(json!({ "test_id": t.test_id, "reason": reason }));
                }
            }
            items.extend(manual.iter().filter(|m| m.matched && m.requirements.contains(req) && (m.counted || m.relayed)).map(|m| m.judged.clone()));
            match rules.requirement(&items) {
                Ok(r) => {
                    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
                    v["tests"] = json!(ids);
                    v["skipped_tests"] = json!(skipped);
                    requirements.insert(req.clone(), v);
                }
                Err(e) => c.fail(format!("requirement {req} could not be rolled up ({e:?})")),
            }
        }
        let all: Vec<Judged> = rows.iter().map(|t| t.judged.clone()).collect();
        let mut summary = match rules.combine(&all) {
            Ok(s) => s,
            Err(e) => {
                c.fail(format!("the run's summary could not be combined ({e:?})"));
                return Value::Null;
            }
        };
        if unit_key.is_none() {
            summary.qualifiers.push(Qualifier::UnitIdentityNotRead);
            summary.qualifiers.sort_unstable();
            summary.qualifiers.dedup();
        }

        let mut not_run: Vec<Value> = rows
            .iter()
            .filter(|t| t.judged.verdict == Verdict::Skipped)
            .map(|t| {
                let reason = if t.reached { serde_json::to_value(t.judged.reason).unwrap_or(Value::Null) } else { t.reason.clone() };
                json!({ "source": "run", "test_id": t.test_id, "requirements": t.requirements, "reason": reason })
            })
            .collect();
        let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
        for e in p["selection"]["entries"].as_array().into_iter().flatten() {
            let status = e["status"].as_str().unwrap_or("").to_string();
            if status != "selected" {
                not_run.push(json!({ "source": "selection", "test_id": e["test_id"], "status": e["status"], "reason": e["reason"] }));
            }
            *by_status.entry(status).or_default() += 1;
        }
        let coverage = json!({
            "tests_in_run": rows.len(),
            "tests_judged": rows.iter().filter(|t| t.reached && t.judged.verdict != Verdict::Skipped).count(),
            "tests_skipped": rows.iter().filter(|t| t.reached && t.judged.verdict == Verdict::Skipped).count(),
            "tests_not_reached": rows.iter().filter(|t| !t.reached).count(),
            "selection": by_status,
        });

        let tests: Vec<Value> = run
            .tests
            .iter()
            .zip(rows)
            .map(|(t, r)| {
                let rec = p["tests"].as_array().and_then(|a| a.iter().find(|x| x["test_id"] == t.test_id.as_str())).cloned().unwrap_or(Value::Null);
                json!({
                    "test_id": t.test_id, "bench": rec["bench"], "definition_hash": rec["definition_hash"], "requirements": rec["requirements"],
                    "verdict": rec["verdict"], "reason": rec["reason"],
                    "qualifiers": if r.reached { serde_json::to_value(&r.judged.qualifiers).unwrap_or(Value::Null) } else { json!([]) },
                    "counts": r.counts, "runs": r.runs, "note": rec["note"], "dut_image": rec["dut_image"],
                    "decision_rule": json!({ "kind": t.decision_rule.kind, "w_multiple": t.decision_rule.w_multiple }),
                    "limits": limits_of(t),
                    "study_ids": rec["study_records"].as_array().into_iter().flatten().map(|s| s["study_id"].clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let study_ids: Vec<Value> = p["tests"].as_array().into_iter().flatten().flat_map(|t| t["study_records"].as_array().into_iter().flatten()).map(|s| s["study_id"].clone()).collect();

        let mut data = json!({
            "schema": 1, "kind": "test_run", "run_id": p["run_id"],
            "revision": self.manifest.map_or(Value::Null, |m| m["predicate"]["revision"].clone()),
            "run_status": p["status"], "reason": p["reason"], "resumes": p["resumes"],
            "suite": { "id": run.suite.id, "version": run.suite.version, "commit": run.suite.commit },
            "standard": { "name": run.standard.name, "revision": run.standard.revision, "requirement_count": null, "atlas": { "joined": false, "note": NO_ATLAS } },
            "unit": {
                "unit_key": unit_key, "identity_read": unit_key.is_some(),
                "declared_serial": run.dut.declared_serial, "board_revision": run.dut.board_revision,
                "declared": if facts.unit_declared.is_null() { Value::Null } else {
                    json!({ "serial": facts.unit_declared["serial"], "board_revision": facts.unit_declared["board_revision"], "scope": facts.unit_declared["scope"], "at_utc_ms": facts.unit_declared["at_utc_ms"] })
                },
            },
            "person": if facts.person.is_null() { Value::Null } else { json!({ "name": facts.person["name"], "scope": facts.person["scope"], "at_utc_ms": facts.person["at_utc_ms"] }) },
            "go_by": facts.go_by, "published_by": facts.published_by,
            "statement": self.rd["statement"],
            "attestation": { "label": attestation_label(facts.person_acted), "core_attested": false },
            "summary": summary, "requirements": requirements, "tests": tests,
            "manual_verdicts": manual_listed, "not_run": not_run, "coverage": coverage,
            "runs_on_unit": runs_on_unit(lines, unit_key),
            "rules": self.rd["rules"],
            "core": { "version": self.manifest.map_or(Value::Null, |m| m["predicate"]["core"]["version"].clone()), "keyid": self.fingerprint },
            "record_sha256": self.files.get(TEST_RECORD_NAME).map(|b| sha(b)),
            "study_ids": study_ids,
        });
        let range = &p["observed"]["call_log"];
        if range.is_object() {
            let count = match (range["first_seq"].as_u64(), range["last_seq"].as_u64()) {
                (Some(f), Some(l)) if l >= f => l + 1 - f,
                _ => 0,
            };
            data["call_log"] = json!({ "head": range["head"], "first_seq": range["first_seq"], "last_seq": range["last_seq"], "count": count });
        }
        data
    }
}

// ---- check 3: report-data's verdicts against the recomputation ---------------

fn compare_verdict_parts(z: &Zip, d: &Derived, c: &mut Check) {
    if d.expected.is_null() {
        return;
    }
    for r in &d.rows {
        let Some(have) = z.rd["tests"].as_array().and_then(|a| a.iter().find(|t| t["test_id"] == r.test_id.as_str())) else {
            c.fail(format!("test {}: report-data.json has no entry for it", r.test_id));
            continue;
        };
        let want = d.expected["tests"].as_array().and_then(|a| a.iter().find(|t| t["test_id"] == r.test_id.as_str())).cloned().unwrap_or(Value::Null);
        for k in ["verdict", "reason", "qualifiers", "counts", "runs"] {
            if have[k] != want[k] {
                c.fail(format!("test {}: report-data.json's {k} is {}, the recomputation gives {}", r.test_id, have[k], want[k]));
            }
        }
    }
    for t in z.rd["tests"].as_array().into_iter().flatten() {
        if !d.rows.iter().any(|r| t["test_id"] == r.test_id.as_str()) {
            c.fail(format!("report-data.json lists test {}, which run.json did not select", t["test_id"]));
        }
    }
    let want_reqs = d.expected["requirements"].as_object().cloned().unwrap_or_default();
    let have_reqs = z.rd["requirements"].as_object().cloned().unwrap_or_default();
    for (id, want) in &want_reqs {
        match have_reqs.get(id) {
            Some(have) if have == want => {}
            Some(have) => c.fail(format!("requirement {id}: report-data.json says {have}, the roll-up over the recomputed verdicts gives {want}")),
            None => c.fail(format!("requirement {id}: report-data.json has no roll-up for it")),
        }
    }
    for id in have_reqs.keys().filter(|k| !want_reqs.contains_key(*k)) {
        c.fail(format!("requirement {id}: report-data.json lists it and no selected test names it"));
    }
    if z.rd["summary"] != d.expected["summary"] {
        c.fail(format!("the run's summary: report-data.json says {}, the roll-up gives {}", z.rd["summary"], d.expected["summary"]));
    }
    c.compared.push(format!(
        "{} requirements and the run's summary rolled up from the recomputed verdicts with the behaviours report-data.json names, and compared with it; report-data.json's rules are Core's claim of which behaviours it used, and a run's own verdicts are assumed judged with them (Core records none per run)",
        want_reqs.len()
    ));
}

// ---- check 5 -----------------------------------------------------------------

fn check_labels(z: &Zip, d: &Derived, c: &mut Check) {
    if z.lines.is_err() || z.manifest.is_none() {
        c.fail("the run-log segment or the signed manifest cannot be read".into());
        return;
    }
    let f = z.log_facts(&d.rows);
    let label = attestation_label(f.person_acted);
    if z.rd["attestation"]["label"] != label {
        c.fail(format!("the attestation label is {}, but the signed records say {label:?}", z.rd["attestation"]["label"]));
    }
    if z.rd["go_by"] != f.go_by || z.rd["published_by"] != f.published_by.as_str() {
        c.fail("go_by or published_by in report-data.json is not what the signed run-log entry and manifest say".into());
    }
    c.compared.push(format!("attestation label {label:?}, go_by {}, published_by {}, against the run-log segment's `test_run_started` entry, the manifest, the unit and person declarations and the manual verdicts that count", f.go_by, f.published_by));
    c.compared.push("a Test run's zip adopts no note or prose file, so none is checked".into());
    for name in z.files.keys().filter(|n| n.starts_with("notes/") || n.starts_with("prose/")) {
        c.fail(format!("{name} is in a Test run's zip, which takes no notes or prose"));
    }
}

// ---- check 6 -----------------------------------------------------------------

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A line with the version of the writer, which a later `verify` need not share.
fn versionless(src: &str) -> String {
    src.lines().filter(|l| !l.contains("embarch-api version") && !l.contains("Typst version")).collect::<Vec<_>>().join("\n")
}

fn check_report_claims(z: &Zip, d: &Derived, c: &mut Check) {
    let want = &d.expected;
    if want.is_null() {
        c.partial("not recomputed: the verdicts could not be rolled up (check 3)".into());
        return;
    }
    let mut differing: Vec<String> = Vec::new();
    for t in want["tests"].as_array().into_iter().flatten() {
        let have = z.rd["tests"].as_array().and_then(|a| a.iter().find(|h| h["test_id"] == t["test_id"]));
        for k in ["decision_rule", "limits"] {
            if have.is_some_and(|h| h[k] != t[k]) {
                c.fail(format!("test {}: report-data.json's {k} is not what run.json gives for it ({} against {})", show(&t["test_id"]), have.map_or(Value::Null, |h| h[k].clone()), t[k]));
            }
        }
    }
    for (k, w) in want.as_object().into_iter().flatten() {
        let have = &z.rd[k.as_str()];
        let same = if k == "attestation" { have["label"] == w["label"] && have["core_attested"] == w["core_attested"] } else { have == w };
        if !same {
            differing.push(k.clone());
        }
    }
    for k in z.rd.as_object().into_iter().flat_map(|o| o.keys()) {
        if want.get(k.as_str()).is_none() {
            differing.push(k.clone());
        }
    }
    if differing.is_empty() {
        c.compared.push(
            "every field of report-data.json recomputed from the signed records, run.json, result.json, each study run's files, the run-log segment and the manifest; the attestation's explanatory text is Core's wording and is not compared"
                .into(),
        );
    } else {
        c.fail(format!("report-data.json's {} is not what the signed records, the run log's segment and the manifest give", differing.join(", ")));
    }
    let rd = &z.rd;
    match report_source(rd) {
        Ok(src) if z.files.get(REPORT_FILE).map(Vec::as_slice) == Some(src.as_bytes()) => c.compared.push("report.typ is what report-data.json regenerates, byte for byte".into()),
        Ok(_) => c.fail("report.typ is not what report-data.json regenerates".into()),
        Err(e) => c.fail(format!("report.typ cannot be regenerated: {e:#}")),
    }
    let supersedes = z.manifest.map(|m| &m["predicate"]["supersedes"]).filter(|s| s.is_object()).map(|s| Supersedes {
        revision: s["revision"].as_u64().unwrap_or(0),
        manifest_sha256: s["manifest_sha256"].as_str().map(str::to_string),
        from_zip: false,
    });
    let pins = RunPins {
        run_json_sha256: &z.files.get(RUN_JSON_NAME).map(|b| sha(b)).unwrap_or_default(),
        plan_hash: z.run_value["plan_hash"].as_str(),
        revision: z.manifest.and_then(|m| m["predicate"]["revision"].as_u64()).unwrap_or(0),
        supersedes: supersedes.as_ref(),
    };
    let appendix = appendix_source(rd, &pins);
    match z.files.get(APPENDIX_FILE) {
        Some(b) if versionless(&String::from_utf8_lossy(b)) == versionless(&appendix) => {
            c.compared.push("appendix.typ is what report-data.json, run.json and the manifest regenerate (the two version lines, the writer's, aside)".into())
        }
        _ => c.fail("appendix.typ is not what report-data.json, run.json and the manifest regenerate".into()),
    }

    let Some(pdf) = z.files.get(REPORT_PDF) else {
        c.fail(format!("{REPORT_PDF} is not in the zip"));
        return;
    };
    let pages = match embarch_study_designer::pdf_text::extract_pages(pdf) {
        Ok(p) => p,
        Err(e) => {
            c.fail(format!("the PDF cannot be read, so what it prints of report-data.json is not checked: {e}"));
            return;
        }
    };
    let printed = squash(&pages.iter().flat_map(embarch_study_designer::pdf_text::words).map(|w| w.text).collect::<Vec<_>>().join(" "));
    let text = |k: &str| match &want[k] {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let claims: Vec<(&str, String)> = vec![
        ("the run's id", text("run_id")),
        ("the run's status", text("run_status")),
        ("the attestation label", want["attestation"]["label"].as_str().unwrap_or("").to_string()),
        ("who began the run", text("go_by")),
        ("who published it", text("published_by")),
        ("the revision", format!("This is revision {}.", want["revision"])),
    ];
    for (what, t) in &claims {
        if printed.contains(&squash(t)) {
            c.compared.push(format!("the PDF prints {what}: {t:?}"));
        } else {
            c.fail(format!("the PDF does not print {what} as report-data.json gives it ({t:?})"));
        }
    }
}

// ---- check 7 -----------------------------------------------------------------

fn check_exports(z: &Zip) -> Check {
    let mut c = Check::new(7, "the exports");
    if z.rd.is_null() {
        c.fail("report-data.json cannot be read".into());
        return c;
    }
    let measurements: BTreeMap<String, Value> = z.studies.iter().filter_map(|(id, s)| Some((id.clone(), s.measurements.clone()?))).collect();
    let inputs = TestExportInputs { report_data: &z.rd, measurements: &measurements };
    for (name, regenerated) in [(JUNIT_NAME, junit_xml(&inputs)), (OCP_NAME, ocp_jsonl(&inputs))] {
        match z.files.get(name) {
            None => c.fail(format!("{name} is not in the zip")),
            Some(b) if b.as_slice() != regenerated.as_bytes() => {
                c.fail(format!("{name} differs from what the signed data regenerates ({} bytes in the zip, {} regenerated)", b.len(), regenerated.len()))
            }
            Some(b) => c.compared.push(format!("{name}: regenerated from report-data.json and each study run's measurements.json, byte for byte ({} bytes)", b.len())),
        }
    }
    c
}

// ---- check 8 -----------------------------------------------------------------

fn check_log_segment(z: &Zip) -> Check {
    let mut c = Check::new(8, "the run log's segment");
    let lines = match &z.lines {
        Ok(l) => l,
        Err(e) => {
            c.fail(e.clone());
            return c;
        }
    };
    if !check_chain(&mut c, lines, z.manifest) {
        return c;
    }
    let run_id = z.predicate()["run_id"].as_str().unwrap_or("");
    let mine: Vec<&Value> = lines.iter().map(|l| &l["entry"]).filter(|e| e["run_id"] == run_id).collect();
    let ends: Vec<&&Value> = mine.iter().filter(|e| e["type"] == "test_run").collect();
    if ends.len() != 1 {
        c.fail(format!("the segment holds {} `test_run` entries for this run, not one", ends.len()));
    } else {
        let end = ends[0];
        if end["record_sha256"].as_str() != z.files.get(TEST_RECORD_NAME).map(|b| sha(b)).as_deref() {
            c.fail("the `test_run` entry's record_sha256 is not the hash of test-run.dsse.json".into());
        }
        for s in end["study_records"].as_array().into_iter().flatten() {
            let id = s["study_id"].as_str().unwrap_or("");
            if s["record_sha256"].as_str() != z.files.get(&format!("{STUDIES}{id}/{STUDY_RECORD}")).map(|b| sha(b)).as_deref() {
                c.fail(format!("the `test_run` entry's record for study {id} is not the zip's"));
            }
        }
        c.compared.push("the `test_run` entry names this run's signed record and each study run's record by hash".into());
    }
    let discarded: Vec<String> = lines
        .iter()
        .map(|l| &l["entry"])
        .filter(|e| e["type"] == "discarded" && e["study_id"].as_str().is_some_and(|s| z.studies.contains_key(s)))
        .map(|e| e["study_id"].as_str().unwrap_or("").to_string())
        .collect();
    if !discarded.is_empty() {
        c.fail(format!("the segment holds a discard of the captures of {}", discarded.join(", ")));
    }
    c.compared.push(format!("{} entries keyed on this run", mine.len()));
    check_call_log(&mut c, z.files, z.record);
    c.compared.push("the publish's own entry and any later revision are after the segment's head and cannot be checked offline".into());
    c
}
