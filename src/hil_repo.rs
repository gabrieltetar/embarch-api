//! The agent's side of a HIL Test's definitions (`embarch-api` decision 134): the
//! `test_propose` and `test_plan` tools and the `embarch/tests/` layout they write.
//!
//! ```text
//! embarch/tests/suites/<suite>/suite.json          a custom suite's manifest
//! embarch/tests/suites/<suite>/<test id>/test.json  a test, and its study beside it
//! embarch/tests/plans/<plan>.json                  a saved selection
//! embarch/tests/results.jsonl                      reserved for the results mirror
//! ```
//!
//! A proposal is checked by `embarch-study-designer` first (shapes, and
//! `hil_check::check_test` where a study and bench travel with it), so a proposal the
//! crate refuses never reaches Core; then it is sent to `POST /definitions`, where the
//! engineer confirms it. Nothing here confirms anything: the confirmation routes refuse
//! the agent's token.
//!
//! Selecting a suite (a plan naming it is written) copies its benches into
//! `embarch/topologies/` and never overwrites a saved bench.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use embarch_core_client::CoreClient;
use embarch_study_designer::hil_check::{check_test, By, Measured, Metrology, StudyKind};
use embarch_study_designer::hil_test::{
    canonical_json, Capability, HookMapping, Plan, SuiteBench, SuiteManifest, SuiteTest, TestDef,
};
use embarch_study_designer::HostStudy;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub use crate::lease::Report;
use embarch_api::person_writer::{ConfinedWriter, Kind};

/// The version a custom suite's manifest starts at.
const NEW_SUITE_VERSION: &str = "0.1.0";
/// What a custom suite's `validated_with` says: no EmbArch release validated it.
const UNVALIDATED: &str = "unvalidated";

pub fn tests_dir(source_path: &Path) -> PathBuf {
    source_path.join("embarch").join("tests")
}

pub(crate) fn failed(message: impl Into<String>) -> Report {
    let message = message.into();
    Report { success: false, value: json!({ "success": false, "error": message }), human: message }
}

/// A name that is one safe path step: lowercase letter or digit first, then
/// `a-z 0-9 - _ .`, no `..`.
pub fn is_slug(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_lowercase() || f.is_ascii_digit())
        && c.all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '-' | '_' | '.'))
        && !s.contains("..")
        && s.len() <= 100
}

fn need_slug(what: &str, s: &str) -> Result<(), String> {
    if is_slug(s) {
        Ok(())
    } else {
        Err(format!("{what} `{s}` is not a name: lowercase letters, digits and `- _ .`, starting with a letter or digit"))
    }
}

/// A saved bench's file name from its name (`embarch-ui` decision 73): lowercase, every
/// other run of characters one hyphen.
pub fn bench_slug(name: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

fn parse<T: DeserializeOwned>(what: &str, v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("not a valid {what}: {e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn pretty(v: &impl serde::Serialize) -> Vec<u8> {
    let mut b = serde_json::to_vec_pretty(v).unwrap_or_default();
    b.push(b'\n');
    b
}

// ------------------------------------------------------------- confined writes

/// Writes `bytes` as one `kind` file through the person writer (decision 132): no link
/// followed, a hard-linked target refused, a temp file and a rename. `overwrite` false
/// refuses an existing file. Returns whether it replaced one.
///
/// A repo with no `embarch/` yet gets one, as `api/196`'s writer did; the person writer
/// opens only an existing folder, and it still refuses a link put in its place.
fn write_under_embarch(repo: &Path, kind: Kind, name: &str, bytes: &[u8], overwrite: bool) -> Result<bool, String> {
    match std::fs::create_dir(repo.join("embarch")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("creating {}/embarch: {e}", repo.display())),
    }
    let w = ConfinedWriter::open(repo).map_err(|e| format!("{e:#}"))?;
    w.write_with(kind, name, bytes, overwrite).map(|(_, replaced)| replaced).map_err(|e| format!("{e:#}"))
}

// ------------------------------------------------------------------ propose

pub struct ProposeArgs<'a> {
    /// `test`, `capability` or `hook`.
    pub kind: &'a str,
    pub definition: Value,
    /// A host study, with `bench`: the checks that need them run.
    pub study: Option<Value>,
    /// The suite bench the test runs on.
    pub bench: Option<Value>,
    /// `{<measurement>: {by, source, range_min, range_max, resolution, u}}`.
    pub measured: Option<Value>,
    /// A custom suite in the repo to write a test under.
    pub suite: Option<&'a str>,
}

