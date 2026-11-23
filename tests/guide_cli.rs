//! `embarch-api guide hil` serves the guide embedded at build (`tasks/api/163`,
//! decision 108): from a directory with no `embarch-doc` and no config, and
//! every command the `embarch-hil` skill names resolves.

use std::path::PathBuf;
use std::process::{Command, Output};

fn run_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .current_dir(dir)
        .env_remove("EMBARCH_API_CONFIG")
        .args(args)
        .output()
        .expect("failed to run embarch-api")
}

fn empty_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("embarch-guide-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn each_mode_prints_its_section_with_no_embarch_doc_reachable() {
    let dir = empty_dir("modes");
    for (mode, heading) in [("study", "## Study"), ("test", "## Test"), ("live", "## Live")] {
        let out = run_in(&dir, &["guide", "hil", mode]);
        assert!(out.status.success(), "{mode}: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.starts_with(heading), "{mode}: {text:?}");
    }
    let out = run_in(&dir, &["guide", "hil"]);
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().starts_with("## Every mode"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_mode_exits_nonzero_and_lists_the_modes() {
    let dir = empty_dir("bad");
    let out = run_in(&dir, &["guide", "hil", "bogus"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("live, study, test"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every `embarch-api guide hil ...` the skill names runs and succeeds.
#[test]
fn every_pointer_in_the_skill_resolves() {
    let skill = include_str!("../skills/embarch-hil/SKILL.md");
    let dir = empty_dir("skill");
    let mut seen = 0;
    for line in skill.lines() {
        let Some(rest) = line.split('`').nth(1).and_then(|c| c.strip_prefix("embarch-api ")) else {
            continue;
        };
        let args: Vec<&str> = rest.split_whitespace().collect();
        let out = run_in(&dir, &args);
        assert!(out.status.success(), "`embarch-api {rest}` does not resolve");
        seen += 1;
    }
    assert!(seen >= 4, "expected the skill to name the guide commands, saw {seen}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Drift guard: where a checkout of `embarch-doc` is reachable (the
/// `EMBARCH_DOC_DIR` variable, else a sibling `embarch-doc`), and it already
/// has `suite/hil-guide.md`, the pinned copy must equal it. Silent when there
/// is none, which is every adopter's machine and today's.
#[test]
fn the_pinned_copy_matches_the_suite_guide_where_one_is_reachable() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = std::env::var_os("EMBARCH_DOC_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("../embarch-doc"), root.join("../../embarch-doc/main")]);
    for doc in candidates {
        let suite = doc.join("suite/hil-guide.md");
        if let Ok(theirs) = std::fs::read_to_string(&suite) {
            let ours = include_str!("../guide/hil-guide.md");
            assert_eq!(
                ours, theirs,
                "guide/hil-guide.md has drifted from {}: copy the suite guide over it",
                suite.display()
            );
            return;
        }
    }
}
