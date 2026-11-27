//! The test ledger (`embarch-api` decision 142): Core's run log is the record, and
//! `embarch/tests/results.jsonl` is its mirror in the firmware repo. The MCP tools
//! `test_show`, `test_pending`, `test_add` and the `test-show`, `test-pending`,
//! `test-add` subcommands are two front-ends over the functions here.
//!
//! - The record is `GET /run-log`: a Test run's `test` entries and core decision 147's
//!   `manual_verdict` entries. A `relayed` manual verdict (the agent's token) is listed
//!   and never counts, not even as a failure.
//! - The mirror is rewritten from the log on every call; where a row differs from the
//!   log, the log wins. Only the repo-side `branch` and `commit` are kept from the old row.
//! - `embarch/ledger/` (decision 98) is read as legacy and never written.
//! - The pending view is computed on every call and never stored.

use anyhow::{Context, Result};
use embarch_study_designer::hil_test::TestDef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use crate::lease::Report;

/// Why a test is listed as pending.
pub const NEVER_RUN: &str = "never-run";
pub const COMMIT_UNREACHABLE: &str = "commit-unreachable";
pub const IMAGE_CHANGED: &str = "image-changed";
/// A `test` entry for a test the run never reached has a null verdict.
pub const NOT_REACHED: &str = "not-reached";
/// The bench a test known only from the log is grouped under.
pub const UNKNOWN_BENCH: &str = "(unknown bench)";
/// What `test_add` accepts; Core refuses anything else.
pub const VERDICTS: &[&str] = &["pass", "fail", "inconclusive"];
const KIND_TEST: &str = "test";
const KIND_MANUAL: &str = "manual_verdict";
const SHA256_HEX_LEN: usize = 64;

/// The legacy ledger's folder (decision 98), read and never written.
pub fn legacy_dir(source_path: &Path) -> PathBuf {
    source_path.join("embarch").join("ledger")
}

fn tests_dir(source_path: &Path) -> PathBuf {
    source_path.join("embarch").join("tests")
}

fn results_path(source_path: &Path) -> PathBuf {
    tests_dir(source_path).join("results.jsonl")
}

/// A decision-98 row, `embarch/ledger/rows.jsonl`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Row {
    pub when: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub image_hash: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub test: Option<String>,
    pub result: String,
    #[serde(default)]
    pub study_id: Option<String>,
    #[serde(default)]
    pub notes: String,
}

/// The legacy rows, oldest first; none when the folder or file is absent.
pub fn load_legacy_rows(source_path: &Path) -> Result<Vec<Row>> {
    let path = legacy_dir(source_path).join("rows.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    raw.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{} line {}", path.display(), i + 1)))
        .collect()
}

// ---------------------------------------------------------------- the clock

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time (Howard Hinnant's civil-from-days).
pub fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------- the log

/// One `test` or `manual_verdict` entry of Core's run log, with the hash that
/// names it. Other entry types are not the ledger's.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub hash: String,
    pub seq: u64,
    pub kind: String,
    pub test_id: String,
    /// Null in the log for a test the run never reached.
    pub verdict: Option<String>,
    pub run_id: Option<String>,
    pub unit_key: Option<String>,
    pub definition_hash: Option<String>,
    pub dut_image: Value,
    pub relayed: bool,
    pub reason: Option<String>,
    pub at_utc_ms: Option<u64>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The ledger's entries of a `GET /run-log` body, in chain order.
pub fn entries_of(log: &Value) -> Vec<Entry> {
    let lines = log.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let e = line.get("entry")?;
            let kind = e.get("type")?.as_str()?;
            if kind != KIND_TEST && kind != KIND_MANUAL {
                return None;
            }
            Some(Entry {
                hash: text(line, "hash").unwrap_or_default(),
                seq: e.get("seq").and_then(Value::as_u64).unwrap_or(i as u64),
                kind: kind.to_string(),
                test_id: text(e, "test_id")?,
                verdict: text(e, "verdict"),
                run_id: text(e, "run_id"),
                unit_key: text(e, "unit_key"),
                definition_hash: text(e, "definition_hash"),
                dut_image: e.get("dut_image").cloned().unwrap_or(Value::Null),
                relayed: e.get("relayed").and_then(Value::as_bool).unwrap_or(false),
                reason: text(e, "reason"),
                at_utc_ms: e.get("at_utc_ms").and_then(Value::as_u64),
            })
        })
        .collect()
}

