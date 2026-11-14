//! A server whose working directory is a git worktree of the clone a
//! project's `source_path` points into builds that worktree, or refuses
//! (decision 106, EMB-181).
//!
//! A worktree session finds the main clone's linked `embarch/embarch.toml`,
//! whose `source_path` and `build_dir_root` are absolute paths into the main
//! clone; the working directory only finds the config. This runs once at
//! startup, after the config loads and before anything resolves a plan, and
//! rewrites those paths — so `embarch-firmware-build` is untouched and its
//! public API (which `embarch-ui` also uses) does not change.
//!
//! Detection is read-only `git` (`rev-parse`), never a mutating subcommand.

use anyhow::{bail, Result};
use embarch_firmware_build::config::{Config, ProjectConfig};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A working tree and the repository's shared git directory, both canonical.
/// Two trees are one clone's when their `common` agree.
#[derive(Debug, PartialEq)]
struct Tree {
    top: PathBuf,
    common: PathBuf,
}

fn tree_of(dir: &Path) -> Option<Tree> {
    let out = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let top = std::fs::canonicalize(lines.next()?).ok()?;
    let common = std::fs::canonicalize(lines.next()?).ok()?;
    Some(Tree { top, common })
}

/// FNV-1a, by hand so the directory name does not depend on the compiler
/// (the same reason `BuildLocks` does it, decision 91).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

/// Where a worktree's builds go: a directory of its own under the
/// configured `build_dir_root`, named for the worktree and keyed by its
/// absolute path, so it is never the main clone's directory nor another
/// worktree's.
fn worktree_build_root(root: &Path, worktree: &Path) -> PathBuf {
    let name = worktree.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    root.join(format!("worktree-{name}-{:08x}", fnv1a(&worktree.to_string_lossy()) as u32))
}

/// Rebase one project onto the worktree `cwd` is in, if it is a worktree of
/// the clone the project's `source_path` is in. `Ok(true)` when it moved.
fn rebase_project(project: &mut ProjectConfig, cwd: &Tree) -> Result<bool> {
    let Some(source) = tree_of(&project.source_path) else {
        return Ok(false);
    };
    if source.common != cwd.common || source.top == cwd.top {
        return Ok(false);
    }
    let canonical = std::fs::canonicalize(&project.source_path)?;
    let relative = canonical.strip_prefix(&source.top)?.to_path_buf();
    let moved = cwd.top.join(&relative);
    if !moved.is_dir() {
        bail!(
            "project '{}': this server's working directory is the worktree {}, but the config's \
             source_path is {} in the main clone {}, and the worktree has no {} — refusing to \
             build the other tree while you edit this one. Check out a commit that has it, or \
             start the server from the main clone.",
            project.name,
            cwd.top.display(),
            project.source_path.display(),
            source.top.display(),
            relative.display(),
        );
    }
    if let Some(cwd_rel) = project.build_cwd.as_ref().filter(|c| c.is_absolute()) {
        if let Ok(inside) = cwd_rel.strip_prefix(&source.top) {
            project.build_cwd = Some(cwd.top.join(inside));
        }
    }
    project.source_path = moved;
    if let Some(root) = project.build_dir_root.as_mut() {
        *root = worktree_build_root(root, &cwd.top);
    }
    Ok(true)
}

