//! `run_test` and `test_status` (`embarch-api` decision 135): the agent prepares a HIL
//! Test run and watches it.
//!
//! `run_test` resolves a selection through `embarch-study-designer`'s `expand`, sends
//! `run.json` with the definitions it came from to Core's `POST /test-runs`, and shows
//! Core's card. **It never starts the run and nothing here can**: the go takes the
//! person's token on every Core, and the client crate has no method for it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use embarch_core_client::CoreClient;
use embarch_study_designer::hil_expand::{expand, DutIdentity, ExpandContext, SelectionStatus, Standard, SuiteContent};
use embarch_study_designer::hil_test::{definition_hash, Capability, DutInputs, HookMapping, Plan, Selection, SuiteManifest, SuitePin, TestDef};
use embarch_study_designer::HostStudy;
use serde_json::{json, Value};

use crate::hil_repo::{bench_slug, failed, load_manifest, read_plan, suite_tests, tests_dir};
pub use crate::lease::Report;

/// What a built plan's suite pin says when the caller gives no commit.
const UNPINNED: &str = "unpinned";

pub struct RunTestArgs<'a> {
    /// A saved plan under `embarch/tests/plans/`.
    pub plan: Option<&'a str>,
    /// A suite, with `tests`, `tags` or `all`; with none of them the applicable tests are listed.
    pub suite: Option<&'a str>,
    pub tests: Vec<String>,
    pub tags: Vec<String>,
    pub all: bool,
    /// `[{name, value, provenance}]`; a plan brings its own.
    pub capabilities: Option<Value>,
    /// `[{hook, target}]`; a plan brings its own.
    pub hooks: Option<Value>,
    pub stop_on_fail: bool,
    /// The suite pin's commit for a built plan.
    pub commit: Option<&'a str>,
    pub standard_name: Option<&'a str>,
    pub standard_revision: Option<&'a str>,
    pub declared_serial: Option<&'a str>,
    pub board_revision: Option<&'a str>,
    pub hardware_id: Option<&'a str>,
}

fn parse<T: serde::de::DeserializeOwned>(what: &str, v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("not a valid {what}: {e}"))
}

/// A fresh id in Core's `[A-Za-z0-9_-]{1,128}`: the clock and the process, hashed.
fn fresh_run_id() -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let h = Sha256::digest(format!("{nanos}:{}", std::process::id()).as_bytes());
    let hex: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("run-{hex}")
}

/// A string Core wrote as `{"agent_text": s}`, or a plain one.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) if m.len() == 1 && m.contains_key("agent_text") => plain(&m["agent_text"]),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The studies of a suite by `study.file`: a repo suite's from beside each test, a premade
/// one's through Core by the study's hash. A study that cannot be found is left out, and
/// `expand` names it if a selected test needs it.
async fn load_studies(repo: &Path, core: &CoreClient, suite: &str, manifest: &SuiteManifest, tests: &[TestDef], source: &str) -> BTreeMap<String, HostStudy> {
    let mut out = BTreeMap::new();
    for d in tests {
        let parsed: Option<HostStudy> = if source == "repo" {
            let dir = manifest
                .tests
                .iter()
                .find(|t| t.test_id == d.test_id)
                .and_then(|t| Path::new(&t.file).parent().map(Path::to_path_buf))
                .unwrap_or_default();
            let path = tests_dir(repo).join("suites").join(suite).join(dir).join(&d.study.file);
            std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok())
        } else {
            match core.definition(&d.study.hash).await {
                Ok(k) => serde_json::from_value(k["definition"].clone()).ok(),
                Err(_) => None,
            }
        };
        if let Some(s) = parsed {
            out.insert(d.study.file.clone(), s);
        }
    }
    out
}

/// The suite's benches that exist: a saved bench under `embarch/topologies/` by the
/// bench's file name. Selecting a suite (`test_plan`) is what copies them there.
fn benches_present(repo: &Path, manifest: &SuiteManifest) -> Vec<String> {
    manifest
        .benches
        .iter()
        .filter(|b| repo.join("embarch").join("topologies").join(format!("{}.toml", bench_slug(&b.name))).is_file())
        .map(|b| b.name.clone())
        .collect()
}