impl Entry {
    /// The image hash the run named, when it named one as a string `claim`.
    fn claim(&self) -> Option<&str> {
        self.dut_image.get("claim").and_then(Value::as_str).or_else(|| self.dut_image.as_str())
    }

    /// Whether the entry can clear a test: any entry but a relayed manual verdict.
    fn counts(&self) -> bool {
        !self.relayed
    }

    fn is_pass(&self) -> bool {
        self.verdict.as_deref() == Some("pass")
    }
}

// ---------------------------------------------------------------- the mirror

/// A row of `embarch/tests/results.jsonl`: the entry's fields as the log has them, plus
/// the repo-side `branch` and `commit` the tree stood at when the row was first written
/// (the log has no commit, so the rebased-away rule reads it here).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Mirror {
    pub hash: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub test_id: String,
    pub verdict: Option<String>,
    pub run_id: Option<String>,
    pub unit_key: Option<String>,
    pub definition_hash: Option<String>,
    pub dut_image: Value,
    pub relayed: bool,
    pub reason: Option<String>,
    pub at_utc_ms: Option<u64>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

impl Mirror {
    fn from_entry(e: &Entry, branch: Option<String>, commit: Option<String>) -> Mirror {
        Mirror {
            hash: e.hash.clone(),
            seq: e.seq,
            kind: e.kind.clone(),
            test_id: e.test_id.clone(),
            verdict: e.verdict.clone(),
            run_id: e.run_id.clone(),
            unit_key: e.unit_key.clone(),
            definition_hash: e.definition_hash.clone(),
            dut_image: e.dut_image.clone(),
            relayed: e.relayed,
            reason: e.reason.clone(),
            at_utc_ms: e.at_utc_ms,
            branch,
            commit,
        }
    }

    /// The row with its repo-side fields blanked: what the log decides.
    fn log_part(&self) -> Mirror {
        Mirror { branch: None, commit: None, ..self.clone() }
    }
}

/// What a sync did to the mirror.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct Synced {
    pub added: usize,
    /// Rows that differed from the log's entry and were rewritten to it.
    pub rewritten: usize,
    /// Rows whose entry is not in the log and were dropped.
    pub removed: usize,
}

/// The mirror's rows from `existing`, lenient: a line that does not parse is dropped
/// (and so counts as removed), since the log is what the file is made from.
fn parse_mirror(raw: &str) -> Vec<Mirror> {
    raw.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// The new mirror text and what changed, from the entries and the file's old text.
pub fn merge_mirror(entries: &[Entry], old: &str, tree: &(Option<String>, Option<String>)) -> (String, Synced) {
    let old_rows = parse_mirror(old);
    let by_hash: BTreeMap<&str, &Mirror> = old_rows.iter().map(|m| (m.hash.as_str(), m)).collect();
    let mut synced = Synced::default();
    let mut out = String::new();
    for e in entries {
        let fresh = Mirror::from_entry(e, None, None);
        let row = match by_hash.get(e.hash.as_str()) {
            Some(old) => {
                if old.log_part() != fresh {
                    synced.rewritten += 1;
                }
                Mirror { branch: old.branch.clone(), commit: old.commit.clone(), ..fresh }
            }
            None => {
                synced.added += 1;
                Mirror { branch: tree.0.clone(), commit: tree.1.clone(), ..fresh }
            }
        };
        out.push_str(&serde_json::to_string(&row).unwrap_or_default());
        out.push('\n');
    }
    let kept: BTreeSet<&str> = entries.iter().map(|e| e.hash.as_str()).collect();
    synced.removed = old.lines().filter(|l| !l.trim().is_empty()).count() - old_rows.iter().filter(|m| kept.contains(m.hash.as_str())).count();
    (out, synced)
}

/// Rewrite `embarch/tests/results.jsonl` to the log's entries; a file already right is not
/// touched. Refuses a link in its place.
pub fn sync_mirror(source_path: &Path, entries: &[Entry]) -> Result<(Synced, Vec<Mirror>)> {
    let path = results_path(source_path);
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!("{} is a link; the mirror is not written through links", path.display());
        }
    }
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let (text, synced) = merge_mirror(entries, &old, &head(source_path));
    if text != old {
        let dir = tests_dir(source_path);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!(".results.jsonl.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    }
    Ok((synced, parse_mirror(&text)))
}

// ---------------------------------------------------------------- tests known

/// A test the repo's custom suites define: its bench is where pending groups it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub id: String,
    pub title: String,
    pub bench: String,
    pub definition_hash: String,
}