fn parse_measured(v: &Value) -> Result<BTreeMap<String, Measured>, String> {
    let obj = v.as_object().ok_or("`measured` is not an object of measurements")?;
    let mut out = BTreeMap::new();
    for (name, m) in obj {
        let by = match m.get("by").and_then(Value::as_str) {
            Some("bench") => By::Bench,
            Some("dut") => By::Dut,
            Some("person") => By::Person,
            _ => return Err(format!("measured.{name}.by must be bench, dut or person")),
        };
        let num = |k: &str| m.get(k).and_then(Value::as_f64);
        out.insert(
            name.clone(),
            Measured {
                by,
                source: m.get("source").and_then(Value::as_str).unwrap_or_default().to_string(),
                metrology: Metrology { range_min: num("range_min"), range_max: num("range_max"), resolution: num("resolution"), u: num("u") },
            },
        );
    }
    Ok(out)
}

/// What a checked test carries on to Core and to the repo.
struct CheckedTest {
    def: TestDef,
    study_bytes: Option<Vec<u8>>,
    bench: Option<SuiteBench>,
}

/// The crate's checks for a test, before Core is called. A refusal is the crate's own
/// reason, one line per refusal.
fn check_proposed_test(a: &ProposeArgs<'_>) -> Result<CheckedTest, String> {
    let mut definition = a.definition.clone();
    if a.study.is_some() {
        // The study's hash is the hash of the bytes this tool writes; leave it out and it is filled.
        if let Some(study) = definition.get_mut("study").and_then(Value::as_object_mut) {
            study.entry("hash").or_insert_with(|| Value::String(String::new()));
        }
    }
    let mut def: TestDef = parse("test", &definition)?;
    def.check().map_err(|e| format!("{}: {e:?}", def.test_id))?;
    need_slug("test id", &def.test_id)?;
    match (&a.study, &a.bench) {
        (None, None) => Ok(CheckedTest { def, study_bytes: None, bench: None }),
        (Some(_), None) | (None, Some(_)) => Err("`study` and `bench` go together: the definition-time checks need both".into()),
        (Some(study_v), Some(bench_v)) => {
            let study: HostStudy = parse("host study", study_v)?;
            let bench: SuiteBench = parse("suite bench", bench_v)?;
            if !bench.description.is_object() {
                return Err(format!("bench `{}`: description is not an object", bench.name));
            }
            let bytes = pretty(&study);
            let hash = sha256_hex(&bytes);
            if def.study.hash.is_empty() {
                def.study.hash = hash;
            } else if def.study.hash != hash {
                return Err(format!(
                    "{}: study.hash is {}, the study as written hashes to {hash}; leave `hash` out of study to have it filled",
                    def.test_id, def.study.hash
                ));
            }
            let measured = match &a.measured {
                Some(m) => parse_measured(m)?,
                None => BTreeMap::new(),
            };
            check_test(&def, StudyKind::Host(&study), &bench, &measured).map_err(|refusals| {
                refusals.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
            })?;
            Ok(CheckedTest { def, study_bytes: Some(bytes), bench: Some(bench) })
        }
    }
}

