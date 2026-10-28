//! `embarch-api stack` against throwaway git repos (`tasks/api/130`, decision
//! 96): a fixture with several commits for `plan`, `fold`'s backups, tree
//! identity and conflict path, and a 4-branch stack pushed to a local bare
//! remote for `restack`. Nothing here touches a real repository.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("embarch-stack-{tag}-{}", std::process::id()));
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

fn try_git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = try_git(dir, args);
    assert!(ok, "git {args:?}: {out}");
    out.trim().to_string()
}

fn lines(tag: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{tag} line {i}\n")).collect()
}

fn commit_file(dir: &Path, file: &str, tag: &str) -> String {
    std::fs::write(dir.join(file), lines(tag, 10)).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", &format!("add {file}")]);
    git(dir, &["rev-parse", "HEAD"])
}

fn replace_line(dir: &Path, file: &str, n: usize, with: &str) {
    let text = std::fs::read_to_string(dir.join(file)).unwrap();
    let out: String =
        text.lines().enumerate().map(|(i, l)| if i + 1 == n { format!("{with}\n") } else { format!("{l}\n") }).collect();
    std::fs::write(dir.join(file), out).unwrap();
}

fn stack(dir: &Path, args: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .current_dir(dir)
        .arg("--json")
        .arg("stack")
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.success(), v)
}

/// A repo on `feat` with base.txt on `main` and three commits above it.
fn three_commits(tag: &str) -> (Tmp, PathBuf, Vec<String>) {
    let tmp = Tmp::new(tag);
    let repo = tmp.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    commit_file(&repo, "base.txt", "base");
    git(&repo, &["switch", "-q", "-c", "feat"]);
    let shas = ["f1.txt", "f2.txt", "f3.txt"].iter().map(|f| commit_file(&repo, f, f)).collect();
    (tmp, repo, shas)
}

#[test]
fn plan_places_hunks_and_lists_what_it_cannot() {
    let (_t, repo, shas) = three_commits("plan");
    replace_line(&repo, "f1.txt", 3, "edited by review");
    replace_line(&repo, "f2.txt", 5, "edited by review");
    replace_line(&repo, "base.txt", 2, "older than the range");
    std::fs::write(repo.join("new.txt"), "brand new\n").unwrap();
    let (ok, v) = stack(&repo, &["plan", "main"]);
    assert!(ok, "{v}");
    let placed = v["placed"].as_array().unwrap();
    assert_eq!(placed.len(), 2, "{v}");
    assert_eq!(placed[0]["target"], shas[0]);
    assert_eq!(placed[1]["target"], shas[1]);
    let unplaced: Vec<String> = v["unplaced"].as_array().unwrap().iter().map(|u| u["file"].as_str().unwrap().to_string()).collect();
    assert!(unplaced.contains(&"base.txt".to_string()) && unplaced.contains(&"new.txt".to_string()), "{v}");
    let plan = std::fs::read_to_string(v["plan_file"].as_str().unwrap()).unwrap();
    assert!(plan.contains("base.txt") && plan.contains("new.txt") && plan.contains("older than the range"), "{plan}");
    // Two fixups on top, the leftover still in the working tree.
    let log = git(&repo, &["log", "--format=%s", "main..HEAD"]);
    assert_eq!(log.lines().filter(|l| l.starts_with("fixup! ")).count(), 2, "{log}");
    let left = git(&repo, &["status", "--porcelain"]);
    assert!(left.contains("base.txt") && left.contains("new.txt") && !left.contains("f1.txt"), "{left}");
    assert!(v["backup"].as_str().unwrap().starts_with("backup/feat-"));
}

