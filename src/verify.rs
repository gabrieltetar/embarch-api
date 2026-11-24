//! `embarch-api verify <zip>`: a published Study zip checked offline, from the
//! zip and a public key alone (`embarch-api` decisions 118 to 121;
//! `interfaces/publish.md`, "verify"; `notes/hil/06`, "Verify").
//!
//! **Nothing here asks Core anything or reads a clock.** Every check answers
//! with what it compared and a [`Status`]; a check this version does not make
//! says [`Status::NotChecked`], never a pass.
//!
//! Checks 4 (the PDF's text), 6 (`report-data.json` recomputed) and 9 (the
//! syntax-tree whitelist of `report.typ`) are in `verify_report` (decisions 122
//! to 125); 3 is a Test's and does not apply to a Study.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use embarch_study_designer::reducers::{Input, Marker, Point, ReduceError, Registry, Window};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::publish::{slug, JUNIT_NAME, MANIFEST_NAME, MEASUREMENTS_NAME, OCP_NAME, REPORT_DATA_NAME};
use crate::publish_export::{junit_xml, ocp_jsonl, ExportInputs, StepRow};

pub(crate) const RECORD_NAME: &str = "record.dsse.json";
const REPORT_DATA_ENVELOPE: &str = "report-data.dsse.json";
const SEGMENT_NAME: &str = "run-log-segment.jsonl";
const CALL_SEGMENT_NAME: &str = "call-log-segment.jsonl";
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub(crate) const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The words `notes/hil/06` gives an unattested key's result.
pub const UNATTESTED: &str = "origin unattested: this key does not match an attested fingerprint, so its signatures \
prove integrity only. The agent can read an unattested Core's key, so the numbers and verdicts may have been \
produced by the agent, whatever the record claims about itself";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Everything this check compares matched.
    Pass,
    /// Something compared did not match, or a required file is missing.
    Fail,
    /// What was compared matched, and part of what the check names was not
    /// recomputed (said line by line).
    Partial,
    /// This version does not make the check.
    NotChecked,
    /// The check is a Test's.
    NotApplicable,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "FAIL",
            Status::Partial => "partial",
            Status::NotChecked => "not checked by this version",
            Status::NotApplicable => "not applicable",
        }
    }
}

#[derive(Debug)]
pub struct Check {
    pub id: u8,
    pub name: &'static str,
    pub status: Status,
    /// What it compared, one line each.
    pub compared: Vec<String>,
    /// What did not match, or was not recomputed.
    pub problems: Vec<String>,
}

impl Check {
    pub(crate) fn new(id: u8, name: &'static str) -> Self {
        Check { id, name, status: Status::Pass, compared: Vec::new(), problems: Vec::new() }
    }
    pub(crate) fn fail(&mut self, why: String) {
        self.status = Status::Fail;
        self.problems.push(why);
    }
    pub(crate) fn partial(&mut self, why: String) {
        if self.status == Status::Pass {
            self.status = Status::Partial;
        }
        self.problems.push(why);
    }
    fn fixed(id: u8, name: &'static str, status: Status, why: &str) -> Self {
        Check { id, name, status, compared: Vec::new(), problems: vec![why.to_string()] }
    }
}

#[derive(Debug)]
pub struct Report {
    pub zip: String,
    pub produced_by: Option<String>,
    pub verified_by: &'static str,
    pub key_fingerprint: String,
    pub attested: bool,
    pub checks: Vec<Check>,
}

impl Report {
    /// No check failed. A [`Status::Partial`] or [`Status::NotChecked`] is not a failure.
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.status != Status::Fail)
    }
    /// Every check ran in full and passed (or does not apply).
    pub fn complete(&self) -> bool {
        self.checks.iter().all(|c| matches!(c.status, Status::Pass | Status::NotApplicable))
    }

    pub fn to_json(&self) -> Value {
        json!({
            "success": self.ok(), "complete": self.complete(), "zip": self.zip,
            "produced_by_embarch_api": self.produced_by, "verified_by_embarch_api": self.verified_by,
            "key_fingerprint": self.key_fingerprint, "attested": self.attested,
            "origin": if self.attested { "attested" } else { UNATTESTED },
            "checks": self.checks.iter().map(|c| json!({
                "check": c.id, "name": c.name, "status": c.status.word(),
                "compared": c.compared, "problems": c.problems,
            })).collect::<Vec<_>>(),
        })
    }

    pub fn to_text(&self) -> String {
        let mut o = format!("verify {}\n", self.zip);
        o.push_str(&format!(
            "  produced by embarch-api {}; verified by embarch-api {}\n  key fingerprint {}: {}\n",
            self.produced_by.as_deref().unwrap_or("(not recorded)"),
            self.verified_by,
            self.key_fingerprint,
            if self.attested { "attested" } else { UNATTESTED }
        ));
        for c in &self.checks {
            o.push_str(&format!("check {} ({}): {}\n", c.id, c.name, c.status.word()));
            for l in &c.compared {
                o.push_str(&format!("    compared: {l}\n"));
            }
            for l in &c.problems {
                o.push_str(&format!("    {l}\n"));
            }
        }
        o.push_str(if !self.ok() {
            "RESULT: FAILED\n"
        } else if self.complete() {
            "RESULT: every check passed\n"
        } else {
            "RESULT: no check failed; some were partial or not checked by this version (above)\n"
        });
        o
    }
}