pub async fn propose(source_path: &Path, core: &CoreClient, a: ProposeArgs<'_>) -> Report {
    if let Some(suite) = a.suite {
        if let Err(e) = need_slug("suite", suite) {
            return failed(e);
        }
        if a.kind != "test" {
            return failed("`suite` places a test in a custom suite; a capability or a hook is not a test");
        }
    }
    let mut body = json!({ "kind": a.kind });
    let mut checked: Option<CheckedTest> = None;
    match a.kind {
        "test" => match check_proposed_test(&a) {
            Ok(c) => {
                body["definition"] = serde_json::to_value(&c.def).unwrap_or(Value::Null);
                if c.study_bytes.is_some() {
                    for k in ["study", "bench", "measured"] {
                        let v = match k {
                            "study" => a.study.clone(),
                            "bench" => a.bench.clone(),
                            _ => a.measured.clone(),
                        };
                        if let Some(v) = v {
                            body[k] = v;
                        }
                    }
                }
                checked = Some(c);
            }
            Err(reason) => return refused(reason),
        },
        "capability" => match parse::<Capability>("capability", &a.definition) {
            Ok(c) => body["definition"] = serde_json::to_value(c).unwrap_or(Value::Null),
            Err(e) => return refused(e),
        },
        "hook" => match parse::<HookMapping>("hook mapping", &a.definition) {
            Ok(h) => body["definition"] = serde_json::to_value(h).unwrap_or(Value::Null),
            Err(e) => return refused(e),
        },
        other => return refused(format!("`kind` is `{other}`: a proposal is a test, a capability or a hook")),
    }

    let reply = match core.post_definition(&body).await {
        Ok(r) => r,
        Err(e) => return failed(format!("test_propose: Core did not keep it: {e:#}")),
    };
    let hash = reply["hash"].as_str().unwrap_or_default().to_string();
    let confirmation = reply["confirmation"].as_str().unwrap_or_default().to_string();
    let waits = confirmation == "unconfirmed";

    let mut written = Vec::new();
    let mut write_note = None;
    if let (Some(suite), Some(c)) = (a.suite, &checked) {
        match write_test(source_path, suite, c) {
            Ok((paths, note)) => {
                written = paths;
                write_note = note;
            }
            Err(e) => write_note = Some(format!("Core kept it, but the repo write failed: {e}")),
        }
    }

    let what = match a.kind {
        "test" => format!("test {}", checked.as_ref().map_or("", |c| c.def.test_id.as_str())),
        k => k.to_string(),
    };
    let says = if waits {
        "it waits for the engineer to confirm it, on the card in embarch-ui or in the terminal; you cannot confirm it".to_string()
    } else {
        format!("Core's standing for it: {confirmation}")
    };
    let mut human = format!("proposed {what}: hash {hash}, checked: {}. {says}.", reply["checked"].as_str().unwrap_or("?"));
    for p in &written {
        human.push_str(&format!("\nwrote {p}"));
    }
    if let Some(n) = &write_note {
        human.push_str(&format!("\n{n}"));
    }
    Report {
        success: write_note.as_ref().is_none_or(|n| !n.starts_with("Core kept it")),
        value: json!({
            "success": true,
            "kind": a.kind,
            "hash": hash,
            "kept": reply["kept"],
            "checked": reply["checked"],
            "confirmation": confirmation,
            "waits_for_confirmation": waits,
            "message": says,
            "written": written,
            "note": write_note,
        }),
        human,
    }
}

fn refused(reason: String) -> Report {
    let human = format!("test_propose: refused before Core was called, by embarch-study-designer:\n{reason}");
    Report { success: false, value: json!({ "success": false, "refused_before_core": true, "reason": reason, "error": human }), human }
}

/// Writes a checked test under `embarch/tests/suites/<suite>/`, and its manifest line
/// when a bench travelled with it. Returns the paths written and a note.
fn write_test(repo: &Path, suite: &str, c: &CheckedTest) -> Result<(Vec<String>, Option<String>), String> {
    let id = c.def.test_id.as_str();
    let mut written = Vec::new();
    let suite_dir = tests_dir(repo).join("suites").join(suite);
    // The manifest is read first: one that does not parse is never clobbered.
    let manifest_path = suite_dir.join("suite.json");
    let mut manifest: Option<SuiteManifest> = match std::fs::read(&manifest_path) {
        Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| format!("{} does not parse, left alone: {e}", manifest_path.display()))?),
        Err(_) => None,
    };
    let test_name = format!("{id}/test.json");
    write_under_embarch(repo, Kind::Test, &format!("{suite}/{id}"), &pretty(&c.def), true)?;
    written.push(format!("embarch/tests/suites/{suite}/{test_name}"));
    if let Some(bytes) = &c.study_bytes {
        let file = c.def.study.file.as_str();
        if file.contains('/') || file.contains('\\') || file.starts_with('.') || file.is_empty() {
            return Err(format!("study.file `{file}` is not a bare file name"));
        }
        write_under_embarch(repo, Kind::TestStudy, &format!("{suite}/{id}/{file}"), bytes, true)?;
        written.push(format!("embarch/tests/suites/{suite}/{id}/{file}"));
    }
    let Some(bench) = &c.bench else {
        let note = manifest.is_none().then(|| "suite.json not written: no bench travelled with the test, and a manifest lists its benches".to_string());
        if let Some(m) = manifest.as_mut() {
            upsert_test(m, id, &test_name, &c.def);
            write_under_embarch(repo, Kind::Suite, suite, &pretty(m), true)?;
            written.push(format!("embarch/tests/suites/{suite}/suite.json"));
        }
        return Ok((written, note));
    };
    let m = manifest.get_or_insert_with(|| SuiteManifest {
        schema: 1,
        id: suite.to_string(),
        version: NEW_SUITE_VERSION.to_string(),
        validated_with: UNVALIDATED.to_string(),
        tests: Vec::new(),
        benches: Vec::new(),
    });
    upsert_test(m, id, &test_name, &c.def);
    match m.benches.iter_mut().find(|b| b.name == bench.name) {
        Some(slot) => *slot = bench.clone(),
        None => m.benches.push(bench.clone()),
    }
    write_under_embarch(repo, Kind::Suite, suite, &pretty(m), true)?;
    written.push(format!("embarch/tests/suites/{suite}/suite.json"));
    Ok((written, None))
}

