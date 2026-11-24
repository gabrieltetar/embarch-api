//! The live log: one JSON line in the firmware repo for every tool call or
//! subcommand that reached a lease-gated route (`interfaces/live-log.md`).
//!
//! A readable mirror of Core's call log (`embarch-core` decision 122): what was
//! sent, to which board, the image hash on it, the time, the answer and
//! Core's entries. It is written after Core answers, in append mode with one
//! `write` per line so `embarch-atlas` can append to the same file.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use embarch_core_client::call_trace::CallEntry;
use embarch_core_client::CoreClient;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::Config;

/// The `schema` field of a line; `embarch-atlas` writes the same one.
pub const SCHEMA: &str = "embarch-live/1";
/// An answer longer than this is cut in the line; its full length and SHA-256
/// ride beside it. Chosen, not measured: a mem-read or a console exchange of a
/// few hundred bytes fits whole, a serial log does not.
pub const ANSWER_CAP_BYTES: usize = 4096;

/// What one finished tool call or subcommand is, for [`record`].
pub struct Call<'a> {
    /// `"mcp"` or `"cli"`.
    pub via: &'static str,
    pub tool: &'a str,
    /// The tool's arguments (MCP) or the subcommand as the CLI parsed it.
    pub sent: Value,
    /// A `project` the call named, which picks the repo when the process is in none.
    pub project: Option<&'a str>,
    pub ok: bool,
    pub answer: &'a str,
    /// The JSON the answer was made from, when it is not itself JSON text.
    pub structured: Option<&'a Value>,
    pub entries: &'a [CallEntry],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The call reached no lease-gated route (or only a study's), so it is not Live.
    NotLive,
    Written(PathBuf),
    /// A Live call with nowhere to put its line, or a write that failed.
    NotWritten(String),
}

impl Outcome {
    /// What to tell the caller when a Live call left no line.
    pub fn notice(&self) -> Option<String> {
        match self {
            Outcome::NotWritten(why) => Some(format!("live log: no line written ({why})")),
            _ => None,
        }
    }
}

tokio::task_local! {
    static CLI_ANSWER: Arc<Mutex<Option<(String, Value)>>>;
}

/// Runs a CLI subcommand with a slot its printer fills with the answer text.
pub async fn with_cli_answer<F: std::future::Future>(fut: F) -> (F::Output, Option<(String, Value)>) {
    let slot: Arc<Mutex<Option<(String, Value)>>> = Arc::default();
    let out = CLI_ANSWER.scope(slot.clone(), fut).await;
    let answer = slot.lock().ok().and_then(|mut g| g.take());
    (out, answer)
}

/// The CLI printer's hand-off: the text of the answer it is about to print and
/// the JSON it was made from (which names the board when the text does not).
pub fn note_cli_answer(text: &str, value: &Value) {
    let _ = CLI_ANSWER.try_with(|s| {
        if let Ok(mut g) = s.lock() {
            *g = Some((text.to_string(), value.clone()));
        }
    });
}