pub struct Options<'a> {
    /// Core's public key, 64 hex characters: the one whose SHA-256 is the key
    /// fingerprint, got from the engineer and never from the zip.
    pub public_key_hex: &'a str,
    /// The attested fingerprint from Core's elevated install, when the reader has one.
    pub attested_fingerprint: Option<&'a str>,
}

pub(crate) fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn read_zip(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?)
        .context("not a zip")?;
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b)?;
        out.insert(f.name()?.to_string(), b);
    }
    Ok(out)
}

/// A DSSE envelope's payload and whether its signature verifies under `key`.
fn open_envelope(bytes: &[u8], key: &VerifyingKey, fingerprint: &str) -> Result<Value, String> {
    let env: Value = serde_json::from_slice(bytes).map_err(|e| format!("is not JSON: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let payload_type = env["payloadType"].as_str().unwrap_or("");
    if payload_type != PAYLOAD_TYPE {
        return Err(format!("has payloadType {payload_type:?}, not {PAYLOAD_TYPE}"));
    }
    let payload = b64.decode(env["payload"].as_str().unwrap_or("")).map_err(|_| "payload is not base64".to_string())?;
    let sigs = env["signatures"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if sigs.is_empty() {
        return Err("carries no signature".into());
    }
    let mut signed = format!("DSSEv1 {} {} {} ", payload_type.len(), payload_type, payload.len()).into_bytes();
    signed.extend_from_slice(&payload);
    let mut last = String::from("no signature names this key");
    for s in sigs {
        if s["keyid"].as_str() != Some(fingerprint) {
            last = format!("is signed under key {}, not the given key {fingerprint}", s["keyid"].as_str().unwrap_or("(none)"));
            continue;
        }
        let raw = b64.decode(s["sig"].as_str().unwrap_or("")).unwrap_or_default();
        let Ok(arr) = <[u8; 64]>::try_from(raw.as_slice()) else {
            last = "signature is not 64 bytes".into();
            continue;
        };
        if key.verify_strict(&signed, &Signature::from_bytes(&arr)).is_ok() {
            return serde_json::from_slice(&payload).map_err(|_| "signed payload is not JSON".to_string());
        }
        last = "signature does not verify".into();
    }
    Err(last)
}

fn subjects(statement: &Value) -> BTreeMap<String, String> {
    statement["subject"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| Some((s["name"].as_str()?.to_string(), s["digest"]["sha256"].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn json_of(files: &BTreeMap<String, Vec<u8>>, name: &str) -> Option<Value> {
    serde_json::from_slice(files.get(name)?).ok()
}

/// What the embarch-api that wrote the zip says it was, from the appendix's
/// pinned-inputs table (a claim of embarch-api's, not a signed fact).
fn produced_by(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
    let text = String::from_utf8_lossy(files.get("appendix.typ")?).into_owned();
    let at = text.find("\"embarch-api version\"")?;
    let rest = &text[at + "\"embarch-api version\"".len()..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string())
}

/// The whole of `verify`: opens `path`, runs checks 0 to 9 as far as this version makes them.
pub fn verify_zip(path: &Path, opts: &Options) -> Result<Report> {
    let key_bytes = unhex(opts.public_key_hex.trim())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| anyhow!("--public-key must be 64 hex characters (Core's raw Ed25519 public key)"))?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|e| anyhow!("--public-key is not an Ed25519 key: {e}"))?;
    let fingerprint = sha(&key_bytes);
    let attested = opts.attested_fingerprint.is_some_and(|a| a.trim().eq_ignore_ascii_case(&fingerprint));
    let files = read_zip(path)?;

    let mut checks = Vec::new();
    let (c0, manifest, record) = check_signatures(&files, &key, &fingerprint, attested);
    checks.push(c0);
    checks.push(check_manifest(&files, manifest.as_ref()));
    checks.push(check_measurements(&files, record.as_ref()));
    checks.push(Check::fixed(3, "verdicts", Status::NotApplicable, "a Study has no verdict; nothing to recompute"));
    checks.push(crate::verify_report::check_pdf_text(&files));
    checks.push(check_labels(&files, manifest.as_ref(), record.as_ref()));
    checks.push(crate::verify_report::check_report_claims(&files, manifest.as_ref(), record.as_ref(), &fingerprint));
    checks.push(check_exports(&files));
    checks.push(check_log_segment(&files, manifest.as_ref(), record.as_ref()));
    checks.push(crate::verify_report::check_pictures(&files));
    checks.sort_by_key(|c| c.id);

    Ok(Report {
        zip: path.display().to_string(),
        produced_by: produced_by(&files),
        verified_by: env!("CARGO_PKG_VERSION"),
        key_fingerprint: fingerprint,
        attested,
        checks,
    })
}

// ---- check 0 -----------------------------------------------------------------

fn check_signatures(
    files: &BTreeMap<String, Vec<u8>>,
    key: &VerifyingKey,
    fingerprint: &str,
    attested: bool,
) -> (Check, Option<Value>, Option<Value>) {
    let mut c = Check::new(0, "the signatures");
    let open = |name: &str, c: &mut Check| -> Option<Value> {
        let Some(bytes) = files.get(name) else {
            c.fail(format!("{name} is not in the zip"));
            return None;
        };
        match open_envelope(bytes, key, fingerprint) {
            Ok(statement) => {
                c.compared.push(format!("{name}: Ed25519 signature verifies under key {fingerprint}"));
                Some(statement)
            }
            Err(why) => {
                c.fail(format!("{name} {why}"));
                None
            }
        }
    };
    let manifest = open(MANIFEST_NAME, &mut c);
    let record = open(RECORD_NAME, &mut c);
    let rd_env = open(REPORT_DATA_ENVELOPE, &mut c);

    if let Some(st) = &rd_env {
        let named = subjects(st).get(REPORT_DATA_NAME).cloned();
        let have = files.get(REPORT_DATA_NAME).map(|b| sha(b));
        if named.is_none() || named != have {
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
                None => c.fail(format!("the signed record names {name}, which the zip lacks")),
                Some(b) if &sha(b) != digest => c.fail(format!("{name} does not match the digest the signed record holds")),
                _ => {}
            }
        }
        c.compared.push(format!("the signed record's {} subjects hash to the zip's files", subs.len()));
        for name in files.keys() {
            let core_run_file = matches!(name.as_str(), "events.json" | MEASUREMENTS_NAME | "study.json") || name.starts_with("streams/");
            if core_run_file && !subs.contains_key(name) {
                c.fail(format!("{name} is not a subject of the signed record"));
            }
        }
    }
    if c.status == Status::Fail {
        return (c, manifest, record);
    }
    if attested {
        c.compared.push("the key is the attested fingerprint the reader gave".into());
    } else {
        c.compared.push(UNATTESTED.to_string());
    }
    (c, manifest, record)
}

// ---- check 1 -----------------------------------------------------------------

fn check_manifest(files: &BTreeMap<String, Vec<u8>>, manifest: Option<&Value>) -> Check {
    let mut c = Check::new(1, "the manifest");
    let Some(m) = manifest else {
        c.fail("no manifest could be read (check 0)".into());
        return c;
    };
    let subs = subjects(m);
    for (name, digest) in &subs {
        match files.get(name) {
            None => c.fail(format!("the manifest names {name}, which the zip lacks")),
            Some(b) if &sha(b) != digest => c.fail(format!("{name}: SHA-256 differs from the manifest's")),
            _ => {}
        }
    }
    let listed: BTreeMap<String, String> = m["predicate"]["embarch_api_files"]
        .as_array()
        .map(|a| {
            a.iter().filter_map(|f| Some((f["name"].as_str()?.to_string(), f["sha256"].as_str()?.to_string()))).collect()
        })
        .unwrap_or_default();
    for (name, digest) in &listed {
        match files.get(name) {
            None => c.fail(format!("the manifest lists {name} as embarch-api's, which the zip lacks")),
            Some(b) if &sha(b) != digest => c.fail(format!("{name}: SHA-256 differs from the one embarch-api sent at publish")),
            _ => {}
        }
    }
    for name in files.keys() {
        if name != MANIFEST_NAME && !subs.contains_key(name) && !listed.contains_key(name) {
            c.fail(format!("{name} is in the zip and in neither list of the manifest"));
        }
    }
    for (what, key, file) in [("record", "record_sha256", RECORD_NAME), ("report-data", "report_data_sha256", REPORT_DATA_NAME)] {
        if let (Some(want), Some(b)) = (m["predicate"][key].as_str(), files.get(file)) {
            if sha(b) != want {
                c.fail(format!("the manifest's {key} is not the hash of {file}: the {what} was swapped"));
            }
        }
    }
    c.compared.push(format!(
        "{} files Core signed and {} files embarch-api listed, each SHA-256 against the manifest; every file in the zip is one or the other",
        subs.len(),
        listed.len()
    ));
    c.compared.push("embarch-api's files are listed, not vouched for by Core's signature: checks 5 and 7 stand behind them".into());
    c
}

// ---- check 2 -----------------------------------------------------------------

/// The encoding's kind: what column a tap's rendered rows are read from.
enum Tap {
    Column(String),
    CountRows,
    Empty,
    Unsupported(&'static str),
}

fn tap_reader(encoding: &Value, field: Option<&str>) -> Tap {
    match encoding {
        Value::String(s) => match s.as_str() {
            "GattTranscript" => Tap::CountRows,
            "Raw" => Tap::Empty,
            "Text" => Tap::Unsupported("a Text tap's markers need its arrival stamps"),
            _ => Tap::Unsupported("an outpost trace is not recomputed by this version"),
        },
        Value::Object(o) if o.contains_key("Samples") => Tap::Column("value".into()),
        Value::Object(o) if o.contains_key("Struct") => match field {
            Some(f) => Tap::Column(f.to_string()),
            None => Tap::CountRows,
        },
        _ => Tap::Unsupported("an encoding this version does not read"),
    }
}

fn unit_of(tap: &Value, decl: &Value) -> String {
    if let Some(u) = decl["unit"].as_str() {
        return u.to_string();
    }
    match tap["encoding"]["Samples"]["unit"].as_str() {
        Some("Milliamps") => "mA",
        Some("Volts") => "V",
        Some("Milliwatts") => "mW",
        _ => "1",
    }
    .to_string()
}

fn csv_points(csv: &[u8], column: Option<&str>) -> Vec<Point> {
    let text = String::from_utf8_lossy(csv);
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
    let value_at = column.and_then(|c| {
        let from = header.iter().position(|h| *h == "rep_index").map_or(0, |i| i + 1);
        header.iter().skip(from).position(|h| *h == c).map(|i| i + from)
    });
    let mut points = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let cells: Vec<&str> = line.split(',').collect();
        let Some(t_ms) = cells.last().and_then(|c| c.parse::<u64>().ok()) else { continue };
        let value = match value_at {
            Some(i) => cells.get(i).and_then(|c| c.parse::<f64>().ok()).unwrap_or(f64::NAN),
            None => 0.0,
        };
        points.push(Point { t_ms, value });
    }
    points
}

fn check_measurements(files: &BTreeMap<String, Vec<u8>>, record: Option<&Value>) -> Check {
    let mut c = Check::new(2, "the measurements");
    let study = json_of(files, "study.json");
    let declared: Vec<Value> = study.as_ref().and_then(|s| s["measurements"].as_array().cloned()).unwrap_or_default();
    let Some(ms_file) = json_of(files, MEASUREMENTS_NAME) else {
        if declared.is_empty() && !files.contains_key(MEASUREMENTS_NAME) {
            c.compared.push("the study declares no measurement and the zip has no measurements.json".into());
        } else {
            c.fail("measurements.json is missing or not JSON, and the signed study file declares measurements".into());
        }
        c.partial("step outcomes were not re-evaluated against their recorded replies: not recomputed by this version".into());
        return c;
    };
    let Some(study) = study else {
        c.fail("study.json is missing or not JSON, so no measurement can be recomputed".into());
        return c;
    };
    let registry = Registry::builtin();
    let rows = ms_file["measurements"].as_array().cloned().unwrap_or_default();
    let by_name: BTreeMap<&str, &Value> = rows.iter().filter_map(|m| Some((m["name"].as_str()?, m))).collect();
    let signed_behaviours = record.map(|r| &r["predicate"]["observed"]["behaviours"]);
    if signed_behaviours.is_some_and(|b| *b != ms_file["behaviours"]) {
        c.fail("measurements.json's behaviours differ from the ones the signed record holds".into());
    }
    let declared_names: BTreeSet<&str> = declared.iter().filter_map(|d| d["name"].as_str()).collect();
    for name in by_name.keys() {
        if !declared_names.contains(name) {
            c.fail(format!("measurement {name} is not one the signed study file declares"));
        }
    }
    let mut matched = 0;
    for decl in &declared {
        let name = decl["name"].as_str().unwrap_or("(unnamed)");
        let Some(m) = by_name.get(name) else {
            c.fail(format!("{name}: declared by the study, absent from measurements.json"));
            continue;
        };
        let reducer = decl["reducer"].as_str().unwrap_or("");
        let tap = study["streams"].as_array().and_then(|a| a.iter().find(|t| t["id"] == decl["tap"]));
        let Some(tap) = tap else {
            c.fail(format!("{name}: its tap is not in the signed study file"));
            continue;
        };
        if matches!(reducer, "interval" | "value_at_marker") {
            c.partial(format!("{name}: not recomputed ({reducer} reads markers from a text capture, which this version does not read)"));
            continue;
        }
        let tap_name = tap["name"].as_str().unwrap_or("");
        let points = match tap_reader(&tap["encoding"], decl["field"].as_str()) {
            Tap::Unsupported(why) => {
                c.partial(format!("{name}: not recomputed ({why})"));
                continue;
            }
            Tap::Empty => Vec::new(),
            Tap::Column(col) => match files.get(&format!("streams/{tap_name}.csv")) {
                Some(b) => csv_points(b, Some(&col)),
                None => {
                    c.fail(format!("{name}: streams/{tap_name}.csv, the capture's rendering, is not in the zip"));
                    continue;
                }
            },
            Tap::CountRows => match files.get(&format!("streams/{tap_name}.csv")) {
                Some(b) => csv_points(b, None),
                None => {
                    c.fail(format!("{name}: streams/{tap_name}.csv, the capture's rendering, is not in the zip"));
                    continue;
                }
            },
        };
        let behaviour = signed_behaviours
            .and_then(|b| b[reducer].as_u64())
            .or_else(|| m["method"]["behaviour"].as_u64())
            .unwrap_or(0) as u32;
        let (from, to) = match (points.first(), points.last()) {
            (Some(a), Some(b)) => (Some(a.t_ms), Some(b.t_ms)),
            _ => (None, None),
        };
        let input = Input {
            points,
            markers: Vec::<Marker>::new(),
            unit: unit_of(tap, decl),
            u: None,
            clock_u_ms: None,
            k: embarch_study_designer::reducers::DEFAULT_K,
            incomplete: Vec::new(),
            window: Window { from_utc_ms: from, to_utc_ms: to, percentile: decl["window"]["percentile"].as_f64(), ..Window::default() },
        };
        match registry.reduce(reducer, behaviour, &input) {
            Err(ReduceError::UnknownBehaviour { name: r, behaviour: b }) => {
                c.partial(format!("{name}: not recomputed: the signed record names {r} behaviour {b}, which this build lacks (a verify older than the Core)"));
            }
            Err(ReduceError::UnknownReducer(r)) => c.fail(format!("{name}: {r} is not a reducer this build knows")),
            Ok(got) => {
                let have = &m["value"];
                let same = match (got.value, have) {
                    (None, Value::Null) => true,
                    (Some(g), v) => v.as_f64() == Some(g),
                    _ => false,
                };
                if m["method"]["name"] != reducer || m["method"]["behaviour"] != behaviour {
                    c.fail(format!("{name}: its method is not {reducer} behaviour {behaviour}, which the signed record names"));
                } else if same {
                    matched += 1;
                    c.compared.push(format!("{name}: {reducer} v{behaviour} over streams/{tap_name}.csv recomputes to {have}, as measurements.json says"));
                } else {
                    c.fail(format!("{name}: recomputed {:?}, measurements.json says {have}", got.value));
                }
            }
        }
    }
    c.compared.push(format!("{matched} of {} declared measurements recomputed from the raw captures and matched", declared.len()));
    c.compared.push("what Core read from the hardware (identities, the image, an instrument's reply) rests on Core's signature, not on a recompute".into());
    c.partial("step outcomes were not re-evaluated against their recorded replies: not recomputed by this version".into());
    c
}

// ---- check 5 -----------------------------------------------------------------

fn attribution(kind: &str, scope: &str) -> &'static str {
    match (kind, scope) {
        ("note", "agent") => "relayed by the agent",
        ("note", _) => "the person's",
        (_, "agent") => "entered with the agent's token",
        _ => "entered with the person's token",
    }
}

pub(crate) fn segment_lines(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(files.get(SEGMENT_NAME).ok_or_else(|| anyhow!("{SEGMENT_NAME} is not in the zip"))?)?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|_| anyhow!("a segment line is not JSON")))
        .collect()
}

/// The latest adoption of each `(kind, name)` for run `id`, as Core's
/// `report-data.json` lists them, and the run's own entry.
pub(crate) fn adoptions<'a>(lines: &'a [Value], id: &str) -> (Vec<Value>, Option<&'a Value>) {
    let mut latest: BTreeMap<(String, String), Value> = BTreeMap::new();
    let mut run: Option<&Value> = None;
    for l in lines {
        let e = &l["entry"];
        if e["study_id"] != id {
            continue;
        }
        if e["type"] == "adopted" {
            let (kind, name) = (e["kind"].as_str().unwrap_or("").to_string(), e["name"].as_str().unwrap_or("").to_string());
            latest.insert(
                (kind.clone(), name),
                json!({ "kind": e["kind"], "name": e["name"], "text_sha256": e["text_sha256"], "scope": e["scope"],
                        "attribution": attribution(&kind, e["scope"].as_str().unwrap_or("agent")), "at_utc_ms": e["at_utc_ms"] }),
            );
        } else if e["type"] == "run" {
            run = Some(e);
        }
    }
    (latest.into_values().collect(), run)
}