/// Every `embarch/tests/suites/<suite>/<test id>/test.json` that parses as a test.
pub fn known_tests(source_path: &Path) -> Vec<Known> {
    let mut out = Vec::new();
    let Ok(suites) = std::fs::read_dir(tests_dir(source_path).join("suites")) else { return out };
    let mut suite_dirs: Vec<PathBuf> = suites.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    suite_dirs.sort();
    for s in suite_dirs {
        let Ok(tests) = std::fs::read_dir(&s) else { continue };
        let mut test_dirs: Vec<PathBuf> = tests.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        test_dirs.sort();
        for t in test_dirs {
            let Ok(raw) = std::fs::read(t.join("test.json")) else { continue };
            let Ok(def) = serde_json::from_slice::<TestDef>(&raw) else { continue };
            out.push(Known { id: def.test_id.clone(), title: def.title.clone(), bench: def.bench.name.clone(), definition_hash: def.hash() });
        }
    }
    out
}

// ---------------------------------------------------------------- pending

/// A relayed manual verdict, listed beside the test and never counted.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Relayed {
    pub verdict: String,
    pub reason: String,
    pub at_utc_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub title: String,
    /// `never-run`, `commit-unreachable`, `image-changed`, or the latest counted
    /// verdict when it was not a pass (`fail`, `inconclusive`, ...; `not-reached`).
    pub reason: String,
    pub relayed: Vec<Relayed>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Pending {
    /// Bench name → the tests that must run on it.
    pub by_bench: BTreeMap<String, Vec<Item>>,
}

/// Whether a commit is still reachable from a branch. A trait so the pending
/// logic is testable without a repository.
pub trait Reach {
    fn reachable(&self, commit: &str, branch: &str) -> bool;
}

/// Read-only `git` against a project's tree (spec invariant: this crate never
/// runs a tree-mutating subcommand).
pub struct GitReach<'a>(pub &'a Path);