fn need<'a>(what: &str, v: Option<&'a str>) -> Result<&'a str, String> {
    v.filter(|s| !s.is_empty()).ok_or_else(|| format!("run_test needs {what}: ask the engineer, do not guess it"))
}

/// Prepares a Test run. Never starts it.
pub async fn prepare(repo: &Path, core: &CoreClient, a: RunTestArgs<'_>) -> Report {
    let picked = a.all || !a.tests.is_empty() || !a.tags.is_empty();
    if a.plan.is_none() && !picked {
        return list_instead(repo, core, &a).await;
    }
    let plan: Plan = match build_plan(repo, core, &a).await {
        Ok(p) => p,
        Err(e) => return failed(format!("run_test: {e}")),
    };
    let (standard, identity) = match identity(&a) {
        Ok(x) => x,
        Err(e) => return failed(e),
    };
    let (manifest, source) = match load_manifest(repo, core, &plan.suite.id).await {
        Ok(x) => x,
        Err(e) => return failed(format!("run_test: {e}")),
    };
    let tests = match suite_tests(repo, core, &plan.suite.id, &manifest, source).await {
        Ok(t) => t,
        Err(e) => return failed(format!("run_test: reading suite {}: {e}", plan.suite.id)),
    };
    let studies = load_studies(repo, core, &plan.suite.id, &manifest, &tests, source).await;
    let plan_hash = definition_hash(&plan);
    let ctx = ExpandContext {
        run_id: fresh_run_id(),
        plan_hash: Some(plan_hash.clone()),
        standard,
        identity,
        resumes: None,
        benches_present: benches_present(repo, &manifest),
        // Core holds the confirmations; its comparison blanks `confirmed` on both sides.
        confirmations: Vec::new(),
    };
    let suite = SuiteContent { manifest: &manifest, tests: &tests, studies: &studies };
    let expansion = match expand(&suite, &plan, &plan.dut, &ctx) {
        Ok(x) => x,
        Err(e) => return failed(format!("run_test: refused before Core was called, by embarch-study-designer: {e:?}")),
    };
    let not_run: Vec<Value> = expansion
        .selection
        .entries
        .iter()
        .filter(|e| !matches!(e.status, SelectionStatus::Selected | SelectionStatus::NotSelected))
        .map(|e| json!({ "test_id": e.test_id, "status": e.status, "reason": e.reason }))
        .collect();
    if expansion.run.tests.is_empty() {
        let mut msg = "run_test: nothing to run: the selection expanded to no test".to_string();
        for n in &not_run {
            msg.push_str(&format!("\n  {}: {} ({})", n["test_id"].as_str().unwrap_or(""), n["status"].as_str().unwrap_or(""), n["reason"].as_str().unwrap_or("")));
        }
        return Report { success: false, value: json!({ "success": false, "error": msg, "not_run": not_run }), human: msg };
    }

    // Definitions first, so Core can resolve what run.json names; each is idempotent.
    let mut suite_hash: Option<String> = None;
    if source == "repo" {
        for t in &tests {
            if let Err(e) = keep(core, "test", serde_json::to_value(t).unwrap_or_default()).await {
                return failed(format!("run_test: Core did not keep test {}: {e}", t.test_id));
            }
        }
        match keep(core, "suite", serde_json::to_value(&manifest).unwrap_or_default()).await {
            Ok(h) => suite_hash = Some(h),
            Err(e) => return failed(format!("run_test: Core did not keep suite {}: {e}", manifest.id)),
        }
    }
    if let Err(e) = keep(core, "plan", serde_json::to_value(&plan).unwrap_or_default()).await {
        return failed(format!("run_test: Core did not keep the plan: {e}"));
    }
    let mut files: BTreeMap<String, Value> = studies.iter().filter_map(|(f, s)| serde_json::to_value(s).ok().map(|v| (f.clone(), v))).collect();
    for s in &expansion.studies {
        if let Ok(v) = serde_json::to_value(&s.study) {
            files.insert(s.file.clone(), v);
        }
    }
    let mut body = json!({ "run": expansion.run, "plan": plan_hash, "studies": files });
    if let Some(h) = &suite_hash {
        body["suite"] = json!(h);
    }
    let prepared = match core.prepare_test_run(&body).await {
        Ok(v) => v,
        Err(e) => return failed(format!("run_test: Core did not prepare the run: {e:#}")),
    };
    let run_id = prepared["run_id"].as_str().unwrap_or(&expansion.run.run_id).to_string();
    shown(core, &run_id, &prepared, &not_run, json!({})).await
}

