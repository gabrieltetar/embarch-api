//! `run-study` tells Core what it built (decision 111): the project
//! checkout's commit and dirty state ride `POST /study` as `build_commit` and
//! `build_dirty`, and a project with no git checkout sends none of them.
//! Driven through the real subprocess against a mock Core.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use support::{Behavior, MockCore};

async fn core() -> MockCore {
    MockCore::start(Behavior::Router {
        routes: vec![(
            "/status".to_string(),
            Behavior::json_ok(json!({
                "status": "ok",
                "probes": [],
                "study_designer_schema_version": embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
            })),
        )],
        otherwise: Box::new(Behavior::json_ok(json!({ "study_id": "s-claims", "status": "accepted" }))),
    })
    .await
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// Submits the fixture study naming project `fw` at `src`, and returns the
/// query string `POST /study` arrived with.
fn submitted_query(mock: &MockCore, dir: &Path, src: &Path) -> String {
    let config = dir.join("api.toml");
    std::fs::write(
        &config,
        format!(
            "[core]\nbase_url = \"{}\"\ntoken = \"build-claims-test-token\"\n\n[[projects]]\nname = \"fw\"\nsource_path = \"{}\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\nchip = \"nRF54L15\"\nflash_format = \"hex\"\n",
            mock.base_url(),
            src.display()
        ),
    )
    .unwrap();
    let study = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/self_test_study.json");
    let data = dir.join("data");
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&config)
        .arg("--json")
        .args(["run-study", "--study-file"])
        .arg(study)
        .args(["--project", "fw"])
        .env("XDG_DATA_HOME", &data)
        .env("LOCALAPPDATA", &data)
        .output()
        .unwrap();
    let post = mock
        .requests()
        .into_iter()
        .rfind(|r| r.route() == ("POST".to_string(), "/study".to_string()))
        .unwrap_or_else(|| panic!("no POST /study: {}", String::from_utf8_lossy(&out.stdout)));
    post.target.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default()
}

fn scratch(tag: &str) -> PathBuf {
    let d = support::unique_dir(tag);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn a_git_project_sends_its_commit_and_dirty_state() {
    let mock = core().await;
    let dir = scratch("claims-git");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q"]);
    std::fs::write(src.join("f"), "1").unwrap();
    git(&src, &["add", "f"]);
    git(&src, &["commit", "-q", "-m", "x"]);
    let head = String::from_utf8(Command::new("git").arg("-C").arg(&src).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();

    let clean = submitted_query(&mock, &dir, &src);
    assert!(clean.contains(&format!("build_commit={}", head.trim())), "{clean}");
    assert!(clean.contains("build_dirty=false"), "{clean}");
    // A `static` project's build command is not asked what it is.
    assert!(!clean.contains("build_toolchain"), "{clean}");

    std::fs::write(src.join("f"), "2").unwrap();
    let dirty = submitted_query(&mock, &dir, &src);
    assert!(dirty.contains("build_dirty=true"), "{dirty}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_with_no_git_checkout_sends_no_build_claim() {
    let mock = core().await;
    let dir = scratch("claims-nogit");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let query = submitted_query(&mock, &dir, &src);
    assert!(!query.contains("build_"), "a build with no checkout must claim nothing: {query}");
    let _ = std::fs::remove_dir_all(&dir);
}