impl Reach for GitReach<'_> {
    fn reachable(&self, commit: &str, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(self.0)
            .args(["merge-base", "--is-ancestor", commit, branch])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// The computed view. A test is done when its latest counted entry on the current
/// image is a pass whose commit (from the mirror) is still reachable from its branch.
/// With `current_image` known, only entries whose image `claim` matches it, or that
/// name none, are runs "on the current DUT image". A relayed manual verdict is never a
/// pass and never a failure: it is listed beside the test.
pub fn pending(known: &[Known], entries: &[Entry], mirror: &[Mirror], current_image: Option<&str>, reach: &dyn Reach) -> Pending {
    let by_hash: BTreeMap<&str, &Mirror> = mirror.iter().map(|m| (m.hash.as_str(), m)).collect();
    let mut tests: BTreeMap<String, (String, String)> = BTreeMap::new();
    for k in known {
        tests.insert(k.id.clone(), (k.title.clone(), k.bench.clone()));
    }
    for e in entries {
        tests.entry(e.test_id.clone()).or_insert_with(|| (String::new(), UNKNOWN_BENCH.to_string()));
    }
    let mut out = Pending { by_bench: BTreeMap::new() };
    for (id, (title, bench)) in tests {
        let mine: Vec<&Entry> = entries.iter().filter(|e| e.test_id == id).collect();
        let relayed: Vec<Relayed> = mine
            .iter()
            .filter(|e| !e.counts())
            .map(|e| Relayed { verdict: e.verdict.clone().unwrap_or_default(), reason: e.reason.clone().unwrap_or_default(), at_utc_ms: e.at_utc_ms })
            .collect();
        let counted: Vec<&&Entry> = mine.iter().filter(|e| e.counts()).collect();
        let on_image: Vec<&&&Entry> = counted
            .iter()
            .filter(|e| match (e.claim(), current_image) {
                (Some(c), Some(cur)) => c.eq_ignore_ascii_case(cur),
                _ => true,
            })
            .collect();
        let reason = match (on_image.last(), counted.is_empty()) {
            (None, true) => Some(NEVER_RUN.to_string()),
            (None, false) => Some(IMAGE_CHANGED.to_string()),
            (Some(e), _) if !e.is_pass() => Some(e.verdict.clone().unwrap_or_else(|| NOT_REACHED.to_string())),
            (Some(e), _) => {
                let gone = by_hash
                    .get(e.hash.as_str())
                    .and_then(|m| m.commit.as_deref().zip(m.branch.as_deref()))
                    .is_some_and(|(c, b)| !reach.reachable(c, b));
                gone.then(|| COMMIT_UNREACHABLE.to_string())
            }
        };
        let Some(reason) = reason else { continue };
        out.by_bench.entry(bench).or_default().push(Item { id, title, reason, relayed });
    }
    out
}

pub fn render_pending(p: &Pending) -> String {
    let mut s = String::new();
    for (bench, items) in &p.by_bench {
        s += &format!("{bench} ({}):\n", items.len());
        for i in items {
            let relayed = if i.relayed.is_empty() { String::new() } else { format!(" [{} relayed, not counted]", i.relayed.len()) };
            s += &format!("  {} — {}{}{}\n", i.id, i.reason, if i.title.is_empty() { String::new() } else { format!(": {}", i.title) }, relayed);
        }
    }
    if s.is_empty() {
        s = "nothing pending".to_string();
    }
    s.trim_end().to_string()
}

// ---------------------------------------------------------------- git, image

fn git_out(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `(branch, commit)` of the tree as it stands; either is `None` outside a repo.
pub fn head(repo: &Path) -> (Option<String>, Option<String>) {
    (git_out(repo, &["rev-parse", "--abbrev-ref", "HEAD"]), git_out(repo, &["rev-parse", "HEAD"]))
}

pub fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).with_context(|| format!("reading image {}", path.display()))?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// An image hash from whichever of a hash or a path was given; the hash wins.
pub fn image_hash(hash: Option<&str>, path: Option<&str>) -> Result<Option<String>> {
    match (hash, path) {
        (Some(h), _) if !h.trim().is_empty() => Ok(Some(h.trim().to_ascii_lowercase())),
        (_, Some(p)) if !p.trim().is_empty() => hash_file(Path::new(p)).map(Some),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------- front-ends

fn failed(what: &str, e: anyhow::Error) -> Report {
    let message = format!("{what} failed: {e:#}");
    Report { success: false, value: json!({ "success": false, "error": message }), human: message }
}

/// Read Core's log and bring the mirror to it.
async fn read_and_sync(source_path: &Path, core: &embarch_core_client::CoreClient) -> Result<(Value, Vec<Entry>, Synced, Vec<Mirror>)> {
    let log = core.run_log().await.context("reading Core's run log")?;
    let entries = entries_of(&log);
    let (synced, mirror) = sync_mirror(source_path, &entries)?;
    Ok((log, entries, synced, mirror))
}

fn log_state(log: &Value) -> Value {
    json!({ "verified": log.get("verified"), "error": log.get("error"), "length": log.get("length"), "head": log.get("head") })
}

/// The log's entries (newest last), the mirror's changes, and the legacy rows apart.
pub async fn show(source_path: &Path, core: &embarch_core_client::CoreClient, test: Option<&str>, limit: usize) -> Report {
    let go = async {
        let (log, entries, synced, _) = read_and_sync(source_path, core).await?;
        let legacy = load_legacy_rows(source_path)?;
        let mut shown: Vec<&Entry> = entries.iter().filter(|e| test.is_none() || Some(e.test_id.as_str()) == test).collect();
        let skip = shown.len().saturating_sub(limit);
        shown.drain(..skip);
        let legacy: Vec<&Row> = legacy.iter().filter(|r| test.is_none() || r.test.as_deref() == test).collect();
        let mut human = String::new();
        for e in &shown {
            human += &format!(
                "{} {} {} {}{}\n",
                e.at_utc_ms.map(|m| crate::test_ledger::utc_stamp(m / 1000)).unwrap_or_else(|| "-".into()),
                e.test_id,
                e.verdict.as_deref().unwrap_or(NOT_REACHED),
                e.kind,
                if e.relayed { " (relayed by the agent, not counted)" } else { "" }
            );
        }
        if !legacy.is_empty() {
            human += "before Test, not in Core's log:\n";
            for r in &legacy {
                human += &format!("  {} {} {} {}\n", r.when, r.test.as_deref().unwrap_or("-"), r.result, r.notes);
            }
        }
        let value = json!({
            "success": true,
            "log": log_state(&log),
            "entries": shown.iter().map(|e| json!({
                "hash": e.hash, "seq": e.seq, "type": e.kind, "test_id": e.test_id, "verdict": e.verdict,
                "run_id": e.run_id, "unit_key": e.unit_key, "definition_hash": e.definition_hash,
                "dut_image": e.dut_image, "relayed": e.relayed, "reason": e.reason, "at_utc_ms": e.at_utc_ms,
            })).collect::<Vec<_>>(),
            "mirror": synced,
            "legacy": { "note": "before Test, not in Core's log", "rows": legacy },
        });
        Ok::<_, anyhow::Error>((value, human.trim_end().to_string()))
    };
    match go.await {
        Ok((value, human)) => Report { success: true, value, human },
        Err(e) => failed("test_show", e),
    }
}

/// The pending view from Core's log, grouped by the bench each test needs.
pub async fn pending_view(
    source_path: &Path,
    core: &embarch_core_client::CoreClient,
    image_path: Option<&str>,
    image_hash_arg: Option<&str>,
) -> Report {
    let go = async {
        let (log, entries, synced, mirror) = read_and_sync(source_path, core).await?;
        let current = image_hash(image_hash_arg, image_path)?;
        let p = pending(&known_tests(source_path), &entries, &mirror, current.as_deref(), &GitReach(source_path));
        Ok::<_, anyhow::Error>((log, synced, p))
    };
    match go.await {
        Ok((log, synced, p)) => Report {
            success: true,
            human: render_pending(&p),
            value: json!({ "success": true, "log": log_state(&log), "mirror": synced, "by_bench": p.by_bench }),
        },
        Err(e) => failed("test_pending", e),
    }
}

pub struct AddArgs<'a> {
    pub test: &'a str,
    pub verdict: &'a str,
    pub reason: &'a str,
    pub unit_key: &'a str,
    /// Left out, it is the hash of the test in the repo's suites.
    pub definition_hash: Option<&'a str>,
    pub dut_image: Option<&'a str>,
    pub note: Option<&'a str>,
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == SHA256_HEX_LEN && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hash a manual verdict binds: the one given, else the repo's test of that id.
fn definition_hash_for(source_path: &Path, test: &str, given: Option<&str>) -> Result<String> {
    if let Some(h) = given.filter(|h| !h.trim().is_empty()) {
        return Ok(h.trim().to_string());
    }
    let mut found: Vec<Known> = known_tests(source_path).into_iter().filter(|k| k.id == test).collect();
    match found.len() {
        1 => Ok(found.remove(0).definition_hash),
        0 => anyhow::bail!("no definition_hash given and no test `{test}` under embarch/tests/suites/"),
        _ => anyhow::bail!("test `{test}` is in more than one suite; give its definition_hash"),
    }
}

/// A manual verdict through `POST /manual-verdicts`; the mirror follows it.
pub async fn add(source_path: &Path, core: &embarch_core_client::CoreClient, a: AddArgs<'_>) -> Report {
    let go = async {
        if !VERDICTS.contains(&a.verdict) {
            anyhow::bail!("verdict `{}` is not one of {}", a.verdict, VERDICTS.join(", "));
        }
        let hash = definition_hash_for(source_path, a.test, a.definition_hash)?;
        if !is_sha256_hex(&hash) {
            anyhow::bail!("definition_hash must be 64 lowercase hex characters");
        }
        let mut body = json!({
            "test_id": a.test, "definition_hash": hash, "unit_key": a.unit_key,
            "verdict": a.verdict, "reason": a.reason,
        });
        if let Some(i) = a.dut_image.filter(|i| !i.is_empty()) {
            body["dut_image"] = json!(i);
        }
        if let Some(n) = a.note.filter(|n| !n.is_empty()) {
            body["note"] = json!(n);
        }
        let answer = core.manual_verdict(&body).await.context("POST /manual-verdicts")?;
        let relayed = answer.pointer("/entry/relayed").and_then(Value::as_bool).unwrap_or(false);
        // The log is the record already; a mirror that cannot follow is said, not an error.
        let mirror = match read_and_sync(source_path, core).await {
            Ok((_, _, synced, _)) => json!(synced),
            Err(e) => json!({ "skipped": format!("{e:#}") }),
        };
        Ok::<_, anyhow::Error>((answer, relayed, mirror))
    };
    match go.await {
        Ok((answer, relayed, mirror)) => {
            let who = if relayed { "relayed by the agent" } else { "entered with the person's token" };
            let human = format!("manual verdict {} for {}: {who}", a.verdict, a.test);
            Report { success: true, value: json!({ "success": true, "relayed": relayed, "attribution": who, "entry": answer, "mirror": mirror }), human }
        }
        Err(e) => failed("test_add", e),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<(&'static str, &'static str)>);
    impl Reach for Fake {
        fn reachable(&self, c: &str, b: &str) -> bool {
            self.0.iter().any(|(x, y)| *x == c && *y == b)
        }
    }

    fn entry(hash: &str, test: &str, kind: &str, verdict: Option<&str>, relayed: bool) -> Entry {
        Entry {
            hash: hash.into(),
            seq: 0,
            kind: kind.into(),
            test_id: test.into(),
            verdict: verdict.map(str::to_string),
            run_id: None,
            unit_key: None,
            definition_hash: None,
            dut_image: Value::Null,
            relayed,
            reason: None,
            at_utc_ms: None,
        }
    }

    fn known(id: &str, bench: &str) -> Known {
        Known { id: id.into(), title: String::new(), bench: bench.into(), definition_hash: "0".repeat(64) }
    }

    fn mirror_of(entries: &[Entry], commit: Option<&str>) -> Vec<Mirror> {
        entries.iter().map(|e| Mirror::from_entry(e, commit.map(|_| "main".into()), commit.map(str::to_string))).collect()
    }

    fn ids(p: &Pending, bench: &str) -> Vec<(String, String)> {
        p.by_bench.get(bench).map(|v| v.iter().map(|i| (i.id.clone(), i.reason.clone())).collect()).unwrap_or_default()
    }

    #[test]
    fn a_failed_latest_run_is_pending_and_a_passed_one_is_not() {
        let k = [known("a", "b1"), known("b", "b1"), known("c", "b2")];
        let e = [entry("h1", "a", "test", Some("pass"), false), entry("h2", "b", "test", Some("fail"), false)];
        let p = pending(&k, &e, &mirror_of(&e, None), None, &Fake(vec![]));
        assert_eq!(ids(&p, "b1"), [("b".to_string(), "fail".to_string())]);
        assert_eq!(ids(&p, "b2"), [("c".to_string(), NEVER_RUN.to_string())]);
    }

    #[test]
    fn a_relayed_manual_pass_does_not_clear_a_test_and_is_listed() {
        let k = [known("a", "b1")];
        let e = [entry("h1", "a", "test", Some("fail"), false), entry("h2", "a", "manual_verdict", Some("pass"), true)];
        let p = pending(&k, &e, &mirror_of(&e, None), None, &Fake(vec![]));
        assert_eq!(ids(&p, "b1"), [("a".to_string(), "fail".to_string())]);
        assert_eq!(p.by_bench["b1"][0].relayed.len(), 1);
        // A relayed pass alone: never run.
        let e = [entry("h2", "a", "manual_verdict", Some("pass"), true)];
        let p = pending(&k, &e, &mirror_of(&e, None), None, &Fake(vec![]));
        assert_eq!(ids(&p, "b1"), [("a".to_string(), NEVER_RUN.to_string())]);
        // A person's pass does clear it.
        let e = [entry("h1", "a", "test", Some("fail"), false), entry("h3", "a", "manual_verdict", Some("pass"), false)];
        assert!(pending(&k, &e, &mirror_of(&e, None), None, &Fake(vec![])).by_bench.is_empty());
    }

    #[test]
    fn a_null_verdict_is_not_reached_and_a_log_only_test_has_an_unknown_bench() {
        let e = [entry("h1", "x", "test", None, false)];
        let p = pending(&[], &e, &mirror_of(&e, None), None, &Fake(vec![]));
        assert_eq!(ids(&p, UNKNOWN_BENCH), [("x".to_string(), NOT_REACHED.to_string())]);
    }

    #[test]
    fn a_rebased_away_commit_or_another_image_makes_a_pass_pending() {
        let k = [known("a", "b")];
        let mut e = entry("h1", "a", "test", Some("pass"), false);
        e.dut_image = json!({ "claim": "aa" });
        let es = [e];
        let m = mirror_of(&es, Some("gone"));
        let p = pending(&k, &es, &m, None, &Fake(vec![("c1", "main")]));
        assert_eq!(ids(&p, "b"), [("a".to_string(), COMMIT_UNREACHABLE.to_string())]);
        let m = mirror_of(&es, Some("c1"));
        assert!(pending(&k, &es, &m, Some("AA"), &Fake(vec![("c1", "main")])).by_bench.is_empty());
        let p = pending(&k, &es, &m, Some("bb"), &Fake(vec![("c1", "main")]));
        assert_eq!(ids(&p, "b"), [("a".to_string(), IMAGE_CHANGED.to_string())]);
    }

    #[test]
    fn a_differing_mirror_row_is_rewritten_to_the_log_and_keeps_its_commit() {
        let e = [entry("h1", "a", "test", Some("fail"), false)];
        let mut stale = Mirror::from_entry(&e[0], Some("main".into()), Some("c1".into()));
        stale.verdict = Some("pass".into());
        let gone = Mirror::from_entry(&entry("h9", "z", "test", Some("pass"), false), None, None);
        let old = format!("{}\n{}\n", serde_json::to_string(&stale).unwrap(), serde_json::to_string(&gone).unwrap());
        let tree = (Some("main".to_string()), Some("c2".to_string()));
        let (text, synced) = merge_mirror(&e, &old, &tree);
        assert_eq!(synced, Synced { added: 0, rewritten: 1, removed: 1 });
        let rows = parse_mirror(&text);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].verdict.as_deref(), rows[0].commit.as_deref()), (Some("fail"), Some("c1")));
        // Already right: nothing changes.
        let (again, synced) = merge_mirror(&e, &text, &tree);
        assert_eq!((again, synced), (text, Synced::default()));
    }

    #[test]
    fn the_log_parses_to_the_two_entry_types_only() {
        let log = json!({ "entries": [
            { "entry": { "type": "run", "seq": 0 }, "hash": "r" },
            { "entry": { "type": "test", "seq": 1, "test_id": "a", "verdict": "pass", "dut_image": { "claim": "x" } }, "hash": "t" },
            { "entry": { "type": "manual_verdict", "seq": 2, "test_id": "a", "verdict": "pass", "relayed": true }, "hash": "m" },
        ]});
        let es = entries_of(&log);
        assert_eq!(es.iter().map(|e| (e.hash.as_str(), e.relayed)).collect::<Vec<_>>(), [("t", false), ("m", true)]);
        assert_eq!(es[0].claim(), Some("x"));
    }

    #[test]
    fn the_legacy_ledger_is_read_and_a_sync_never_touches_it() {
        let d = std::env::temp_dir().join(format!("tledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let legacy = d.join("embarch/ledger");
        std::fs::create_dir_all(&legacy).unwrap();
        let rows = "{\"when\":\"2026-10-28T00:00:00Z\",\"test\":\"pd\",\"result\":\"pass\"}\n";
        std::fs::write(legacy.join("rows.jsonl"), rows).unwrap();
        assert_eq!(load_legacy_rows(&d).unwrap()[0].test.as_deref(), Some("pd"));
        let e = [entry("h1", "a", "test", Some("pass"), false)];
        let (synced, mirror) = sync_mirror(&d, &e).unwrap();
        assert_eq!((synced.added, mirror.len()), (1, 1));
        assert_eq!(std::fs::read_to_string(legacy.join("rows.jsonl")).unwrap(), rows);
        assert!(!legacy.join("results.jsonl").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stamp_is_civil_utc() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(1_793_145_600 + 3661), "2026-10-28T01:01:01Z");
    }
}