fn check_labels(files: &BTreeMap<String, Vec<u8>>, manifest: Option<&Value>, record: Option<&Value>) -> Check {
    let mut c = Check::new(5, "the labels");
    let (Some(rd), Ok(lines)) = (json_of(files, REPORT_DATA_NAME), segment_lines(files)) else {
        c.fail("report-data.json or the run-log segment cannot be read".into());
        return c;
    };
    let id = rd["study_id"].as_str().unwrap_or("");
    let (expected, run) = adoptions(&lines, id);
    let go_by = run.and_then(|r| r["go_by"].as_str()).unwrap_or("agent");
    let published_by = manifest.and_then(|m| m["predicate"]["published_by"].as_str()).unwrap_or("agent");
    let declared_person = record
        .and_then(|r| r["predicate"]["observed"]["declarations"]["entries"].as_array())
        .is_some_and(|e| e.iter().any(|d| d["scope"] == "person"));
    let person_acted =
        go_by == "person" || published_by == "person" || expected.iter().any(|a| a["scope"] == "person") || declared_person;
    let label = if person_acted { "person's acts not protected" } else { "agent-scoped only" };
    if rd["attestation"]["label"] != label {
        c.fail(format!(
            "the attestation label is {}, but the signed records say {label:?}",
            rd["attestation"]["label"]
        ));
    }
    if rd["go_by"] != go_by || rd["published_by"] != published_by {
        c.fail("go_by or published_by in report-data.json is not what the signed run entry and manifest say".into());
    }
    c.compared.push(format!("attestation label {label:?}, go_by {go_by}, published_by {published_by}, against the signed run entry, manifest, declarations and adoptions"));
    for a in &expected {
        let name = a["name"].as_str().unwrap_or("");
        match files.get(name) {
            None => c.fail(format!("{name} was adopted through Core but is not in the zip")),
            Some(b) if Some(sha(b).as_str()) != a["text_sha256"].as_str() => {
                c.fail(format!("{name} does not hash to the text Core recorded when it was adopted"))
            }
            _ => c.compared.push(format!("{name}: hashes to the text Core recorded ({})", a["attribution"].as_str().unwrap_or(""))),
        }
    }
    let adopted_names: BTreeSet<&str> = expected.iter().filter_map(|a| a["name"].as_str()).collect();
    for name in files.keys().filter(|n| n.starts_with("notes/") || n.starts_with("prose/")) {
        if !adopted_names.contains(name.as_str()) {
            c.fail(format!("{name} is in the zip but Core never recorded it as adopted"));
        }
    }
    c
}