fn upsert_test(m: &mut SuiteManifest, id: &str, file: &str, def: &TestDef) {
    let line = SuiteTest { test_id: id.to_string(), file: file.to_string(), hash: def.hash() };
    match m.tests.iter_mut().find(|t| t.test_id == id) {
        Some(slot) => *slot = line,
        None => m.tests.push(line),
    }
}

// --------------------------------------------------------------------- plan

pub struct PlanArgs<'a> {
    /// The plan's file name (without `.json`).
    pub plan: Option<&'a str>,
    /// A suite to list the applicable tests of.
    pub suite: Option<&'a str>,
    /// A plan to write: with `plan`, creates or updates it.
    pub definition: Option<Value>,
    /// The DUT's capabilities, to list against, when no plan is named.
    pub capabilities: Option<Value>,
}

pub async fn plan(source_path: &Path, core: &CoreClient, a: PlanArgs<'_>) -> Report {
    if let Some(p) = a.plan {
        if let Err(e) = need_slug("plan", p) {
            return failed(e);
        }
    }
    if let Some(s) = a.suite {
        if let Err(e) = need_slug("suite", s) {
            return failed(e);
        }
    }
    let plan_file = a.plan.map(|p| tests_dir(source_path).join("plans").join(format!("{p}.json")));
    match (&a.definition, a.plan, a.suite) {
        (Some(def), Some(name), _) => write_plan(source_path, core, name, def).await,
        (Some(_), None, _) => failed("a plan to write needs `plan`, its file name"),
        (None, Some(name), None) => {
            let path = plan_file.unwrap_or_default();
            match read_plan(&path) {
                Ok(p) => {
                    let human = format!("plan {name}: suite {} {}, {} capabilities, {} hooks", p.suite.id, p.suite.version, p.dut.capabilities.len(), p.dut.hooks.len());
                    Report { success: true, value: json!({ "success": true, "plan": name, "definition": p }), human }
                }
                Err(e) => failed(e),
            }
        }
        (None, plan_name, Some(suite)) => {
            let caps = match (&a.capabilities, plan_name) {
                (Some(c), _) => parse::<Vec<Capability>>("capability list", c),
                (None, Some(_)) => read_plan(&plan_file.unwrap_or_default()).map(|p| p.dut.capabilities),
                (None, None) => Ok(Vec::new()),
            };
            match caps {
                Ok(caps) => list_applicable(source_path, core, suite, &caps).await,
                Err(e) => failed(e),
            }
        }
        (None, None, None) => failed("test_plan: name a suite to list its applicable tests, or a plan (with a definition to write it)"),
    }
}

pub(crate) fn read_plan(path: &Path) -> Result<Plan, String> {
    let b = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_slice(&b).map_err(|e| format!("{} is not a plan: {e}", path.display()))
}