/// Apply the rule to every project. Returns one note per project moved, for
/// the startup log.
pub fn rebase_onto_worktree(config: &mut Config, cwd: &Path) -> Result<Vec<String>> {
    let Some(here) = tree_of(cwd) else {
        return Ok(Vec::new());
    };
    let mut notes = Vec::new();
    for project in &mut config.projects {
        let before = project.source_path.clone();
        if rebase_project(project, &here)? {
            notes.push(format!(
                "project '{}': building the worktree {} (source_path {} -> {})",
                project.name,
                here.top.display(),
                before.display(),
                project.source_path.display(),
            ));
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{resolve_build_with, BuildChoice, Selection};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "embarch-api-worktree-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(std::fs::canonicalize(dir).unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repo with one Zephyr board and one app, committed.
    fn repo(root: &Path) {
        let board = root.join("boards/acme/single");
        std::fs::create_dir_all(&board).unwrap();
        std::fs::write(board.join("single.yml"), "board:\n  name: single\n  socs:\n    - name: nrf54l15\n").unwrap();
        std::fs::write(board.join("single_nrf54l15.dts"), "").unwrap();
        std::fs::create_dir_all(root.join("app/widget")).unwrap();
        std::fs::write(root.join("app/widget/CMakeLists.txt"), "").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
    }

    fn project(source: &Path, extra: &str) -> ProjectConfig {
        toml::from_str(&format!(
            "name = \"p\"\nsource_path = \"{}\"\ndiscovery = \"zephyr-west\"\nflash_format = \"hex\"\n\
             west_binary = \"/venv/bin/west\"\nbuild_dir_root = \"/builds/main\"\n{extra}\n",
            source.display()
        ))
        .unwrap()
    }

    fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag).expect("flag present");
        &argv[i + 1]
    }

    /// Decision 106 (replaces `tasks/api/154`'s test, which pinned the old
    /// behaviour): from a worktree of the clone `source_path` is in, a build
    /// runs in the worktree, passes the worktree's app path, and writes to a
    /// directory of its own; from the main clone, nothing moves.
    #[test]
    fn a_zephyr_build_from_a_worktree_runs_in_the_worktree_not_the_main_clone() {
        let scratch = Scratch::new();
        let main = scratch.0.join("main");
        std::fs::create_dir_all(&main).unwrap();
        repo(&main);
        let wt = scratch.0.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feature"]);

        let mut config = Config {
            core: toml::from_str("base_url = \"http://127.0.0.1:1\"").unwrap(),
            projects: vec![project(&main, "env = { FOO = \"bar\" }")],
            dev_bench: None,
        };

        // Main clone: unchanged, builds the main clone into the configured root.
        assert!(rebase_onto_worktree(&mut config, &main).unwrap().is_empty());
        let plan = resolve_build_with(&config.projects[0], Selection::default(), BuildChoice::default()).unwrap().plan;
        assert_eq!(plan.cwd, main);
        assert!(Path::new(arg_after(&plan.command, "-d")).starts_with("/builds/main"));
        assert_eq!(Path::new(plan.command.last().unwrap()), main.join("app/widget"));
        let main_dir = arg_after(&plan.command, "-d").to_string();

        // Worktree (and a subdirectory of it): the worktree, in a build root of its own.
        std::fs::create_dir_all(wt.join("app")).unwrap();
        let notes = rebase_onto_worktree(&mut config, &wt.join("app")).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let plan = resolve_build_with(&config.projects[0], Selection::default(), BuildChoice::default()).unwrap().plan;
        assert_eq!(plan.cwd, wt);
        assert_eq!(Path::new(plan.command.last().unwrap()), wt.join("app/widget"));
        let wt_dir = arg_after(&plan.command, "-d");
        let root = Path::new(wt_dir).parent().unwrap();
        assert!(root.starts_with("/builds/main"), "{wt_dir}");
        assert_ne!(wt_dir, main_dir, "a worktree build never shares the main clone's directory");
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("worktree-wt-"), "{wt_dir}");
        assert_eq!(plan.env["FOO"], "bar", "the project's env table still rides the plan");
    }

    #[test]
    fn a_worktree_lacking_the_source_path_is_refused_naming_both_trees() {
        let scratch = Scratch::new();
        let main = scratch.0.join("main");
        std::fs::create_dir_all(&main).unwrap();
        repo(&main);
        let wt = scratch.0.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feature"]);
        std::fs::create_dir_all(main.join("deep/fw")).unwrap();

        let mut config = Config {
            core: toml::from_str("base_url = \"http://127.0.0.1:1\"").unwrap(),
            projects: vec![project(&main.join("deep/fw"), "")],
            dev_bench: None,
        };
        let err = format!("{:#}", rebase_onto_worktree(&mut config, &wt).unwrap_err());
        assert!(err.contains(wt.to_str().unwrap()) && err.contains(main.to_str().unwrap()), "{err}");
    }

    #[test]
    fn a_different_repository_or_no_repository_moves_nothing() {
        let scratch = Scratch::new();
        let main = scratch.0.join("main");
        let other = scratch.0.join("other");
        for d in [&main, &other] {
            std::fs::create_dir_all(d).unwrap();
            repo(d);
        }
        let plain = scratch.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut config = Config {
            core: toml::from_str("base_url = \"http://127.0.0.1:1\"").unwrap(),
            projects: vec![project(&main, "")],
            dev_bench: None,
        };
        assert!(rebase_onto_worktree(&mut config, &other).unwrap().is_empty());
        assert!(rebase_onto_worktree(&mut config, &plain).unwrap().is_empty());
        assert_eq!(config.projects[0].source_path, main);
    }
}
