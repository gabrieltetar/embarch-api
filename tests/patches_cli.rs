//! `embarch-api patches` against a tiny throwaway git repo (`tasks/api/129`,
//! decision 92). Never touches a real west workspace: the module, the patch
//! files and `patches.yml` are all built in a temp dir below.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("embarch-patches-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Author")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05+00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05+00:00")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

struct Fixture {
    _tmp: Tmp,
    root: PathBuf,
    module: PathBuf,
    base: String,
    series: PathBuf,
    scratch: PathBuf,
}

/// A module repo with a base commit, a 3-patch series generated from it with
/// `git format-patch`, and the `patches.yml` naming them (with bogus sha256s).
fn fixture(tag: &str) -> Fixture {
    let tmp = Tmp::new(tag);
    let root = tmp.0.clone();
    let module = root.join("module");
    std::fs::create_dir_all(&module).unwrap();
    git(&module, &["init", "-q", "-b", "main"]);
    std::fs::write(module.join("a.txt"), lines(20)).unwrap();
    git(&module, &["add", "."]);
    git(&module, &["commit", "-q", "-m", "base"]);
    let base = git(&module, &["rev-parse", "HEAD"]).trim().to_string();
    // patch 1: edits line 2 of a.txt
    let mut a = lines(20).replace("line 2\n", "line 2 patched\n");
    std::fs::write(module.join("a.txt"), &a).unwrap();
    git(&module, &["commit", "-q", "-am", "first: patch a line 2"]);
    // patch 2: new file
    std::fs::write(module.join("b.txt"), "b\n").unwrap();
    git(&module, &["add", "."]);
    git(&module, &["commit", "-q", "-m", "second: add b\n\nBody of the second patch."]);
    // patch 3: edits line 15
    a = a.replace("line 15\n", "line 15 patched\n");
    std::fs::write(module.join("a.txt"), &a).unwrap();
    git(&module, &["commit", "-q", "-am", "third: patch a line 15\n\nBuilds on patch 0002 and see patch 0009."]);
    let pdir = root.join("manifest/patches");
    std::fs::create_dir_all(&pdir).unwrap();
    git(&module, &["format-patch", "--zero-commit", "--no-signature", "-3", "-o", &pdir.to_string_lossy()]);
    git(&module, &["reset", "-q", "--hard", &base]);
    let mut names: Vec<String> = std::fs::read_dir(&pdir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
    names.sort();
    let mut yml = String::from("# a manifest's patches\npatches:\n");
    for n in &names {
        yml.push_str(&format!("  - path: patches/{n}\n    sha256sum: 0000\n    module: zephyr\n    author: A\n"));
    }
    let series = root.join("manifest/patches.yml");
    std::fs::write(&series, yml).unwrap();
    let scratch = root.join("scratch");
    Fixture { _tmp: tmp, root, module, base, series, scratch }
}

fn run(args: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api")).arg("--json").arg("patches").args(args).output().unwrap();
    let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.success(), v)
}

fn export(f: &Fixture) {
    let (ok, v) = run(&[
        "export", "--series", &f.series.to_string_lossy(), "--repo", &f.module.to_string_lossy(),
        "--base", &f.base, "--scratch", &f.scratch.to_string_lossy(),
    ]);
    assert!(ok, "{v}");
}

fn import(f: &Fixture) -> serde_json::Value {
    let (ok, v) = run(&["import", "--scratch", &f.scratch.to_string_lossy()]);
    assert!(ok, "{v}");
    v
}

fn files(f: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(f.root.join("manifest/patches"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap())
        })
        .collect();
    v.sort();
    v
}

fn sha(bytes: &[u8]) -> String {
    use std::io::Write;
    let mut c = Command::new("sha256sum").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(bytes).unwrap();
    String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap().split_whitespace().next().unwrap().to_string()
}

