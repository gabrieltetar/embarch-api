//! The live log (`tasks/api/180`, `interfaces/live-log.md`) against a mock
//! Core: a gated call appends one line citing Core's call-log entry, the
//! board and the image hash; a refusal appends its own line; `--json` carries
//! Core's entry; `call-log` reads an entry back by `seq`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{json, Value};
use support::{Behavior, MockCore};

const SEQ: u64 = 41;
const HASH: &str = "9f2c0000000000000000000000000000000000000000000000000000000000aa";
const IMAGE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn mem_read_answer() -> Value {
    json!({
        "role": "dut", "chip": "nRF54L15", "probe_serial": "000683", "ranges": [{"address": 0x2000_0000u64, "words": [7]}],
        "halted": true, "halted_us": 12, "found": "running", "left": "running", "halt_count": 1,
    })
}

fn status_answer() -> Value {
    json!({
        "status": "ok", "probes": [],
        "last_images": [{"chip": "nRF54L15", "probe_serial": "000683", "path": "/x/zephyr.hex", "sha256": IMAGE, "flashed_at": "2026-11-23T20:00:00Z"}],
    })
}

fn router(gated: Behavior) -> Behavior {
    Behavior::Router {
        routes: vec![
            ("/live/mem-read".into(), gated.clone()),
            ("/reset".into(), gated),
            ("/status".into(), Behavior::json_ok(status_answer())),
            ("/lease".into(), Behavior::plain_text_error(404, "Not Found", "no lease")),
            (
                "/call-log".into(),
                Behavior::json_ok(json!({
                    "verified": true, "error": null, "length": 42, "head": HASH, "from": SEQ, "to": SEQ + 1,
                    "truncated": false, "entries": [{"entry": {"seq": SEQ, "route": "/live/mem-read"}, "hash": HASH}], "pending_fold": null,
                })),
            ),
        ],
        otherwise: Box::new(Behavior::plain_text_error(404, "Not Found", "unrouted")),
    }
}

struct Run {
    out: Output,
    repo: PathBuf,
}

fn run(mock: &MockCore, args: &[&str]) -> Run {
    let dir = support::unique_dir("livelog");
    let repo = dir.join("fw");
    std::fs::create_dir_all(repo.join("embarch")).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(
        &cfg,
        format!(
            "[core]\nbase_url = \"{}\"\ntoken = \"t\"\n\n[[projects]]\nname = \"p\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nflash_format = \"hex\"\nchip = \"nRF54L15\"\n",
            mock.base_url(),
            repo.display()
        ),
    )
    .unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .current_dir(&repo)
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    Run { out, repo }
}

fn lines(repo: &Path) -> Vec<Value> {
    let live = repo.join("embarch").join("live");
    let Ok(rd) = std::fs::read_dir(&live) else { return vec![] };
    let mut out = vec![];
    for f in rd {
        let f = f.unwrap().path();
        assert!(f.extension().is_some_and(|e| e == "jsonl"), "{f:?}");
        for l in std::fs::read_to_string(&f).unwrap().lines() {
            out.push(serde_json::from_str(l).unwrap());
        }
    }
    out
}

fn stamped_ok() -> Behavior {
    Behavior::stamped(200, "OK", "application/json", &mem_read_answer().to_string(), SEQ, HASH)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mem_read_appends_one_line_naming_the_entry_the_board_and_the_image() {
    let mock = MockCore::start(router(stamped_ok())).await;
    let r = run(&mock, &["mem-read", "--range", "0x20000000"]);
    assert_eq!(r.out.status.code(), Some(0), "{}", String::from_utf8_lossy(&r.out.stderr));
    let l = lines(&r.repo);
    assert_eq!(l.len(), 1, "{l:?}");
    let l = &l[0];
    assert_eq!(l["schema"], "embarch-live/1");
    assert_eq!(l["tool"], "mem-read");
    assert_eq!(l["via"], "cli");
    assert_eq!(l["core"][0]["seq"], SEQ);
    assert_eq!(l["core"][0]["hash"], HASH);
    assert_eq!(l["core"][0]["path"], "/live/mem-read");
    assert_eq!(l["board"]["role"], "dut");
    assert_eq!(l["board"]["chip"], "nRF54L15");
    assert_eq!(l["image_sha256"], IMAGE);
    assert_eq!(l["ok"], true);
    assert_eq!(l["answer"]["truncated"], false);
    assert!(l["t"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reset_appends_one_line_with_the_board_from_its_request() {
    let mock = MockCore::start(router(Behavior::stamped(200, "OK", "application/json", r#"{"reset":true}"#, 5, HASH))).await;
    let r = run(&mock, &["reset", "p"]);
    assert_eq!(r.out.status.code(), Some(0), "{}", String::from_utf8_lossy(&r.out.stderr));
    let l = lines(&r.repo);
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0]["tool"], "reset");
    assert_eq!(l[0]["core"][0]["seq"], 5);
    assert_eq!(l[0]["board"]["chip"], "nRF54L15");
    assert_eq!(l[0]["image_sha256"], IMAGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_appends_a_line_with_the_refusal() {
    let refused = Behavior::stamped(409, "Conflict", "text/plain", "bench held by someone else", 6, HASH);
    let mock = MockCore::start(router(refused)).await;
    let r = run(&mock, &["mem-read", "--range", "0x20000000"]);
    assert_eq!(r.out.status.code(), Some(1));
    let l = lines(&r.repo);
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0]["ok"], false);
    assert_eq!(l[0]["core"][0]["status"], 409);
    assert_eq!(l[0]["core"][0]["seq"], 6);
    assert!(l[0]["answer"]["text"].as_str().unwrap().contains("bench held by someone else"));
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_carries_cores_entry() {
    let mock = MockCore::start(router(stamped_ok())).await;
    let r = run(&mock, &["--json", "mem-read", "--range", "0x20000000"]);
    let v: Value = serde_json::from_slice(&r.out.stdout).unwrap();
    assert_eq!(v["core_calls"][0]["seq"], SEQ);
    assert_eq!(v["core_calls"][0]["hash"], HASH);
}

#[tokio::test(flavor = "multi_thread")]
async fn call_log_returns_an_entry_by_seq() {
    let mock = MockCore::start(router(stamped_ok())).await;
    let r = run(&mock, &["--json", "call-log", "--seq", &SEQ.to_string()]);
    assert_eq!(r.out.status.code(), Some(0), "{}", String::from_utf8_lossy(&r.out.stderr));
    let v: Value = serde_json::from_slice(&r.out.stdout).unwrap();
    assert_eq!(v["call_log"]["entries"][0]["entry"]["seq"], SEQ);
    let asked = mock.requests().into_iter().find(|q| q.path() == "/call-log").unwrap();
    assert!(asked.target.contains(&format!("from={SEQ}")) && asked.target.contains(&format!("to={}", SEQ + 1)), "{}", asked.target);
    // Reading the log is not a gated call: no line.
    assert!(lines(&r.repo).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_with_no_repo_says_so_and_writes_nothing() {
    let mock = MockCore::start(router(stamped_ok())).await;
    let dir = support::unique_dir("livelog-norepo");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .current_dir(&dir)
        .arg("--config")
        .arg(&cfg)
        .args(["mem-read", "--range", "0x20000000"])
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("live log: no line written"), "{}", String::from_utf8_lossy(&out.stderr));
}