/// `MemRead` -> `mem-read`: a subcommand's name from its `Debug` form.
pub fn cli_tool_name(debug: &str) -> String {
    let ident: String = debug.chars().take_while(|c| c.is_alphanumeric()).collect();
    let mut out = String::new();
    for (i, c) in ident.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('-');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// A study is not Live (`run_study`, `run_host_study`): its steps stay in its
/// own `events.json`.
fn is_study_route(path: &str) -> bool {
    path == "/study" || path == "/host-studies" || path.starts_with("/study/")
}

/// The entries that make a call Live.
pub fn live_entries(entries: &[CallEntry]) -> Vec<&CallEntry> {
    entries.iter().filter(|e| !is_study_route(&e.path)).collect()
}

/// Core's entries as the lines and `--json` carry them.
pub fn core_calls(entries: &[CallEntry]) -> Value {
    entries
        .iter()
        .map(|e| json!({ "seq": e.seq, "hash": e.hash, "repeat": e.repeat, "method": e.method, "path": e.path, "status": e.status }))
        .collect()
}

/// The repo whose `embarch/` this process runs in: the nearest directory from
/// the working directory up to its git root that holds an `embarch/` folder.
fn repo_from_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for dir in cwd.ancestors() {
        if dir.join("embarch").is_dir() {
            return Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// The firmware repo a line goes to: the one this process runs in, else the
/// configured project's source path (the named one, or the only one).
pub fn repo_dir(config: &Config, project: Option<&str>) -> Result<PathBuf, String> {
    if let Some(dir) = repo_from_cwd() {
        return Ok(dir);
    }
    let configured = match project.filter(|p| !p.is_empty() && *p != "desk") {
        Some(name) => config.project(name).map_err(|e| e.to_string())?.source_path.clone(),
        None => match config.projects.as_slice() {
            [only] => only.source_path.clone(),
            [] => return Err("this process is in no repo with an embarch/ folder and no project is configured".into()),
            _ => return Err("this process is in no repo with an embarch/ folder and several projects are configured".into()),
        },
    };
    if configured.is_dir() {
        Ok(configured)
    } else {
        Err(format!("the project's source path {} is not a directory", configured.display()))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The answer cut at [`ANSWER_CAP_BYTES`] on a character boundary, with its
/// full size and SHA-256.
fn answer_value(answer: &str) -> Value {
    let truncated = answer.len() > ANSWER_CAP_BYTES;
    let mut end = answer.len().min(ANSWER_CAP_BYTES);
    while !answer.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "text": &answer[..end],
        "bytes": answer.len(),
        "sha256": sha256_hex(answer.as_bytes()),
        "truncated": truncated,
    })
}

/// The board a call reached: the first role, probe serial and chip any call's
/// body named, then the tool's own arguments, then the answer's.
fn board(entries: &[&CallEntry], sent: &Value, answer: &str, structured: Option<&Value>) -> (Option<String>, Option<String>, Option<String>) {
    let parsed: Option<Value> = serde_json::from_str(answer).ok().or_else(|| structured.cloned());
    let from_answer = |key: &str| -> Option<String> {
        let obj = parsed.as_ref()?.as_object()?;
        let direct = obj.get(key).and_then(Value::as_str);
        let nested = obj.values().filter_map(Value::as_object).find_map(|o| o.get(key).and_then(Value::as_str));
        direct.or(nested).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let pick = |key: &str| -> Option<String> {
        entries
            .iter()
            .filter_map(|e| e.sent.as_ref())
            .chain(std::iter::once(sent))
            .find_map(|v| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
            .or_else(|| from_answer(key))
    };
    (pick("role"), pick("probe_serial"), pick("chip"))
}

/// The session a line belongs to, from the live lease: the file name's
/// `<date>-<session>` and the lease's holder and start.
fn session(lease: Option<&embarch_core_client::LeaseView>, now: &str) -> (String, Value) {
    let started = lease.map(|l| l.started_at.as_str()).filter(|s| s.len() >= 19 && s.is_char_boundary(19) && s.as_bytes()[10] == b'T');
    match (lease, started) {
        (Some(l), Some(s)) => {
            let holder: String = l.holder.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            let clock: String = s[11..19].chars().filter(|c| c.is_ascii_digit()).collect();
            (format!("{}-{holder}-{clock}", &s[..10]), json!({ "holder": l.holder, "started_at": l.started_at }))
        }
        _ => (format!("{}-no-lease", &now[..10]), Value::Null),
    }
}

/// The sha256 Core last put on the board, from `GET /status`'s `last_images`;
/// `"unknown"` when Core says nothing about it, which is not "nothing flashed".
fn image_sha256(status: Option<&embarch_core_client::StatusResponse>, chip: Option<&str>, probe: Option<&str>) -> String {
    let images = status.and_then(|s| s.last_images.as_deref()).unwrap_or(&[]);
    let by_chip = |i: &&embarch_core_client::FlashedImage| chip.is_some_and(|c| i.chip.eq_ignore_ascii_case(c));
    let by_probe = |i: &&embarch_core_client::FlashedImage| probe.is_some() && i.probe_serial.as_deref() == probe;
    images
        .iter()
        .find(|i| by_chip(i) && by_probe(i))
        .or_else(|| images.iter().find(by_probe))
        .or_else(|| images.iter().find(by_chip))
        .and_then(|i| i.sha256.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    crate::ledger::utc_stamp(secs)
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // One `write` of the whole line, so a reader never sees half of one and a
    // second writer's line lands whole beside it.
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{line}\n").as_bytes())
}

/// Appends the line for a finished call, when it was Live.
pub async fn record(config: &Config, core: &CoreClient, call: Call<'_>) -> Outcome {
    let live = live_entries(call.entries);
    if live.is_empty() {
        return Outcome::NotLive;
    }
    let repo = match repo_dir(config, call.project) {
        Ok(r) => r,
        Err(why) => return Outcome::NotWritten(why),
    };
    let now = now_stamp();
    let (lease, status) = tokio::join!(core.lease_show(), core.status());
    let lease = lease.ok().flatten();
    let (file_stem, session) = session(lease.as_ref(), &now);
    let (role, probe_serial, chip) = board(&live, &call.sent, call.answer, call.structured);
    let image = image_sha256(status.as_ref().ok(), chip.as_deref(), probe_serial.as_deref());
    let live_owned: Vec<CallEntry> = live.iter().map(|e| (*e).clone()).collect();
    let line = json!({
        "schema": SCHEMA,
        "t": now,
        "writer": "embarch-api",
        "via": call.via,
        "tool": call.tool,
        "sent": call.sent,
        "board": { "role": role, "probe_serial": probe_serial, "chip": chip },
        "image_sha256": image,
        "ok": call.ok,
        "answer": answer_value(call.answer),
        "core": core_calls(&live_owned),
        "session": session,
    });
    let path = repo.join("embarch").join("live").join(format!("{file_stem}.jsonl"));
    match append(&path, &line.to_string()) {
        Ok(()) => Outcome::Written(path),
        Err(e) => Outcome::NotWritten(format!("could not write {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subcommand_name_is_kebab_cased() {
        assert_eq!(cli_tool_name("MemRead { ranges: [] }"), "mem-read");
        assert_eq!(cli_tool_name("Reset { project: \"x\" }"), "reset");
        assert_eq!(cli_tool_name("CoreState"), "core-state");
    }

    #[test]
    fn an_answer_is_cut_on_a_char_boundary_and_keeps_its_full_hash() {
        let long = "é".repeat(ANSWER_CAP_BYTES);
        let v = answer_value(&long);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["bytes"], long.len());
        assert_eq!(v["sha256"], sha256_hex(long.as_bytes()));
        assert!(v["text"].as_str().unwrap().len() <= ANSWER_CAP_BYTES);
        assert_eq!(answer_value("short")["truncated"], false);
    }

    #[test]
    fn a_study_route_is_not_live() {
        assert!(is_study_route("/study"));
        assert!(is_study_route("/host-studies"));
        assert!(is_study_route("/study/abc/cancel"));
        assert!(!is_study_route("/live/mem-read"));
    }
}