/// Reads the card of a run Core just prepared and answers with it: the one path a fresh run and
/// a resume share, so neither can start the run or surface a nonce. `extra` is merged into the answer.
async fn shown(core: &CoreClient, run_id: &str, prepared: &Value, not_run: &[Value], extra: Value) -> Report {
    let card = core.test_run_card(run_id).await;
    let (card_value, card_note) = match card {
        Ok(mut v) => {
            // The agent has no use for a nonce and the go is not its to give.
            if let Some(o) = v.as_object_mut() {
                o.remove("nonce");
                o.remove("expires_utc_ms");
            }
            (v, None)
        }
        Err(e) => (Value::Null, Some(format!("the run is prepared but its card could not be read: {e:#}"))),
    };
    let mut human = render(run_id, prepared, &card_value, card_note.as_deref(), not_run);
    let mut value = json!({
        "success": true,
        "run_id": run_id,
        "run_hash": prepared["run_hash"],
        "duration_bound_ms": prepared["duration_bound_ms"],
        "lease_holder": prepared["lease_holder"],
        "started": false,
        "card": card_value,
        "not_run": not_run,
        "message": GO_MESSAGE,
    });
    if let (Some(v), Some(x)) = (value.as_object_mut(), extra.as_object()) {
        for (k, val) in x {
            v.insert(k.clone(), val.clone());
        }
    }
    if let Some(first) = extra["resumes"].as_str() {
        human = format!("resumes {first}: pending {}, kept {}\n{human}", plain_list(&extra["pending"]), plain_list(&extra["kept"]));
    }
    Report { success: true, value, human }
}