async fn write_plan(repo: &Path, core: &CoreClient, name: &str, def: &Value) -> Report {
    let plan: Plan = match parse("plan", def) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if let Err(e) = plan.check() {
        return failed(format!("plan {name}: {e:?}"));
    }
    if let Err(e) = need_slug("suite id", &plan.suite.id) {
        return failed(e);
    }
    let file = format!("{name}.json");
    let replaced = match write_under_embarch(repo, Kind::Plan, name, &pretty(&plan), true) {
        Ok(r) => r,
        Err(e) => return failed(e),
    };
    let selection = select_suite(repo, core, &plan.suite.id).await;
    let mut human = format!(
        "{} embarch/tests/plans/{file} (suite {} {}, hash {})",
        if replaced { "updated" } else { "wrote" },
        plan.suite.id,
        plan.suite.version,
        embarch_study_designer::hil_test::definition_hash(&plan)
    );
    human.push('\n');
    human.push_str(&selection.1);
    Report {
        success: true,
        value: json!({ "success": true, "plan": name, "path": format!("embarch/tests/plans/{file}"), "replaced": replaced, "selection": selection.0 }),
        human,
    }
}

// ---------------------------------------------------------------- selection

/// Where a suite's manifest was read.
pub(crate) async fn load_manifest(repo: &Path, core: &CoreClient, id: &str) -> Result<(SuiteManifest, &'static str), String> {
    if is_slug(id) {
        let path = tests_dir(repo).join("suites").join(id).join("suite.json");
        if let Ok(b) = std::fs::read(&path) {
            let m = serde_json::from_slice(&b).map_err(|e| format!("{} does not parse: {e}", path.display()))?;
            return Ok((m, "repo"));
        }
    }
    let suites = core.suites().await.map_err(|e| format!("not in the repo, and Core's suites could not be read: {e:#}"))?;
    let entry = suites["suites"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["id"] == id))
        .ok_or_else(|| format!("no suite `{id}` in embarch/tests/suites/ or among Core's premade suites"))?;
    let hash = entry["manifest_sha256"].as_str().unwrap_or_default();
    let kept = core.definition(hash).await.map_err(|e| format!("Core lists suite `{id}` but does not serve its manifest ({hash}): {e:#}"))?;
    let m = serde_json::from_value(kept["definition"].clone()).map_err(|e| format!("Core's manifest for `{id}` does not parse: {e}"))?;
    Ok((m, "core"))
}

/// Selecting a suite: copy each of its benches into `embarch/topologies/<slug>.toml`,
/// leaving a name already taken with other content alone and reporting the diff.
pub async fn select_suite(repo: &Path, core: &CoreClient, id: &str) -> (Value, String) {
    let (manifest, source) = match load_manifest(repo, core, id).await {
        Ok(x) => x,
        Err(why) => {
            let human = format!("selecting suite {id}: its benches were not copied: {why}");
            return (json!({ "suite": id, "source": null, "copied": [], "unchanged": [], "left_alone": [], "note": why }), human);
        }
    };
    let (mut copied, mut unchanged, mut left) = (Vec::new(), Vec::new(), Vec::new());
    let mut notes = Vec::new();
    for bench in &manifest.benches {
        let slug = bench_slug(&bench.name);
        if slug.is_empty() || !is_slug(&slug) {
            notes.push(format!("bench `{}` has no usable file name", bench.name));
            continue;
        }
        let file = format!("{slug}.toml");
        let wanted = match description_toml(&bench.description) {
            Ok(v) => v,
            Err(e) => {
                notes.push(format!("bench `{}` not copied: {e}", bench.name));
                continue;
            }
        };
        let text = toml::to_string_pretty(&wanted).unwrap_or_default();
        let path = repo.join("embarch").join("topologies").join(&file);
        let rel = format!("embarch/topologies/{file}");
        match std::fs::read_to_string(&path) {
            Ok(have) => {
                let same = toml::from_str::<toml::Value>(&have).is_ok_and(|v| v == wanted);
                if same {
                    unchanged.push(json!({ "bench": bench.name, "path": rel }));
                } else {
                    left.push(json!({ "bench": bench.name, "path": rel, "diff": line_diff(&have, &text) }));
                }
            }
            Err(_) => match write_under_embarch(repo, Kind::SavedBench, &slug, text.as_bytes(), false) {
                Ok(_) => copied.push(json!({ "bench": bench.name, "path": rel })),
                Err(e) => notes.push(format!("bench `{}` not copied: {e}", bench.name)),
            },
        }
    }
    let mut human = format!(
        "selecting suite {id} ({source}): {} bench copied, {} already the same, {} left alone",
        copied.len(),
        unchanged.len(),
        left.len()
    );
    for l in &left {
        human.push_str(&format!(
            "\n  left alone {}: the saved bench differs from the suite's (- yours, + the suite's):\n{}",
            l["path"].as_str().unwrap_or(""),
            l["diff"].as_array().map(|d| d.iter().filter_map(Value::as_str).map(|x| format!("    {x}")).collect::<Vec<_>>().join("\n")).unwrap_or_default()
        ));
    }
    for n in &notes {
        human.push_str(&format!("\n  {n}"));
    }
    (json!({ "suite": id, "source": source, "copied": copied, "unchanged": unchanged, "left_alone": left, "notes": notes }), human)
}

