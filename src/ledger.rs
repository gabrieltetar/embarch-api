//! The hardware test ledger (`embarch-api` decision 98): what ran, on what, and
//! what waits for which hardware. The MCP tools `ledger_add`, `ledger_show`,
//! `ledger_pending` and the `ledger-*` subcommands are two front-ends over the
//! functions here.
//!
//! The files live in `<project source_path>/embarch/ledger/`, untracked like
//! the rest of `embarch/`:
//!
//! - `tests.toml` — **test definitions**, hand-written: `[[test]]` with `id`,
//!   `title`, `pass` (the criterion, prose), `image` (what must be flashed),
//!   `needs` (the roles and instruments it requires, by name) and `notes`.
//! - `rows.jsonl` — **ledger rows**, append-only, one JSON object per line.
//! - `bench.json` — the **bench-state header**: `{ "attached": [..],
//!   "broken": [{ "what": "partner", "note": ".." }] }`. `attached` is only the
//!   fallback for a Core that cannot be reached; `broken` names hardware that
//!   is present but unusable, and counts as missing.
//!
//! The pending view is computed on every call and never stored.

use anyhow::{bail, Context, Result};
use embarch_core_client::{CoreClient, EnrolledBoard, Instrument};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use crate::lease::Report;

/// Why a test is listed as pending.
pub const NEVER_RUN: &str = "never-run";
pub const COMMIT_UNREACHABLE: &str = "commit-unreachable";
pub const IMAGE_CHANGED: &str = "image-changed";