#[test]
fn round_trip_is_byte_identical_and_updates_sha256() {
    let f = fixture("roundtrip");
    let before = files(&f);
    export(&f);
    assert_eq!(git(&f.scratch, &["rev-list", "--count", &format!("{}..HEAD", f.base)]).trim(), "3");
    assert_eq!(git(&f.scratch, &["log", "-1", "--format=%an"]).trim(), "Fixture Author");
    let v = import(&f);
    assert_eq!(v["changed"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(files(&f), before);
    let yml = std::fs::read_to_string(&f.series).unwrap();
    for (name, bytes) in &before {
        assert!(yml.contains(&format!("sha256sum: {}", sha(bytes))), "missing sha for {name}\n{yml}");
    }
    assert!(!yml.contains("sha256sum: 0000"));
    assert!(yml.contains("# a manifest's patches") && yml.contains("module: zephyr"));
    // Re-export reuses the tree.
    let (ok, v) = run(&["export", "--series", &f.series.to_string_lossy(), "--repo", &f.module.to_string_lossy(),
        "--base", &f.base, "--scratch", &f.scratch.to_string_lossy()]);
    assert!(ok && v["reused"] == true, "{v}");
}

#[test]
fn an_amended_patch_rewrites_only_its_file() {
    let f = fixture("amend");
    export(&f);
    let before = files(&f);
    // Amend the second commit's message via rebase -i with exec-free reword.
    git(&f.scratch, &["checkout", "-q", "HEAD~1"]);
    git(&f.scratch, &["commit", "-q", "--amend", "-m", "second: reworded"]);
    let c = git(&f.scratch, &["rev-parse", "HEAD"]).trim().to_string();
    git(&f.scratch, &["cherry-pick", "embarch-patches/series"]);
    git(&f.scratch, &["branch", "-f", "embarch-patches/series", "HEAD"]);
    git(&f.scratch, &["checkout", "-q", "embarch-patches/series"]);
    let _ = c;
    let v = import(&f);
    assert_eq!(v["changed"].as_array().unwrap().len(), 1, "only the reworded patch changes: {v}");
    let after = files(&f);
    assert_eq!(after[0], before[0]);
    assert!(String::from_utf8_lossy(&after[1].1).contains("second: reworded"));
}

#[test]
fn a_fix_commit_folds_into_both_patches_it_touches() {
    let f = fixture("fold");
    export(&f);
    let before = files(&f);
    let a = std::fs::read_to_string(f.scratch.join("a.txt")).unwrap();
    let fixed = a.replace("line 2 patched\n", "line 2 fixed\n").replace("line 15 patched\n", "line 15 fixed\n");
    std::fs::write(f.scratch.join("a.txt"), &fixed).unwrap();
    git(&f.scratch, &["commit", "-q", "-am", "Fix: correct patches 0001 and 0003"]);
    let fix_tree = git(&f.scratch, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let v = import(&f);
    let changed: Vec<String> = v["changed"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    assert_eq!(changed.len(), 2, "{v}");
    assert!(changed[0].contains("0001") && changed[1].contains("0003"), "{changed:?}");
    let after = files(&f);
    assert_eq!(after[1], before[1], "the untouched patch stays byte-identical");
    assert!(String::from_utf8_lossy(&after[0].1).contains("line 2 fixed"));
    assert!(String::from_utf8_lossy(&after[2].1).contains("line 15 fixed"));
    // The branch is still base + exactly 3 commits and the tree is the fix's.
    assert_eq!(git(&f.scratch, &["rev-list", "--count", &format!("{}..HEAD", f.base)]).trim(), "3");
    assert_eq!(git(&f.scratch, &["rev-parse", "HEAD^{tree}"]).trim(), fix_tree);
    // Applying the rewritten series to a clean base gives the same tree.
    let second = f.root.join("scratch2");
    let (ok, v) = run(&["export", "--series", &f.series.to_string_lossy(), "--repo", &f.module.to_string_lossy(),
        "--base", &f.base, "--scratch", &second.to_string_lossy()]);
    assert!(ok, "{v}");
    assert_eq!(git(&second, &["rev-parse", "HEAD^{tree}"]).trim(), fix_tree);
    // sha256s follow the rewritten files.
    let yml = std::fs::read_to_string(&f.series).unwrap();
    for (_, b) in &after {
        assert!(yml.contains(&sha(b)));
    }
}

#[test]
fn regroup_proves_tree_identity_renumbers_and_rewrites_references() {
    let f = fixture("regroup");
    export(&f);
    import(&f);
    let tree = git(&f.scratch, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let names: Vec<String> = files(&f).into_iter().map(|(n, _)| n).collect();
    let map = f.root.join("map.txt");
    std::fs::write(&map, "# topic regroup\nlocal/0001-a-and-b.patch: 0001, 0002 | Combined: a and b\nlocal/0002-third.patch: 0003\n").unwrap();
    let (ok, v) = run(&["regroup", "--scratch", &f.scratch.to_string_lossy(), "--map", &map.to_string_lossy()]);
    assert!(ok, "{v}");
    assert_eq!(v["tree_identical"], true);
    assert_eq!(v["tree"], tree.as_str());
    assert_eq!(git(&f.scratch, &["rev-parse", "HEAD^{tree}"]).trim(), tree);
    assert_eq!(git(&f.scratch, &["rev-list", "--count", &format!("{}..HEAD", f.base)]).trim(), "2");
    let dir = f.root.join("manifest");
    for n in &names {
        assert!(!dir.join("patches").join(n).exists(), "{n} should be gone");
    }
    // Entry paths are relative to patches.yml's directory.
    let third = std::fs::read_to_string(dir.join("local/0002-third.patch")).unwrap();
    assert!(third.contains("Builds on patch 0001"), "{third}");
    assert!(third.contains("see patch 0009"), "unmappable reference left as is");
    assert!(std::fs::read_to_string(dir.join("local/0001-a-and-b.patch")).unwrap().contains("Subject: [PATCH] Combined: a and b"));
    let yml = std::fs::read_to_string(&f.series).unwrap();
    assert!(yml.contains("- path: local/0001-a-and-b.patch") && yml.contains("- path: local/0002-third.patch"));
    assert!(!yml.contains("sha256sum: 0000") && yml.contains("module: zephyr"));
    assert_eq!(yml.matches("- path:").count(), 2);
    assert_eq!(v["references_rewritten"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["references_unresolved"][0]["number"], "0009", "{v}");
    // The regrouped series applies to a clean base to the same tree.
    let second = f.root.join("scratch2");
    let (ok, v) = run(&["export", "--series", &f.series.to_string_lossy(), "--repo", &f.module.to_string_lossy(),
        "--base", &f.base, "--scratch", &second.to_string_lossy()]);
    assert!(ok, "{v}");
    assert_eq!(git(&second, &["rev-parse", "HEAD^{tree}"]).trim(), tree);
}

#[test]
fn regroup_refuses_a_map_that_drops_a_patch() {
    let f = fixture("regroup-bad");
    export(&f);
    import(&f);
    let map = f.root.join("map.txt");
    std::fs::write(&map, "x/0001.patch: 0001, 0002\n").unwrap();
    let (ok, v) = run(&["regroup", "--scratch", &f.scratch.to_string_lossy(), "--map", &map.to_string_lossy()]);
    assert!(!ok && v["error"].as_str().unwrap().contains("leaves out"), "{v}");
}