/// A saved-bench description (JSON, `null` meaning absent) as a TOML value.
fn description_toml(d: &Value) -> Result<toml::Value, String> {
    fn strip(v: &Value) -> Option<Value> {
        match v {
            Value::Null => None,
            Value::Object(m) => Some(Value::Object(m.iter().filter_map(|(k, x)| strip(x).map(|x| (k.clone(), x))).collect())),
            Value::Array(a) => Some(Value::Array(a.iter().filter_map(strip).collect())),
            other => Some(other.clone()),
        }
    }
    let clean = strip(d).unwrap_or(Value::Null);
    toml::Value::try_from(&clean).map_err(|e| format!("its description is not TOML-shaped: {e}"))
}

/// Lines of `mine` that `theirs` lacks as `- ..`, then lines `theirs` has that `mine` lacks as `+ ..`.
fn line_diff(mine: &str, theirs: &str) -> Vec<String> {
    let a: BTreeSet<&str> = mine.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let b: BTreeSet<&str> = theirs.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut out: Vec<String> = mine.lines().map(str::trim).filter(|l| !l.is_empty() && !b.contains(l)).map(|l| format!("- {l}")).collect();
    out.extend(theirs.lines().map(str::trim).filter(|l| !l.is_empty() && !a.contains(l)).map(|l| format!("+ {l}")));
    out
}

// ------------------------------------------------------------------ listing

fn unmet(c: &embarch_study_designer::hil_test::Condition, caps: &[Capability]) -> Option<String> {
    use embarch_study_designer::hil_test::Condition::*;
    let find = |n: &str| caps.iter().find(|c| c.name == n);
    match c {
        Present { capability } => find(capability).is_none().then(|| format!("capability {capability} is not present")),
        Equals { capability, value } => {
            (!find(capability).is_some_and(|c| c.value == *value)).then(|| format!("capability {capability} must equal {}", canonical_json(value)))
        }
        Contains { capability, value } => (!find(capability).and_then(|c| c.value.as_array()).is_some_and(|a| a.contains(value)))
            .then(|| format!("capability {capability} must contain {}", canonical_json(value))),
    }
}

pub(crate) async fn suite_tests(repo: &Path, core: &CoreClient, suite: &str, manifest: &SuiteManifest, source: &str) -> Result<Vec<TestDef>, String> {
    let mut out = Vec::new();
    for t in &manifest.tests {
        if source == "repo" {
            let path = tests_dir(repo).join("suites").join(suite).join(&t.file);
            let b = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push(serde_json::from_slice(&b).map_err(|e| format!("{} is not a test: {e}", path.display()))?);
        } else {
            let suites = core.suites().await.map_err(|e| format!("{e:#}"))?;
            let sha = suites["suites"]
                .as_array()
                .and_then(|a| a.iter().find(|s| s["id"] == suite))
                .and_then(|s| s["tests"].as_array())
                .and_then(|ts| ts.iter().find(|x| x["test_id"] == t.test_id))
                .and_then(|x| x["sha256"].as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("test {} is not among the hashes Core lists", t.test_id))?;
            let kept = core.definition(&sha).await.map_err(|e| format!("{}: {e:#}", t.test_id))?;
            out.push(serde_json::from_value(kept["definition"].clone()).map_err(|e| format!("{}: {e}", t.test_id))?);
        }
    }
    Ok(out)
}

