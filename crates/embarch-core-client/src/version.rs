//! Deriving the version of a firmware working tree — one implementation,
//! two call sites.
//!
//! Lived in `embarch-api`'s own `reflash.rs` until 2026-08-26, and moved here
//! when `embarch-ui`'s Study Designer needed the same answer for a different
//! reason: `embarch-ui/design.md` §3 decision 11 prefills a `Study`'s
//! `requires.firmware_version` from "the configured project's own
//! `git describe`", and `embarch-ui` cannot depend on `embarch-api` (no such
//! dependency direction exists in this suite). The alternative was a second
//! copy of the argv and the safety rule below, in a crate that would never
//! see the first one's tests — the same "one implementation, multiple call
//! sites" case `embarch-topology`'s own extraction was made on.
//!
//! This crate is otherwise "how do I reach embarch-core over HTTP+Bearer",
//! and this is not that. It is here for the same reason [`crate::api_log`]
//! is: this crate is where `embarch-api` and `embarch-ui` already meet, and a
//! fact both need has nowhere else to live that both can see.
//!
//! # The load-bearing constraint, moved with the code
//!
//! **Nothing here ever runs `git checkout`.** Manipulating an engineer's
//! working tree to satisfy a test harness is destructive to the thing they
//! are actively editing. Deriving a version is a *read*, and
//! [`reject_tree_mutating_command`] is what keeps it one even when the argv
//! arrives from a human-written config file this crate does not get to trust.

use anyhow::{Context, Result};
use std::path::Path;

/// The default version command: the same `git describe` invocation
/// `embarch-dev-bench`'s own build embeds into `HelloAck.firmware_version`
/// and `embarch-umbrella`'s doctor check 13 compares against, so a project
/// that declares nothing gets the convention the rest of the suite already
/// uses.
pub const DEFAULT_VERSION_COMMAND: &[&str] =
    &["git", "describe", "--always", "--dirty", "--abbrev=8"];

/// [`DEFAULT_VERSION_COMMAND`] as owned `String`s, for the callers that hold
/// an `Option<Vec<String>>` of config and want the default when it is absent.
pub fn default_version_command() -> Vec<String> {
    DEFAULT_VERSION_COMMAND.iter().map(|s| s.to_string()).collect()
}

/// Every argv this crate is ever allowed to run against a firmware repo for
/// versioning purposes is a *read*. These are the `git` subcommands that
/// mutate a working tree, and [`reject_tree_mutating_command`] refuses any of
/// them outright.
const TREE_MUTATING_GIT_SUBCOMMANDS: &[&str] = &[
    "checkout", "switch", "reset", "restore", "clean", "stash", "merge", "rebase", "cherry-pick",
    "apply", "am", "pull", "revert", "worktree", "submodule", "sparse-checkout",
];

/// Refuses a version command that would move the working tree.
///
/// Deliberately matches **any** argument, not just the one in subcommand
/// position. `git -C /elsewhere checkout main` puts a path where the
/// subcommand looks like it should be, and a rule that only inspected the
/// first non-flag argument would wave it through — so this over-rejects
/// instead, refusing a read-only `git log merge` along with the real thing.
/// That trade is one-sided: a version command has no business naming any of
/// these, and the cost of the false positive is renaming an argument, while
/// the cost of the false negative is somebody's uncommitted work.
pub fn reject_tree_mutating_command(command: &[String]) -> Result<()> {
    let Some((program, args)) = command.split_first() else {
        anyhow::bail!("version_command is empty — give it at least a program to run");
    };
    let program_name = Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program.as_str());
    if program_name != "git" {
        return Ok(());
    }
    if let Some(subcommand) = args
        .iter()
        .find(|a| TREE_MUTATING_GIT_SUBCOMMANDS.contains(&a.as_str()))
    {
        anyhow::bail!(
            "refusing to run `git ... {subcommand} ...`: EmbArch never moves an engineer's \
             working tree to satisfy a study's version requirement \
             (`embarch-api/design.md` §3 decision 40). A reflash builds the tree as it stands; if \
             the study wants another revision, that checkout is yours to make."
        );
    }
    Ok(())
}

/// Runs `command` in `cwd` and returns its trimmed stdout as a version string.
///
/// A blank result is an error rather than an empty version: a blank string
/// recorded as a firmware version is an unreadable fact presented as a fact,
/// which is the same defect `VersionSource::Declared` exists to make visible.
pub async fn derive_version(cwd: &Path, command: &[String]) -> Result<String> {
    reject_tree_mutating_command(command)?;
    let (program, args) = command
        .split_first()
        .context("version_command must have at least one element")?;

    if !cwd.exists() {
        anyhow::bail!("cannot derive a firmware version: {} does not exist", cwd.display());
    }

    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| {
            format!("failed to run version_command ({}) in {}", command.join(" "), cwd.display())
        })?;

    if !output.status.success() {
        anyhow::bail!(
            "version_command ({}) failed in {}: {}",
            command.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        anyhow::bail!(
            "version_command ({}) produced no output in {} — a blank firmware version would be \
             recorded as a fact nobody can read",
            command.join(" "),
            cwd.display()
        );
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_default_command_is_a_read() {
        assert!(reject_tree_mutating_command(&default_version_command()).is_ok());
    }

    /// The `git -C` case is why the rule scans every argument rather than the
    /// subcommand position — pinned here rather than left to the reasoning in
    /// the doc comment.
    #[test]
    fn a_tree_mutating_subcommand_is_refused_wherever_it_sits() {
        for command in [
            argv(&["git", "checkout", "main"]),
            argv(&["git", "-C", "/elsewhere", "checkout", "main"]),
            argv(&["/usr/bin/git", "stash"]),
        ] {
            let err = reject_tree_mutating_command(&command).unwrap_err().to_string();
            assert!(err.contains("refusing to run"), "{command:?} was waved through: {err}");
        }
    }

    /// Not a git command at all: this rule is about `git`'s tree, and a
    /// project that versions itself some other way is not this rule's
    /// business.
    #[test]
    fn a_non_git_program_is_not_second_guessed() {
        assert!(reject_tree_mutating_command(&argv(&["cat", "VERSION"])).is_ok());
        assert!(reject_tree_mutating_command(&argv(&["./scripts/version.sh", "clean"])).is_ok());
    }

    #[test]
    fn an_empty_command_is_refused_rather_than_run() {
        assert!(reject_tree_mutating_command(&[]).is_err());
    }

    #[tokio::test]
    async fn a_command_that_prints_nothing_is_an_error_not_a_blank_version() {
        let dir = tempfile::tempdir().unwrap();
        let err = derive_version(dir.path(), &argv(&["true"])).await.unwrap_err().to_string();
        assert!(err.contains("no output"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_directory_says_so_rather_than_failing_inside_the_spawn() {
        let err = derive_version(Path::new("/nonexistent-embarch-repo"), &argv(&["true"]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }
}