/// The result a row carries between a study's submission and its end.
pub const SUBMITTED: &str = "submitted";
/// What `ledger_add` accepts; the rest are written by the system.
pub const RESULTS: &[&str] = &["pass", "fail", "skipped"];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TestDef {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
struct TestsFile {
    #[serde(default)]
    test: Vec<TestDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Row {
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub when: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    /// SHA-256 of the image that ran, hex.
    #[serde(default)]
    pub image_hash: Option<String>,
    /// What was attached when it ran.
    #[serde(default)]
    pub roles: Vec<String>,
    /// `None` for a study run that named no test.
    #[serde(default)]
    pub test: Option<String>,
    pub result: String,
    #[serde(default)]
    pub study_id: Option<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Broken {
    pub what: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Bench {
    #[serde(default)]
    pub attached: Vec<String>,
    #[serde(default)]
    pub broken: Vec<Broken>,
}

pub fn dir(source_path: &Path) -> PathBuf {
    source_path.join("embarch").join("ledger")
}

pub fn load_tests(dir: &Path) -> Result<Vec<TestDef>> {
    let path = dir.join("tests.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: TestsFile = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let mut seen = BTreeSet::new();
    for t in &file.test {
        if !seen.insert(t.id.as_str()) {
            bail!("{}: test id `{}` is defined twice", path.display(), t.id);
        }
    }
    Ok(file.test)
}

pub fn load_rows(dir: &Path) -> Result<Vec<Row>> {
    let path = dir.join("rows.jsonl");
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

pub fn append_row(dir: &Path, row: &Row) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("rows.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(row)?).with_context(|| format!("writing {}", path.display()))
}

pub fn load_bench(dir: &Path) -> Result<Bench> {
    let path = dir.join("bench.json");
    if !path.exists() {
        return Ok(Bench::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn save_bench(dir: &Path, bench: &Bench) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("bench.json");
    std::fs::write(&path, serde_json::to_string_pretty(bench)? + "\n").with_context(|| format!("writing {}", path.display()))
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

fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    utc_stamp(secs)
}

// ---------------------------------------------------------------- git, image

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

// ---------------------------------------------------------------- attached

/// What is attached, from Core's topology: each enrolled board by role and by
/// board name, and each declared instrument by name. Lowercased.
pub fn attached_from(boards: &[EnrolledBoard], instruments: &[Instrument]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for b in boards {
        for s in [b.role.as_str(), b.name.as_str()] {
            if !s.trim().is_empty() {
                set.insert(s.trim().to_ascii_lowercase());
            }
        }
    }
    for i in instruments {
        set.insert(i.name.trim().to_ascii_lowercase());
    }
    set
}

/// Where the attached set came from, so a view never presents a fallback as
/// a measurement.
async fn attached_now(core: &CoreClient, bench: &Bench) -> (BTreeSet<String>, &'static str, Option<String>) {
    let boards = core.list_enrolled().await;
    let instruments = core.instruments().await;
    match (boards, instruments) {
        (Ok(b), Ok(i)) => (attached_from(&b, &i), "core", None),
        (b, i) => {
            let why = b.err().or(i.err()).map(|e| format!("{e:#}")).unwrap_or_default();
            (bench.attached.iter().map(|s| s.to_ascii_lowercase()).collect(), "bench.json", Some(why))
        }
    }
}

// ---------------------------------------------------------------- pending

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub title: String,
    /// `never-run`, `commit-unreachable`, `image-changed`, or the latest
    /// row's result when that was not a pass (`fail`, `skipped`, `submitted`,
    /// `interrupted`).
    pub reason: String,
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Pending {
    pub runnable_now: Vec<Item>,
    /// Role → the tests that cannot run for want of it. A test missing two
    /// roles is listed under both; `missing` on the item says so.
    pub by_missing_role: BTreeMap<String, Vec<Item>>,
}

/// The computed view. A test is done when its latest row is a pass whose
/// commit is still reachable from its branch and whose image hash, when both
/// it and `current_image` are known, still matches. Anything else is listed.
pub fn pending(
    tests: &[TestDef],
    rows: &[Row],
    attached: &BTreeSet<String>,
    broken: &[Broken],
    current_image: Option<&str>,
    reach: &dyn Reach,
) -> Pending {
    let broken: BTreeSet<String> = broken.iter().map(|b| b.what.to_ascii_lowercase()).collect();
    let mut out = Pending { runnable_now: Vec::new(), by_missing_role: BTreeMap::new() };
    for t in tests {
        let latest = rows.iter().rev().find(|r| r.test.as_deref() == Some(t.id.as_str()));
        let reason = match latest {
            None => Some(NEVER_RUN.to_string()),
            Some(r) if r.result != "pass" => Some(r.result.clone()),
            Some(r) => {
                let gone = match (&r.commit, &r.branch) {
                    (Some(c), Some(b)) => !reach.reachable(c, b),
                    _ => false,
                };
                let changed = matches!((&r.image_hash, current_image), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b));
                if gone {
                    Some(COMMIT_UNREACHABLE.to_string())
                } else if changed {
                    Some(IMAGE_CHANGED.to_string())
                } else {
                    None
                }
            }
        };
        let Some(reason) = reason else { continue };
        let missing: Vec<String> = t
            .needs
            .iter()
            .map(|n| n.to_ascii_lowercase())
            .filter(|n| !attached.contains(n) || broken.contains(n))
            .collect();
        let item = Item { id: t.id.clone(), title: t.title.clone(), reason, missing: missing.clone() };
        if missing.is_empty() {
            out.runnable_now.push(item);
        } else {
            for m in &missing {
                out.by_missing_role.entry(m.clone()).or_default().push(item.clone());
            }
        }
    }
    out
}

pub fn render_pending(p: &Pending, attached_source: &str) -> String {
    let line = |i: &Item| {
        let also = if i.missing.len() > 1 { format!(" (needs {})", i.missing.join(", ")) } else { String::new() };
        format!("  {} — {}{}{}", i.id, i.reason, if i.title.is_empty() { String::new() } else { format!(": {}", i.title) }, also)
    };
    let mut s = format!("attached according to {attached_source}\nrunnable now ({}):\n", p.runnable_now.len());
    for i in &p.runnable_now {
        s += &line(i);
        s.push('\n');
    }
    for (role, items) in &p.by_missing_role {
        s += &format!("waiting for {role} ({}):\n", items.len());
        for i in items {
            s += &line(i);
            s.push('\n');
        }
    }
    s.trim_end().to_string()
}

// ---------------------------------------------------------------- settle

/// What a finished study means for a test: a pass only when Core says
/// `completed` and every step's own outcome was a pass (observed, not
/// inferred); `failed` or any other step outcome is a fail; `interrupted`
/// stays itself. `None` while the study is still going.
pub fn verdict(status: &str, result: Option<&embarch_study_designer::StudyResult>) -> Option<&'static str> {
    use embarch_study_designer::Outcome;
    match status {
        "completed" => Some(match result {
            Some(r) if r.steps.iter().all(|s| matches!(s.outcome, Outcome::Pass)) => "pass",
            _ => "fail",
        }),
        "failed" => Some("fail"),
        "interrupted" => Some("interrupted"),
        _ => None,
    }
}

/// Submitted rows with no later row for the same study.
pub fn unsettled(rows: &[Row]) -> Vec<&Row> {
    rows.iter()
        .filter(|r| r.result == SUBMITTED)
        .filter(|r| {
            let id = r.study_id.as_deref();
            id.is_some() && !rows.iter().any(|o| o.study_id.as_deref() == id && o.result != SUBMITTED)
        })
        .collect()
}

/// Close every submitted row whose study has ended, by appending a row; a
/// study Core no longer knows, or is still running, is left alone. Returns
/// how many rows were appended.
pub async fn settle(dir: &Path, core: &CoreClient) -> Result<usize> {
    let rows = load_rows(dir)?;
    let mut n = 0;
    for r in unsettled(&rows) {
        let id = r.study_id.as_deref().unwrap_or_default();
        let Ok(st) = core.get_study_status(id).await else { continue };
        let Some(v) = verdict(&st.status, st.result.as_ref()) else { continue };
        let mut done = r.clone();
        done.when = now();
        done.result = v.to_string();
        done.notes = format!(
            "{} — settled from study status `{}`{}",
            r.notes,
            st.status,
            st.reason.map(|x| format!(": {x}")).unwrap_or_default()
        );
        append_row(dir, &done)?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------- front-ends

fn failed(what: &str, e: anyhow::Error) -> Report {
    let message = format!("{what} failed: {e:#}");
    Report { success: false, value: serde_json::json!({ "success": false, "error": message }), human: message }
}

pub struct AddArgs<'a> {
    pub test: &'a str,
    pub result: &'a str,
    pub notes: &'a str,
    pub image_path: Option<&'a str>,
    pub image_hash: Option<&'a str>,
    pub study_id: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// Overrides the roles read from Core.
    pub roles: Option<Vec<String>>,
    /// Replaces the bench header's known-broken list; empty clears it.
    pub broken: Option<Vec<String>>,
}

/// Append one row. Branch and commit default to the tree as it stands, the
/// roles to what Core reports attached.
pub async fn add(source_path: &Path, core: &CoreClient, a: AddArgs<'_>) -> Report {
    let go = async {
        let d = dir(source_path);
        if !RESULTS.contains(&a.result) {
            bail!("result `{}` is not one of {}", a.result, RESULTS.join(", "));
        }
        let tests = load_tests(&d)?;
        if !tests.is_empty() && !tests.iter().any(|t| t.id == a.test) {
            let ids: Vec<&str> = tests.iter().map(|t| t.id.as_str()).collect();
            bail!("no test `{}` in tests.toml (known: {})", a.test, ids.join(", "));
        }
        let mut bench = load_bench(&d)?;
        if let Some(b) = &a.broken {
            bench.broken = b.iter().map(|w| Broken { what: w.clone(), note: String::new() }).collect();
            save_bench(&d, &bench)?;
        }
        let roles: Vec<String> = match a.roles.clone() {
            Some(r) => r,
            None => attached_now(core, &bench).await.0.into_iter().collect(),
        };
        let (b, c) = head(source_path);
        let row = Row {
            when: now(),
            branch: a.branch.map(str::to_string).or(b),
            commit: a.commit.map(str::to_string).or(c),
            image_hash: image_hash(a.image_hash, a.image_path)?,
            roles,
            test: Some(a.test.to_string()),
            result: a.result.to_string(),
            study_id: a.study_id.map(str::to_string),
            notes: a.notes.to_string(),
        };
        append_row(&d, &row)?;
        Ok(row)
    };
    match go.await {
        Ok(row) => {
            let human = format!("ledger: {} {} at {}", row.test.as_deref().unwrap_or("?"), row.result, row.commit.as_deref().unwrap_or("no commit"));
            Report { success: true, value: serde_json::json!({ "success": true, "row": row }), human }
        }
        Err(e) => failed("ledger_add", e),
    }
}

/// The row `run_study` appends when it submits: result `submitted`, closed by
/// [`settle`] when the study ends. Never an error a study call must report —
/// the caller gets a one-line outcome to attach.
pub async fn record_submission(
    source_path: &Path,
    core: &CoreClient,
    test: Option<&str>,
    study_id: &str,
    study_name: &str,
    image_path: Option<&str>,
) -> serde_json::Value {
    let go = async {
        let d = dir(source_path);
        let bench = load_bench(&d)?;
        let (branch, commit) = head(source_path);
        let row = Row {
            when: now(),
            branch,
            commit,
            image_hash: image_hash(None, image_path)?,
            roles: attached_now(core, &bench).await.0.into_iter().collect(),
            test: test.map(str::to_string),
            result: SUBMITTED.to_string(),
            study_id: Some(study_id.to_string()),
            notes: format!("study `{study_name}`"),
        };
        append_row(&d, &row)?;
        Ok::<_, anyhow::Error>(d)
    };
    match go.await {
        Ok(d) => serde_json::json!({ "appended": "submitted", "ledger": d.display().to_string() }),
        Err(e) => serde_json::json!({ "skipped": format!("{e:#}") }),
    }
}

/// The bench header, the recent rows (newest last), and the settle count.
pub async fn show(source_path: &Path, core: &CoreClient, test: Option<&str>, limit: usize) -> Report {
    let go = async {
        let d = dir(source_path);
        let settled = settle(&d, core).await.unwrap_or(0);
        let bench = load_bench(&d)?;
        let (attached, source, why) = attached_now(core, &bench).await;
        let rows = load_rows(&d)?;
        let mut rows: Vec<Row> = rows.into_iter().filter(|r| test.is_none() || r.test.as_deref() == test).collect();
        let skip = rows.len().saturating_sub(limit);
        rows.drain(..skip);
        Ok::<_, anyhow::Error>((bench, attached, source, why, rows, settled))
    };
    match go.await {
        Ok((bench, attached, source, why, rows, settled)) => {
            let mut human = format!(
                "bench: attached according to {source}: {}\nknown broken: {}\n",
                attached.iter().cloned().collect::<Vec<_>>().join(", "),
                if bench.broken.is_empty() {
                    "none".to_string()
                } else {
                    bench.broken.iter().map(|b| format!("{} {}", b.what, b.note).trim().to_string()).collect::<Vec<_>>().join("; ")
                }
            );
            for r in &rows {
                human += &format!(
                    "{} {} {} {} @{} {}\n",
                    r.when,
                    r.test.as_deref().unwrap_or("-"),
                    r.result,
                    r.branch.as_deref().unwrap_or("-"),
                    r.commit.as_deref().map(|c| &c[..c.len().min(10)]).unwrap_or("-"),
                    r.notes
                );
            }
            Report {
                success: true,
                value: serde_json::json!({
                    "success": true,
                    "bench": { "attached": attached, "attached_source": source, "core_error": why, "broken": bench.broken },
                    "rows": rows,
                    "settled": settled,
                }),
                human: human.trim_end().to_string(),
            }
        }
        Err(e) => failed("ledger_show", e),
    }
}

pub async fn pending_view(
    source_path: &Path,
    core: &CoreClient,
    attached: Option<Vec<String>>,
    image_path: Option<&str>,
    image_hash_arg: Option<&str>,
) -> Report {
    let go = async {
        let d = dir(source_path);
        let settled = settle(&d, core).await.unwrap_or(0);
        let bench = load_bench(&d)?;
        let (set, source, why) = match attached {
            Some(a) => (a.iter().map(|s| s.to_ascii_lowercase()).collect(), "the attached argument", None),
            None => attached_now(core, &bench).await,
        };
        let current = image_hash(image_hash_arg, image_path)?;
        let p = pending(&load_tests(&d)?, &load_rows(&d)?, &set, &bench.broken, current.as_deref(), &GitReach(source_path));
        Ok::<_, anyhow::Error>((p, source, why, settled))
    };
    match go.await {
        Ok((p, source, why, settled)) => {
            let human = render_pending(&p, source);
            Report {
                success: true,
                value: serde_json::json!({
                    "success": true,
                    "attached_source": source,
                    "core_error": why,
                    "runnable_now": p.runnable_now,
                    "by_missing_role": p.by_missing_role,
                    "settled": settled,
                }),
                human,
            }
        }
        Err(e) => failed("ledger_pending", e),
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

    fn def(id: &str, needs: &[&str]) -> TestDef {
        TestDef { id: id.into(), title: String::new(), pass: String::new(), image: String::new(), needs: needs.iter().map(|s| s.to_string()).collect(), notes: String::new() }
    }

    fn row(test: &str, result: &str, commit: &str, hash: Option<&str>) -> Row {
        Row {
            when: "2026-10-28T00:00:00Z".into(),
            branch: Some("main".into()),
            commit: Some(commit.into()),
            image_hash: hash.map(str::to_string),
            roles: vec![],
            test: Some(test.into()),
            result: result.into(),
            study_id: None,
            notes: String::new(),
        }
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn groups_by_missing_role_and_attaching_moves_tests_to_runnable() {
        let tests = [def("a", &["partner"]), def("b", &["partner", "eload"]), def("c", &[])];
        let fake = Fake(vec![]);
        let p = pending(&tests, &[], &set(&["dut"]), &[], None, &fake);
        assert_eq!(p.runnable_now.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!(p.by_missing_role["partner"].len(), 2);
        assert_eq!(p.by_missing_role["eload"][0].id, "b");

        let p = pending(&tests, &[], &set(&["dut", "partner"]), &[], None, &fake);
        assert_eq!(p.runnable_now.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert!(!p.by_missing_role.contains_key("partner"));
        assert_eq!(p.by_missing_role["eload"][0].missing, ["eload"]);
    }

    #[test]
    fn a_passing_row_with_a_live_commit_is_done_and_a_failed_one_is_not() {
        let tests = [def("a", &[]), def("b", &[])];
        let rows = [row("a", "pass", "c1", None), row("b", "fail", "c1", None)];
        let p = pending(&tests, &rows, &set(&[]), &[], None, &Fake(vec![("c1", "main")]));
        assert_eq!(p.runnable_now.len(), 1);
        assert_eq!((p.runnable_now[0].id.as_str(), p.runnable_now[0].reason.as_str()), ("b", "fail"));
    }

    #[test]
    fn the_latest_row_wins() {
        let tests = [def("a", &[])];
        let rows = [row("a", "fail", "c1", None), row("a", "pass", "c1", None)];
        let p = pending(&tests, &rows, &set(&[]), &[], None, &Fake(vec![("c1", "main")]));
        assert!(p.runnable_now.is_empty());
    }

    #[test]
    fn an_unreachable_commit_or_a_changed_image_flags_a_rerun() {
        let tests = [def("a", &[]), def("b", &[])];
        let rows = [row("a", "pass", "gone", Some("aa")), row("b", "pass", "c1", Some("aa"))];
        let fake = Fake(vec![("c1", "main")]);
        let p = pending(&tests, &rows, &set(&[]), &[], Some("bb"), &fake);
        let r: Vec<_> = p.runnable_now.iter().map(|i| (i.id.as_str(), i.reason.as_str())).collect();
        assert_eq!(r, [("a", COMMIT_UNREACHABLE), ("b", IMAGE_CHANGED)]);
        // An unknown current image never flags a rerun by itself.
        let p = pending(&tests, &rows, &set(&[]), &[], None, &fake);
        assert_eq!(p.runnable_now.len(), 1);
    }

    #[test]
    fn broken_hardware_counts_as_missing() {
        let tests = [def("a", &["partner"])];
        let broken = [Broken { what: "Partner".into(), note: "port dead".into() }];
        let p = pending(&tests, &[], &set(&["partner"]), &broken, None, &Fake(vec![]));
        assert!(p.runnable_now.is_empty());
        assert_eq!(p.by_missing_role["partner"][0].id, "a");
    }

    #[test]
    fn unsettled_means_submitted_with_no_later_row() {
        let mut a = row("a", SUBMITTED, "c", None);
        a.study_id = Some("s1".into());
        let mut b = row("a", SUBMITTED, "c", None);
        b.study_id = Some("s2".into());
        let mut done = row("a", "pass", "c", None);
        done.study_id = Some("s2".into());
        let rows = [a, b, done];
        let u = unsettled(&rows);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].study_id.as_deref(), Some("s1"));
    }

    #[test]
    fn verdicts() {
        assert_eq!(verdict("running", None), None);
        assert_eq!(verdict("failed", None), Some("fail"));
        assert_eq!(verdict("interrupted", None), Some("interrupted"));
        assert_eq!(verdict("completed", None), Some("fail"));
    }

    #[test]
    fn stamp_is_civil_utc() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(1_793_145_600 + 3661), "2026-10-28T01:01:01Z");
    }

    #[test]
    fn files_round_trip() {
        let d = std::env::temp_dir().join(format!("ledger-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("tests.toml"), "[[test]]\nid = \"pd-5v\"\nneeds = [\"partner\"]\n").unwrap();
        assert_eq!(load_tests(&d).unwrap()[0].needs, ["partner"]);
        std::fs::write(d.join("tests.toml"), "[[test]]\nid = \"x\"\n[[test]]\nid = \"x\"\n").unwrap();
        assert!(load_tests(&d).is_err());
        append_row(&d, &row("a", "pass", "c1", Some("aa"))).unwrap();
        append_row(&d, &row("b", "fail", "c2", None)).unwrap();
        let rows = load_rows(&d).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].test.as_deref(), Some("b"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn git_reachability_is_read_from_a_real_repo() {
        let d = std::env::temp_dir().join(format!("ledger-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g = |args: &[&str]| {
            let o = Command::new("git").arg("-C").arg(&d).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let (branch, first) = head(&d);
        let first = first.unwrap();
        assert_eq!(branch.as_deref(), Some("main"));
        g(&["commit", "-q", "--allow-empty", "-m", "two"]);
        assert!(GitReach(&d).reachable(&first, "main"));
        g(&["reset", "-q", "--hard", &format!("{first}~0")]);
        g(&["commit", "-q", "--allow-empty", "-m", "three"]);
        let (_, third) = head(&d);
        g(&["reset", "-q", "--hard", &first]);
        g(&["commit", "-q", "--allow-empty", "-m", "four"]);
        assert!(!GitReach(&d).reachable(&third.unwrap(), "main"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
