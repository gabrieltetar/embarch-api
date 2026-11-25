//! Runs every case in `tests/fixtures/live-log/` through this writer's own
//! code (`interfaces/live-log.md`, "The shared fixture"). The cases are plain
//! JSON so `embarch-atlas`'s test reads the same files; a rule change lands
//! there first.

use std::path::{Path, PathBuf};

use embarch_core_client::call_trace::CallEntry;
use embarch_core_client::{LeaseView, StatusResponse};
use serde_json::Value;

use super::*;

/// The contract fields a case does not pin: they differ by writer or by clock.
const NOT_COMPARED: [&str; 4] = ["t", "writer", "via", "sent"];

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn entry(v: &Value) -> CallEntry {
    CallEntry {
        method: str_of(&v["method"]).unwrap(),
        path: str_of(&v["path"]).unwrap(),
        status: v["status"].as_u64().unwrap() as u16,
        seq: v["seq"].as_u64(),
        hash: str_of(&v["hash"]),
        repeat: v["repeat"].as_u64(),
        sent: (!v["sent"].is_null()).then(|| v["sent"].clone()),
        query: None,
    }
}

fn materialize(root: &Path, layout: &Value) {
    for d in layout["dirs"].as_array().unwrap() {
        std::fs::create_dir_all(root.join(d.as_str().unwrap())).unwrap();
    }
    for f in layout["files"].as_array().unwrap() {
        let p = root.join(f.as_str().unwrap());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }
}

fn config(root: &Path, projects: &Value) -> Config {
    let mut toml = String::from("[core]\nbase_url = \"http://127.0.0.1:1\"\ntoken = \"t\"\n");
    for p in projects.as_array().unwrap() {
        toml.push_str(&format!(
            "\n[[projects]]\nname = \"{}\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nflash_format = \"hex\"\n",
            p["name"].as_str().unwrap(),
            root.join(p["source_path"].as_str().unwrap()).display().to_string().replace('\\', "/"),
        ));
    }
    ::toml::from_str(&toml).unwrap()
}

fn run_case(file: &Path) {
    let case: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let name = file.file_name().unwrap().to_string_lossy().to_string();
    let root = std::env::temp_dir().join(format!("embarch-live-fixture-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&root);
    materialize(&root, &case["layout"]);

    let c = &case["call"];
    let entries: Vec<CallEntry> = c["entries"].as_array().unwrap().iter().map(entry).collect();
    let structured = (!c["structured"].is_null()).then(|| c["structured"].clone());
    let call = Call {
        via: "mcp",
        tool: c["tool"].as_str().unwrap(),
        sent: c["sent"].clone(),
        project: c["project"].as_str(),
        ok: c["ok"].as_bool().unwrap(),
        answer: c["answer"].as_str().unwrap(),
        structured: structured.as_ref(),
        entries: &entries,
    };
    let lease = (!case["lease"].is_null()).then(|| LeaseView {
        holder: str_of(&case["lease"]["holder"]).unwrap(),
        purpose: "fixture".into(),
        started_at: str_of(&case["lease"]["started_at"]).unwrap(),
        expires_at: "2099-01-01T00:00:00Z".into(),
        ttl_s: 1,
    });
    let status: Option<StatusResponse> = (!case["status"].is_null()).then(|| serde_json::from_value(case["status"].clone()).unwrap());
    let now = case["now"].as_str().unwrap();

    let repo = repo_dir_from(repo_above(&root.join(case["cwd"].as_str().unwrap())), &config(&root, &case["projects"]), call.project);
    let live = live_entries(&entries);
    let (stem, mut line) = build_line(&call, &live, lease.as_ref(), status.as_ref(), now);

    let expect = &case["expect"];
    let got_path = repo.ok().map(|r| r.join("embarch").join("live").join(format!("{stem}.jsonl")));
    let want_path = expect["path"].as_str().map(|p| root.join(p));
    assert_eq!(got_path, want_path, "{name}: path");
    if want_path.is_some() {
        assert_eq!(line["t"], now, "{name}: t is the time passed in");
        let obj = line.as_object_mut().unwrap();
        for k in NOT_COMPARED {
            obj.remove(k);
        }
        assert_eq!(&line, &expect["line"], "{name}: line");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_shared_fixture_case_comes_out_as_pinned() {
    let dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "fixtures", "live-log"].iter().collect();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    cases.sort();
    assert!(cases.len() >= 15, "the fixture shrank: {}", cases.len());
    for c in &cases {
        run_case(c);
    }
}