// ---- check 7 -----------------------------------------------------------------

/// `events.json`'s steps as the export reads them (the same reduction Core's
/// `GET /study/{id}/steps` makes).
fn step_rows(events: &Value) -> Vec<StepRow> {
    events["steps"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    let (outcome, reason) = match &s["outcome"] {
                        Value::String(o) => (o.clone(), None),
                        Value::Object(m) => match m.iter().next() {
                            Some((n, body)) => (n.clone(), body["reason"].as_str().map(str::to_string)),
                            None => ("(unrecorded)".to_string(), None),
                        },
                        other => (other.to_string(), None),
                    };
                    StepRow {
                        name: s["step_name"].as_str().unwrap_or("").to_string(),
                        outcome,
                        reason,
                        started_utc_ms: s["started_utc_ms"].as_u64(),
                        ended_utc_ms: s["ended_utc_ms"].as_u64(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn check_exports(files: &BTreeMap<String, Vec<u8>>) -> Check {
    let mut c = Check::new(7, "the exports");
    let (Some(rd), Some(events)) = (json_of(files, REPORT_DATA_NAME), json_of(files, "events.json")) else {
        c.fail("report-data.json or events.json cannot be read".into());
        return c;
    };
    let measurements = json_of(files, MEASUREMENTS_NAME);
    let steps = step_rows(&events);
    let run_id = rd["study_id"].as_str().unwrap_or("").to_string();
    let slug_name = slug(rd["subject"]["study_name"].as_str().unwrap_or(""));
    let inputs = ExportInputs { slug: &slug_name, run_id: &run_id, report_data: &rd, measurements: measurements.as_ref(), steps: &steps };
    for (name, regenerated) in [(JUNIT_NAME, junit_xml(&inputs)), (OCP_NAME, ocp_jsonl(&inputs))] {
        match files.get(name) {
            None => c.fail(format!("{name} is not in the zip")),
            Some(b) if b.as_slice() != regenerated.as_bytes() => {
                c.fail(format!("{name} differs from what the signed data regenerates ({} bytes in the zip, {} regenerated)", b.len(), regenerated.len()))
            }
            Some(b) => c.compared.push(format!("{name}: regenerated from the signed data, byte for byte ({} bytes)", b.len())),
        }
    }
    c
}

// ---- check 8 -----------------------------------------------------------------

fn check_log_segment(files: &BTreeMap<String, Vec<u8>>, manifest: Option<&Value>, record: Option<&Value>) -> Check {
    let mut c = Check::new(8, "the run log's segment");
    let lines = match segment_lines(files) {
        Ok(l) => l,
        Err(e) => {
            c.fail(format!("{e:#}"));
            return c;
        }
    };
    let mut prev: Option<String> = None;
    let mut seq: Option<u64> = None;
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        let (entry, hash) = (&l["entry"], l["hash"].as_str().unwrap_or(""));
        if entry.is_null() || sha(&serde_json::to_vec(entry).unwrap_or_default()) != hash {
            c.fail(format!("line {n}: the entry does not match its hash: it was altered"));
            return c;
        }
        let this_prev = entry["prev"].as_str().unwrap_or("").to_string();
        match &prev {
            Some(p) if *p != this_prev => {
                c.fail(format!("line {n}: its prev is not the previous entry's hash: an entry was removed, added or reordered"));
                return c;
            }
            None if i == 0 && this_prev != GENESIS => {
                c.problems.push(format!("the segment starts at a later entry than the log's first (prev {}…)", &this_prev[..this_prev.len().min(8)]))
            }
            _ => {}
        }
        let this_seq = entry["seq"].as_u64();
        if seq.is_some_and(|s| this_seq != Some(s + 1)) || this_seq.is_none() {
            c.fail(format!("line {n}: its seq does not follow the previous entry's"));
            return c;
        }
        seq = this_seq;
        prev = Some(hash.to_string());
    }
    c.compared.push(format!("{} entries: every hash and prev recomputes from the first entry", lines.len()));
    if let Some(m) = manifest {
        let head = m["predicate"]["run_log"]["head"].as_str();
        let length = m["predicate"]["run_log"]["length"].as_u64();
        if prev.as_deref() != head || length != Some(lines.len() as u64) {
            c.fail("the segment does not end at the head, or hold the number of entries, the signed manifest names (an entry was dropped from the end, or the segment is another's)".into());
        } else {
            c.compared.push("the chain ends at the signed head, and its length is the signed one".into());
        }
    } else {
        c.fail("no signed manifest to compare the chain's head with".into());
    }
    let rd = json_of(files, REPORT_DATA_NAME).unwrap_or(Value::Null);
    let id = rd["study_id"].as_str().unwrap_or("");
    let mine: Vec<&Value> = lines.iter().map(|l| &l["entry"]).filter(|e| e["study_id"] == id).collect();
    let runs: Vec<&&Value> = mine.iter().filter(|e| e["type"] == "run").collect();
    if runs.len() != 1 {
        c.fail(format!("the segment holds {} run entries for this run, not one", runs.len()));
    } else {
        let run = runs[0];
        if let Some(rec) = files.get(RECORD_NAME) {
            if run["record_sha256"].as_str() != Some(sha(rec).as_str()) {
                c.fail("the run entry's record_sha256 is not the hash of record.dsse.json".into());
            }
        }
        for cap in run["captures"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = format!("streams/{}", cap["name"].as_str().unwrap_or(""));
            match files.get(&name) {
                Some(b) if Some(sha(b).as_str()) == cap["sha256"].as_str() => {}
                _ => c.fail(format!("{name}: the run entry's capture hash is not the zip's")),
            }
        }
        c.compared.push("the run entry names this run's signed record and every capture's hash".into());
    }
    if mine.iter().any(|e| e["type"] == "discarded") {
        c.fail("the segment holds a discard of this run's captures".into());
    }
    let types: Vec<String> = mine.iter().filter_map(|e| e["type"].as_str().map(str::to_string)).collect();
    c.compared.push(format!("entries keyed on this run: {}", types.join(", ")));
    check_call_log(&mut c, files, record);
    c.compared.push(
        "the publish's own entry and any later revision are after the segment's head and cannot be checked offline: ask Core, or a log head supplied with the zip"
            .into(),
    );
    c
}

/// Check 8's second half: `call-log-segment.jsonl` against the `observed.call_log`
/// range the signed record names (`embarch-core` decisions 111, 116, 117). The
/// hash rule is Core's `CallLog::segment_bytes`: the SHA-256 of the entry's
/// `serde_json` bytes, `prev` the entry before's hash (the header's `prev` for
/// the first), `seq` from `first_seq` upward.
///
/// A zip with no segment and no range in its record reads "no call log" and is
/// not failed. **Not yet Core's:** the record's `observed.call_log.cites`, an
/// array of `{seq, hash}`, is how a signed record will cite an entry; no record
/// cites one yet, so the key is this check's reading of what the Test phase
/// will write.
fn check_call_log(c: &mut Check, files: &BTreeMap<String, Vec<u8>>, record: Option<&Value>) {
    let range = record.map(|r| &r["predicate"]["observed"]["call_log"]).filter(|r| !r.is_null());
    let Some(bytes) = files.get(CALL_SEGMENT_NAME) else {
        if range.is_some() {
            c.fail(format!("the signed record names a call-log range, and {CALL_SEGMENT_NAME} is not in the zip"));
        } else {
            c.compared.push("the call log: no call log (this zip has none)".into());
        }
        return;
    };
    let Some(range) = range else {
        c.fail(format!("{CALL_SEGMENT_NAME} is in the zip and the signed record names no call-log range: it is another run's"));
        return;
    };
    let Some(text) = std::str::from_utf8(bytes).ok() else {
        c.fail(format!("{CALL_SEGMENT_NAME} is not text"));
        return;
    };
    let parsed: Result<Vec<Value>, _> = text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str::<Value>).collect();
    let Ok(lines) = parsed else {
        c.fail(format!("a {CALL_SEGMENT_NAME} line is not JSON"));
        return;
    };
    let Some((header, entries)) = lines.split_first().map(|(h, e)| (&h["call_log_segment"], e)).filter(|(h, _)| !h.is_null()) else {
        c.fail(format!("{CALL_SEGMENT_NAME} has no call_log_segment header line"));
        return;
    };
    for k in ["prev", "head", "first_seq", "last_seq"] {
        if header[k] != range[k] {
            c.fail(format!("the segment's {k} is not the signed record's call_log.{k}: the segment is another run's, or was cut"));
            return;
        }
    }
    let (Some(mut prev), Some(head)) = (range["prev"].as_str().map(str::to_string), range["head"].as_str()) else {
        c.fail("the signed record's call_log has no prev or head".into());
        return;
    };
    let first = range["first_seq"].as_u64();
    let want = match (first, range["last_seq"].as_u64()) {
        (Some(f), Some(l)) if l >= f => (l - f + 1) as usize,
        (None, None) => 0,
        _ => {
            c.fail("the signed record's call_log first_seq and last_seq are not a range".into());
            return;
        }
    };
    if entries.len() != want {
        c.fail(format!("the segment holds {} entries, the signed range names {want}: an entry was dropped or added", entries.len()));
        return;
    }
    for (i, l) in entries.iter().enumerate() {
        let (entry, hash) = (&l["entry"], l["hash"].as_str().unwrap_or(""));
        let seq = first.map(|f| f + i as u64);
        if entry.is_null() || sha(&serde_json::to_vec(entry).unwrap_or_default()) != hash {
            c.fail(format!("call-log entry {}: it does not match its hash: it was altered", i + 1));
            return;
        }
        if entry["prev"].as_str() != Some(prev.as_str()) {
            c.fail(format!("call-log entry {}: its prev is not the previous entry's hash: an entry was removed, added or reordered", i + 1));
            return;
        }
        if entry["seq"].as_u64() != seq {
            c.fail(format!("call-log entry {}: its seq is not the {} the signed range puts here", i + 1, seq.unwrap_or(0)));
            return;
        }
        prev = hash.to_string();
    }
    if prev != head {
        c.fail("the call-log segment does not end at the head the signed record names".into());
        return;
    }
    if let (Some(l), Some(len)) = (range["last_seq"].as_u64(), range["length"].as_u64()) {
        if len != l + 1 {
            c.fail(format!("the signed record's call_log length {len} is not last_seq {l} + 1"));
            return;
        }
    }
    c.compared.push(format!(
        "the call log: {} entries, seq {}, every hash, prev and seq recomputes from the signed prev to the signed head",
        entries.len(),
        match (first, range["last_seq"].as_u64()) {
            (Some(f), Some(l)) => format!("{f} to {l}"),
            _ => "none (an empty range)".into(),
        }
    ));
    let cites = range["cites"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    for cite in cites {
        let found = entries.iter().find(|l| l["entry"]["seq"] == cite["seq"]);
        if found.is_none_or(|l| l["hash"] != cite["hash"]) {
            c.fail(format!("the signed record cites call-log entry {} and the segment does not hold it with that hash", cite["seq"]));
            return;
        }
    }
    c.compared.push(format!("{} call-log entries the signed record cites are in the segment, by seq and hash", cites.len()));
}

/// Reads `path` and verifies it; the CLI's entry.
pub fn run(path: &Path, opts: &Options) -> Result<Report> {
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    verify_zip(path, opts)
}