#[test]
fn plan_since_splits_review_commits_and_dry_run_moves_nothing() {
    let (_t, repo, shas) = three_commits("since");
    let reviewed = git(&repo, &["rev-parse", "HEAD"]);
    replace_line(&repo, "f3.txt", 1, "fix one");
    git(&repo, &["commit", "-q", "-am", "review fix"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let (ok, v) = stack(&repo, &["plan", "main", "--since", &reviewed, "--dry-run"]);
    assert!(ok, "{v}");
    assert_eq!(v["placed"][0]["target"], shas[2]);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    let (ok, v) = stack(&repo, &["plan", "main", "--since", &reviewed]);
    assert!(ok, "{v}");
    assert_eq!(git(&repo, &["log", "--format=%s", "main..HEAD"]).lines().count(), 4);
    assert!(git(&repo, &["log", "--format=%s", "-1"]).starts_with("fixup! "));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn fold_backs_up_squashes_applies_curation_and_proves_the_tree() {
    let (_t, repo, shas) = three_commits("fold");
    replace_line(&repo, "f1.txt", 3, "edited by review");
    replace_line(&repo, "f3.txt", 4, "edited by review");
    let (ok, v) = stack(&repo, &["plan", "main"]);
    assert!(ok, "{v}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let curation = repo.parent().unwrap().join("curation.txt");
    std::fs::write(&curation, format!("# comment\n{} | add the second file, reworded\n", &shas[1][..10])).unwrap();
    let (ok, v) = stack(&repo, &["fold", "main", "--curation", &curation.to_string_lossy()]);
    assert!(ok, "{v}");
    assert_eq!(v["pushed"], false);
    assert_eq!(git(&repo, &["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(v["tree"], tree.as_str());
    let log = git(&repo, &["log", "--format=%s", "--reverse", "main..HEAD"]);
    assert_eq!(log, "add f1.txt\nadd the second file, reworded\nadd f3.txt");
    // The backup is the pre-fold tip.
    let backup = v["backups"][0]["ref"].as_str().unwrap();
    assert_eq!(git(&repo, &["rev-parse", backup]), before);
}

#[test]
fn fold_refuses_a_dirty_tree() {
    let (_t, repo, _s) = three_commits("dirty");
    replace_line(&repo, "f1.txt", 1, "dirty");
    let (ok, v) = stack(&repo, &["fold", "main"]);
    assert!(!ok);
    assert!(v["error"].as_str().unwrap().contains("dirty"), "{v}");
}

#[test]
fn fold_conflict_restores_the_branch_and_names_the_commit() {
    let (_t, repo, shas) = three_commits("conflict");
    // c4 rewrites f1.txt (owned by c1) after c2 already changed nothing there; make
    // c2' conflict: change f1.txt line 1 in a commit above, then a fixup of c1 touching line 1.
    replace_line(&repo, "f1.txt", 1, "middle edit");
    git(&repo, &["commit", "-q", "-am", "middle edit"]);
    replace_line(&repo, "f1.txt", 1, "fixup edit");
    git(&repo, &["commit", "-q", "-am", &format!("fixup! {}", shas[0])]);
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let (ok, v) = stack(&repo, &["fold", "main"]);
    assert!(!ok, "{v}");
    let msg = v["error"].as_str().unwrap();
    assert!(msg.contains("f1.txt") && msg.contains("fixup!") && msg.contains("backup/feat-"), "{msg}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert!(!repo.join(".git/rebase-merge").exists());
}

/// bare remote + a clone with main and b0..b3 (one file each), all pushed.
struct Stack {
    _t: Tmp,
    root: PathBuf,
    remote: PathBuf,
    repo: PathBuf,
}

fn four_branch_stack(tag: &str) -> Stack {
    let t = Tmp::new(tag);
    let root = t.0.clone();
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    commit_file(&repo, "base.txt", "base");
    git(&repo, &["push", "-q", "origin", "main"]);
    for i in 0..4 {
        git(&repo, &["switch", "-q", "-c", &format!("b{i}")]);
        commit_file(&repo, &format!("f{i}.txt"), &format!("f{i}"));
        git(&repo, &["push", "-q", "-u", "origin", &format!("b{i}")]);
    }
    Stack { _t: t, root, remote, repo }
}

fn fold_bottom(s: &Stack) {
    git(&s.repo, &["switch", "-q", "b0"]);
    replace_line(&s.repo, "f0.txt", 2, "review fix");
    let (ok, v) = stack(&s.repo, &["plan", "main"]);
    assert!(ok, "{v}");
    let (ok, v) = stack(&s.repo, &["fold", "main"]);
    assert!(ok, "{v}");
}

fn remote_sha(s: &Stack, b: &str) -> String {
    git(&s.remote, &["rev-parse", &format!("refs/heads/{b}")])
}

#[test]
fn restack_updates_every_ref_and_pushes_atomically() {
    let s = four_branch_stack("restack");
    fold_bottom(&s);
    let (ok, v) = stack(&s.repo, &["restack", "b0..b3"]);
    assert!(ok, "{v}");
    assert_eq!(v["pushed"], true);
    for w in [("b0", "b1"), ("b1", "b2"), ("b2", "b3")] {
        let (ok, _) = try_git(&s.repo, &["merge-base", "--is-ancestor", w.0, w.1]);
        assert!(ok, "{} is not under {}", w.1, w.0);
    }
    for b in ["b0", "b1", "b2", "b3"] {
        assert_eq!(remote_sha(&s, b), git(&s.repo, &["rev-parse", b]), "{b}");
    }
    assert_eq!(git(&s.repo, &["rev-list", "--count", "main..b3"]), "4");
    assert_eq!(git(&s.repo, &["symbolic-ref", "--short", "HEAD"]), "b0");
    // The review fix is in b0's commit and survived to the top.
    assert!(git(&s.repo, &["show", "b3:f0.txt"]).contains("review fix"));
}

#[test]
fn restack_aborts_the_whole_push_when_a_remote_ref_moved() {
    let s = four_branch_stack("lease");
    fold_bottom(&s);
    let before: Vec<String> = ["b0", "b1", "b2", "b3"].iter().map(|b| remote_sha(&s, b)).collect();
    // Someone else pushes to b2.
    let other = s.root.join("other");
    git(&s.root, &["clone", "-q", &s.remote.to_string_lossy(), "other"]);
    git(&other, &["switch", "-q", "b2"]);
    commit_file(&other, "theirs.txt", "theirs");
    git(&other, &["push", "-q", "origin", "b2"]);
    let moved = remote_sha(&s, "b2");
    let (ok, v) = stack(&s.repo, &["restack", "b0..b3"]);
    assert!(!ok, "{v}");
    assert!(v["error"].as_str().unwrap().contains("nothing was sent"), "{v}");
    assert_eq!(remote_sha(&s, "b0"), before[0]);
    assert_eq!(remote_sha(&s, "b1"), before[1]);
    assert_eq!(remote_sha(&s, "b2"), moved);
    assert_eq!(remote_sha(&s, "b3"), before[3]);
}

#[test]
fn restack_onto_drops_a_rebase_merged_bottom_by_patch_id() {
    let s = four_branch_stack("landed");
    // The bottom PR lands by rebase merge: its commit is replayed on main.
    let other = s.root.join("other");
    git(&s.root, &["clone", "-q", &s.remote.to_string_lossy(), "other"]);
    git(&other, &["fetch", "-q", "origin", "b0"]);
    // Move main on first so the replayed copy has a different sha.
    commit_file(&other, "unrelated.txt", "unrelated");
    git(&other, &["cherry-pick", "origin/b0"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(&s.repo, &["fetch", "-q", "origin"]);
    let (ok, v) = stack(&s.repo, &["restack", "b0..b3", "--onto", "origin/main"]);
    assert!(ok, "{v}");
    assert_eq!(v["landed"], serde_json::json!(["b0"]));
    // Three commits left on top of main; b0's was dropped, not replayed twice.
    assert_eq!(git(&s.repo, &["rev-list", "--count", "origin/main..b3"]), "3");
    let (ok, _) = try_git(&s.repo, &["merge-base", "--is-ancestor", "origin/main", "b1"]);
    assert!(ok);
    for b in ["b1", "b2", "b3"] {
        assert_eq!(remote_sha(&s, b), git(&s.repo, &["rev-parse", b]), "{b}");
    }
    // The landed branch is not pushed.
    assert_ne!(remote_sha(&s, "b0"), git(&s.repo, &["rev-parse", "origin/main"]));
}