fn plain_list(v: &Value) -> String {
    v.as_array().map(|a| a.iter().map(plain).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

/// `resume_test`: asks Core to prepare a new run of what an interrupted run left pending
/// (`POST /test-runs/{id}/resume`, embarch-core decision 153) and shows its card. Never starts
/// it, and a refusal is Core's words, passed on once, never retried or reworded.
pub async fn resume(core: &CoreClient, interrupted: &str, new_run_id: Option<&str>) -> Report {
    let new_id = new_run_id.filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(fresh_run_id);
    let prepared = match core.resume_test_run(interrupted, &new_id).await {
        Ok(v) => v,
        Err(e) => return failed(format!("resume_test: {e:#}")),
    };
    let run_id = prepared["run_id"].as_str().unwrap_or(&new_id).to_string();
    let extra = json!({ "resumes": prepared["resumes"], "pending": prepared["pending"], "kept": prepared["kept"] });
    shown(core, &run_id, &prepared, &[], extra).await
}

const GO_MESSAGE: &str =
    "prepared, not started: the engineer gives the go on the card in the UI or in the terminal. You cannot, and the session must hold the bench lease when they do. Watch it with test_status.";

async fn keep(core: &CoreClient, kind: &str, definition: Value) -> Result<String, String> {
    let reply = core.post_definition(&json!({ "kind": kind, "definition": definition })).await.map_err(|e| format!("{e:#}"))?;
    Ok(reply["hash"].as_str().unwrap_or_default().to_string())
}

fn identity(a: &RunTestArgs<'_>) -> Result<(Standard, DutIdentity), String> {
    let name = need("`standard_name`, the standard the engineer is testing against", a.standard_name)?;
    let rev = need("`standard_revision`", a.standard_revision)?;
    let serial = need("`declared_serial`, the unit's serial", a.declared_serial)?;
    let board = need("`board_revision`", a.board_revision)?;
    Ok((
        Standard { name: name.to_string(), revision: rev.to_string() },
        DutIdentity { hardware_id: a.hardware_id.filter(|s| !s.is_empty()).map(str::to_string), declared_serial: serial.to_string(), board_revision: board.to_string() },
    ))
}

async fn build_plan(repo: &Path, core: &CoreClient, a: &RunTestArgs<'_>) -> Result<Plan, String> {
    if let Some(name) = a.plan {
        if !crate::hil_repo::is_slug(name) {
            return Err(format!("plan `{name}` is not a name"));
        }
        return read_plan(&tests_dir(repo).join("plans").join(format!("{name}.json")));
    }
    let id = a.suite.ok_or("name a plan, or a suite with tests, tags or all")?;
    let (manifest, _) = load_manifest(repo, core, id).await?;
    let capabilities: Vec<Capability> = a.capabilities.as_ref().map(|v| parse("capability list", v)).transpose()?.unwrap_or_default();
    let hooks: Vec<HookMapping> = a.hooks.as_ref().map(|v| parse("hook list", v)).transpose()?.unwrap_or_default();
    let select = if a.all { Selection::All } else { Selection::Some { tests: a.tests.clone(), tags: a.tags.clone() } };
    let plan = Plan {
        schema: 1,
        suite: SuitePin { id: id.to_string(), version: manifest.version.clone(), commit: a.commit.unwrap_or(UNPINNED).to_string() },
        select,
        dut: DutInputs { capabilities, hooks },
        stop_on_fail: a.stop_on_fail,
        bench_order: Vec::new(),
    };
    plan.check().map_err(|e| format!("{e:?}"))?;
    Ok(plan)
}

/// A spec named with no plan and no picks: list what applies and ask.
async fn list_instead(repo: &Path, core: &CoreClient, a: &RunTestArgs<'_>) -> Report {
    let Some(suite) = a.suite else {
        return failed("run_test: name a plan, or a suite (with tests, tags or all)");
    };
    let mut r = crate::hil_repo::plan(
        repo,
        core,
        crate::hil_repo::PlanArgs { plan: None, suite: Some(suite), definition: None, capabilities: a.capabilities.clone() },
    )
    .await;
    if r.success {
        let ask = "Nothing was prepared: no plan or test selection was given. Ask the engineer which tests to run (`tests`, `tags` or `all`), then call run_test again.";
        r.value["prepared"] = json!(false);
        r.value["ask"] = json!(ask);
        r.human.push('\n');
        r.human.push_str(ask);
    }
    r
}

fn render(run_id: &str, prepared: &Value, card: &Value, note: Option<&str>, not_run: &[Value]) -> String {
    let mut h = format!("run {run_id} prepared, not started (run_hash {})", prepared["run_hash"].as_str().unwrap_or("?"));
    if let Some(n) = note {
        h.push_str(&format!("\n  {n}"));
    }
    let c = &card["card"];
    if let Some(ms) = c["duration_bound_ms"].as_u64().or_else(|| prepared["duration_bound_ms"].as_u64()) {
        h.push_str(&format!("\n  duration bound: {ms} ms (timeouts, an upper bound)"));
    }
    for b in c["benches"].as_array().into_iter().flatten() {
        h.push_str(&format!("\n  bench {}: {}", plain(&b["name"]), b["tests"].as_array().map(|t| t.iter().map(plain).collect::<Vec<_>>().join(", ")).unwrap_or_default()));
    }
    for t in c["tests"].as_array().into_iter().flatten() {
        for p in t["prompts"].as_array().into_iter().flatten() {
            h.push_str(&format!("\n  prompt in {} ({}): {}", plain(&t["test_id"]), plain(&p["kind"]), plain(&p["text"])));
        }
        for e in t["energising"].as_array().into_iter().flatten() {
            h.push_str(&format!("\n  powers in {}: {} <- {}", plain(&t["test_id"]), plain(&e["target"]), plain(&e["line"])));
        }
    }
    let unconfirmed: Vec<&Value> = c["unconfirmed"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    if !unconfirmed.is_empty() {
        let names: BTreeSet<String> = unconfirmed.iter().map(|u| format!("{} {}", plain(&u["kind"]), plain(&u["name"]))).collect();
        h.push_str(&format!("\n  unconfirmed, the run cannot start until the engineer confirms: {}", names.into_iter().collect::<Vec<_>>().join("; ")));
    }
    for n in not_run {
        h.push_str(&format!("\n  not in the run: {} ({})", n["test_id"].as_str().unwrap_or(""), n["reason"].as_str().unwrap_or("")));
    }
    h.push('\n');
    h.push_str(GO_MESSAGE);
    h
}

/// What `answer_prompt` adds to a refusal: a `403` on a Test run's prompt is the observation
/// or clearance rule (core decision 138), which no retry changes.
pub fn prompt_refusal_hint(err: &str) -> &'static str {
    if err.contains("403") {
        " | Core refuses a relayed answer to an observation or a clearance: the person answers it on the card in the UI or in their terminal. Do not retry."
    } else {
        ""
    }
}

/// `test_status`: Core's answer for the run, with the relay rule beside a waiting prompt,
/// and each study's own status underneath when asked.
pub async fn status(core: &CoreClient, run_id: &str, with_studies: bool) -> Report {
    let mut v = match core.test_run_status(run_id).await {
        Ok(v) => v,
        Err(e) => return failed(format!("test_status failed for '{run_id}': {e:#}")),
    };
    let kind = v["waiting_prompt"]["kind"].as_str().map(str::to_string);
    if let Some(k) = &kind {
        let relay = matches!(k.as_str(), "setup" | "action");
        v["prompt_relay"] = json!({
            "kind": k,
            "relayable": relay,
            "study_id": if k == "setup" { json!(run_id) } else { Value::Null },
            "say": if relay {
                "show the text to the person you are working for and relay exactly what they said with answer_prompt"
            } else {
                "an observation or a clearance is the person's own: Core refuses a relayed answer; they answer it on the card in the UI or in the terminal"
            },
        });
    }
    if with_studies {
        let mut studies = Vec::new();
        for t in v["verdicts"].as_array().into_iter().flatten() {
            for id in t["study_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                studies.push(match core.get_study_status(id).await {
                    Ok(s) => json!({ "study_id": id, "status": crate::host_study::study_status_json(&s) }),
                    Err(e) => json!({ "study_id": id, "error": format!("{e:#}") }),
                });
            }
        }
        v["studies"] = Value::Array(studies);
    }
    let mut human = format!("run {run_id}: {}", plain(&v["state"]));
    if !v["bench"].is_null() {
        human.push_str(&format!(", bench {}", plain(&v["bench"])));
    }
    if !v["test"].is_null() {
        human.push_str(&format!(", test {}", plain(&v["test"])));
    }
    let verdicts = v["verdicts"].as_array().cloned().unwrap_or_default();
    human.push_str(&format!(", {} verdict(s) so far", verdicts.len()));
    for t in &verdicts {
        human.push_str(&format!("\n  {} on {}: {}", plain(&t["test_id"]), plain(&t["bench"]), plain(&t["verdict"])));
        if !t["reason"].is_null() {
            human.push_str(&format!(" ({})", plain(&t["reason"])));
        }
    }
    if let Some(k) = &kind {
        human.push_str(&format!("\n  waiting prompt [{k}]: {}", plain(&v["waiting_prompt"]["text"])));
        human.push_str(&format!("\n  {}", v["prompt_relay"]["say"].as_str().unwrap_or("")));
    }
    Report { success: true, value: json!({ "success": true, "run": v }), human }
}