/// The tests of `suite` that apply to `caps`, grouped by bench.
async fn list_applicable(repo: &Path, core: &CoreClient, suite: &str, caps: &[Capability]) -> Report {
    let (manifest, source) = match load_manifest(repo, core, suite).await {
        Ok(x) => x,
        Err(e) => return failed(format!("test_plan: {e}")),
    };
    let tests = match suite_tests(repo, core, suite, &manifest, source).await {
        Ok(t) => t,
        Err(e) => return failed(format!("test_plan: reading suite {suite}: {e}")),
    };
    let mut by_bench: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut not_applicable = Vec::new();
    for t in &tests {
        let why = t
            .applies_when
            .iter()
            .find_map(|c| unmet(c, caps))
            .or_else(|| t.template.as_ref().filter(|tp| !caps.iter().any(|c| c.name == tp.capability)).map(|tp| format!("capability {} is not present", tp.capability)));
        match why {
            Some(reason) => not_applicable.push(json!({ "test_id": t.test_id, "bench": t.bench.name, "reason": reason })),
            None => by_bench.entry(t.bench.name.clone()).or_default().push(json!({
                "test_id": t.test_id, "title": t.title, "tags": t.tags, "requires": t.requires, "hooks": t.hooks,
            })),
        }
    }
    let mut human = format!(
        "suite {suite} {} ({source}): {} of {} tests apply to {} declared capabilities, by bench",
        manifest.version,
        by_bench.values().map(Vec::len).sum::<usize>(),
        tests.len(),
        caps.len()
    );
    for (bench, ts) in &by_bench {
        human.push_str(&format!("\n  {bench}:"));
        for t in ts {
            human.push_str(&format!("\n    {} - {}", t["test_id"].as_str().unwrap_or(""), t["title"].as_str().unwrap_or("")));
        }
    }
    for n in &not_applicable {
        human.push_str(&format!("\n  not applicable: {} ({})", n["test_id"].as_str().unwrap_or(""), n["reason"].as_str().unwrap_or("")));
    }
    Report {
        success: true,
        value: json!({
            "success": true, "suite": suite, "version": manifest.version, "source": source,
            "benches": manifest.benches.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
            "applicable_by_bench": by_bench, "not_applicable": not_applicable,
            "note": "a listing, not the expansion: Core re-expands a run from confirmed definitions and the DUT's inputs",
        }),
        human,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_refuse_traversal_and_uppercase() {
        assert!(is_slug("usb-pd-r3.2"));
        for bad in ["", "..", "a/b", "A", "-x", "a..b", ".hidden", "a b"] {
            assert!(!is_slug(bad), "{bad}");
        }
    }

    #[test]
    fn a_bench_slug_collapses_runs_like_the_saved_bench_does() {
        assert_eq!(bench_slug("PD sink  bench!"), "pd-sink-bench");
        assert_eq!(bench_slug("../../etc/passwd"), "etc-passwd");
    }

    #[test]
    fn a_description_with_nulls_becomes_toml_without_them() {
        let v = description_toml(&json!({ "slug": "b", "x": null, "parts": [{ "id": "u", "role": "dut", "n": null }] })).unwrap();
        assert!(v.get("x").is_none());
        assert_eq!(v["parts"][0]["role"].as_str(), Some("dut"));
    }

    #[test]
    fn the_diff_names_what_each_side_has_alone() {
        assert_eq!(line_diff("a = 1\nb = 2\n", "a = 1\nb = 3\n"), ["- b = 2", "+ b = 3"]);
    }

    #[test]
    fn the_writer_follows_no_link_and_keeps_an_existing_file_unless_told() {
        let dir = std::env::temp_dir().join(format!("hil-repo-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("embarch")).unwrap();
        assert!(!write_under_embarch(&dir, Kind::Plan, "a", b"1", true).unwrap());
        assert!(write_under_embarch(&dir, Kind::Plan, "a", b"2", false).is_err());
        assert!(write_under_embarch(&dir, Kind::Plan, "a", b"3", true).unwrap());
        assert_eq!(std::fs::read(dir.join("embarch/tests/plans/a.json")).unwrap(), b"3");
        assert!(write_under_embarch(&dir, Kind::Plan, "..", b"x", true).is_err());
        #[cfg(unix)]
        {
            let elsewhere = dir.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, dir.join("embarch/tests/suites")).unwrap();
            assert!(write_under_embarch(&dir, Kind::Test, "s/t", b"x", true).is_err());
            assert!(!elsewhere.join("s").exists());
            let victim = dir.join("victim");
            std::fs::write(&victim, b"keep").unwrap();
            std::fs::remove_file(dir.join("embarch/tests/plans/a.json")).unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("embarch/tests/plans/a.json")).unwrap();
            assert!(write_under_embarch(&dir, Kind::Plan, "a", b"x", true).is_err());
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
