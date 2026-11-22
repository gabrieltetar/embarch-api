//! What this crate tells Core about the build a study runs against
//! (decision 111): `build_commit`, `build_dirty` and `build_toolchain`.
//!
//! Core records them under the run record's `claimed.build`, never as
//! something it observed. Each value is sent only when it was actually read;
//! a checkout that is not a git work tree, a git that fails, or a toolchain
//! that cannot be asked yields nothing for that field, never a placeholder.

use std::path::Path;
use std::process::Command;

use embarch_core_client::BuildClaims;
use embarch_firmware_build::config::ProjectConfig;

/// Core's own limit on `build_toolchain` (`embarch-core` decision 110).
const MAX_TOOLCHAIN_BYTES: usize = 200;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit and dirty state of the checkout at `repo`. Both `None` when it
/// is not a git work tree. Dirty means tracked files differ from `HEAD`
/// (untracked files do not count, the same reading `build.rs` stamps this
/// binary with).
pub fn checkout_claims(repo: &Path) -> BuildClaims {
    let commit = git(repo, &["rev-parse", "HEAD"]).filter(|c| (7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()));
    let Some(commit) = commit else { return BuildClaims::default() };
    let dirty = git(repo, &["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty());
    BuildClaims { commit: Some(commit.to_ascii_lowercase()), dirty, toolchain: None }
}

/// The first line `west --version` prints, for a `zephyr-west` project: the
/// build tool that produced the firmware, which is what this crate can read
/// without running a build. `None` for a `static` project, whose build
/// command is an arbitrary program this crate will not run again just to ask.
pub fn toolchain(project: &ProjectConfig) -> Option<String> {
    let west = project.west_binary.as_ref().filter(|_| project.is_zephyr_west())?;
    let out = Command::new(west).arg("--version").stdin(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty() && line.len() <= MAX_TOOLCHAIN_BYTES && !line.chars().any(char::is_control)).then(|| line.to_string())
}

/// Everything known about `project`'s build: its checkout's commit and dirty
/// state, and its toolchain.
pub fn for_project(project: &ProjectConfig) -> BuildClaims {
    BuildClaims { toolchain: toolchain(project), ..checkout_claims(&project.source_path) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let s = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(s.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&s.stderr));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("build-claims-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_checkout_sends_its_commit_and_its_dirty_state() {
        let d = scratch("git");
        run(&d, &["init", "-q"]);
        std::fs::write(d.join("a"), "1").unwrap();
        run(&d, &["add", "a"]);
        run(&d, &["commit", "-q", "-m", "x"]);
        let clean = checkout_claims(&d);
        assert_eq!(clean.commit.as_deref().map(str::len), Some(40));
        assert_eq!(clean.dirty, Some(false));
        std::fs::write(d.join("a"), "2").unwrap();
        assert_eq!(checkout_claims(&d).dirty, Some(true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_directory_with_no_git_checkout_sends_nothing() {
        let d = scratch("nogit");
        assert_eq!(checkout_claims(&d), BuildClaims::default());
        assert_eq!(checkout_claims(&d.join("missing")), BuildClaims::default());
        let _ = std::fs::remove_dir_all(&d);
    }
}
