//! `run_study`'s reflash selector and the version derivation behind it —
//! `design.md` §3 decision 40, the orchestrating half of
//! `embarch-study-designer/design.md` §3 decision 40.
//!
//! Sequencing is **check → build → flash → `POST /study`**, all here,
//! because Core has no build system (`embarch-core/design.md` §3 decision
//! 31). Core still gates independently on submit, so nothing in this module
//! is the enforcement point: a `Study` posted straight to Core with a stale
//! bench is rejected whether or not anything here ran. What this adds is the
//! *choice* about what to do when a version does not line up.
//!
//! # The load-bearing constraint
//!
//! **This crate never runs `git checkout`.** "Reflash" means: build and
//! flash the configured project **as the working tree currently stands**,
//! then verify what that produced against what the `Study` requires — and
//! fail, naming both revisions, when it does not match. It does not mean
//! "make my tree be that version". Manipulating an engineer's working tree
//! to satisfy a test harness is destructive to the thing they are actively
//! editing, and the failure message saying which revision the study wants is
//! where the decision to move there stays theirs.
//!
//! Nothing in this file spawns `git` for anything but a **read**
//! ([`derive_version`], which runs a project-declared command whose default
//! is `git describe`), and [`tests::the_reflash_path_never_moves_the_tree`]
//! is what keeps it that way.

use anyhow::{Context, Result};

/// The suite's default version command, the read that runs it, and the rule
/// that keeps it a read — all three now live in `embarch-core-client`
/// (`version.rs`), re-exported here so every call site and doc reference in
/// this crate keeps its existing path.
///
/// **Moved 2026-08-26, and the move is the point.** `embarch-ui`'s Study
/// Designer prefills a `Study`'s `requires.firmware_version` from the
/// configured project's own `git describe` (`embarch-ui/design.md` §3
/// decision 11), and `embarch-ui` cannot depend on this crate — no such
/// dependency direction exists in the suite. The alternative was a second copy
/// of this argv and of [`reject_tree_mutating_command`]'s over-rejecting rule,
/// living in a crate that would never see these tests.
/// `embarch-core-client` is where these two crates already meet.
///
/// [`tests::the_reflash_path_never_moves_the_tree`] still lives here and still
/// fails the moment *this* crate's reflash path acquires a way to move an
/// engineer's working tree — the constraint is about this path, wherever the
/// function enforcing it is defined.
pub use embarch_core_client::version::{default_version_command, derive_version};

/// Which firmware a run should rebuild and reflash before submitting its
/// study (`design.md` §3 decision 40).
///
/// `None` is the default and the safe one: flashing is the destructive-ish
/// half, and a study that merely observes a board somebody just flashed by
/// hand must not silently overwrite it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ReflashTarget {
    #[default]
    None,
    DevBench,
    Dut,
    Both,
}

impl ReflashTarget {
    pub fn parse(raw: &str) -> Result<ReflashTarget> {
        match raw {
            "none" => Ok(ReflashTarget::None),
            "dev-bench" => Ok(ReflashTarget::DevBench),
            "dut" => Ok(ReflashTarget::Dut),
            "both" => Ok(ReflashTarget::Both),
            other => anyhow::bail!(
                "unknown reflash target '{other}' — expected one of none, dev-bench, dut, both"
            ),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ReflashTarget::None => "none",
            ReflashTarget::DevBench => "dev-bench",
            ReflashTarget::Dut => "dut",
            ReflashTarget::Both => "both",
        }
    }

    pub const fn includes_dev_bench(self) -> bool {
        matches!(self, ReflashTarget::DevBench | ReflashTarget::Both)
    }

    pub const fn includes_dut(self) -> bool {
        matches!(self, ReflashTarget::Dut | ReflashTarget::Both)
    }
}

/// The version command for a project: whatever it declared, else
/// [`DEFAULT_VERSION_COMMAND`].
pub fn version_command_for(declared: Option<&Vec<String>>) -> Vec<String> {
    match declared {
        Some(cmd) if !cmd.is_empty() => cmd.clone(),
        _ => default_version_command(),
    }
}

/// The message a version mismatch fails with when no override was given.
///
/// Names **both** strings and says which side moved, because the whole
/// content of the failure is the gap between them — and says plainly that
/// moving the tree is the engineer's call, not this process's.
pub fn mismatch_message(what: &str, required: &str, actual: &str, remedy: &str) -> String {
    format!(
        "{what} version mismatch: this study requires '{required}', but this run has '{actual}'. \
         {remedy} embarch-api will not check out a revision to satisfy a study — reflash builds \
         the tree as it stands. Re-run with allow_version_mismatch to proceed anyway; the \
         override is recorded in StudyResult.provenance.overrides, never silently honoured."
    )
}

/// Which configured project a run will reflash as the DUT, if any.
///
/// `run_study` deliberately has no `project` parameter for the ordinary case
/// (`design.md` §5's own note: a study targets whatever DUT is connected
/// through Core's dev-bench link, not one of this crate's configured
/// projects). Rebuilding that DUT's firmware is a different thing and *is*
/// project-shaped, so the parameter appears exactly when it becomes
/// meaningful — and is a clear error when it is needed and missing, rather
/// than a guess at which of several configured projects was meant.
pub fn dut_project(reflash: ReflashTarget, project: Option<&str>) -> Result<Option<&str>> {
    match (reflash.includes_dut(), project) {
        (true, Some(name)) => Ok(Some(name)),
        (true, None) => anyhow::bail!(
            "reflash '{}' needs to know which project is the DUT: pass project. A study itself \
             isn't tied to a configured project, but rebuilding the DUT's firmware is.",
            reflash.as_str()
        ),
        (false, _) => Ok(None),
    }
}

/// What a `run_study` call actually did on the way to submitting, beyond
/// the `study_id` it returns.
#[derive(Debug, Default)]
pub struct RunStudyOutcome {
    pub study_id: String,
    /// One entry per firmware this run rebuilt and reflashed, in the order
    /// it happened.
    pub reflashed: Vec<serde_json::Value>,
    /// The DUT build this run put on the board, when it flashed one. This is
    /// what Core turns into `VersionSource::FlashedThisRun`.
    pub flashed_firmware_version: Option<String>,
    /// The bench's own reported version, when this run had reason to read it.
    pub dev_bench_version: Option<String>,
}

impl RunStudyOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "study_id": self.study_id,
            "reflashed": self.reflashed,
            "flashed_firmware_version": self.flashed_firmware_version,
            "dev_bench_version": self.dev_bench_version,
        })
    }
}

/// Everything a `run_study` call needs beyond the `Study` itself.
pub struct RunStudyRequest<'a> {
    pub reflash: ReflashTarget,
    pub allow_version_mismatch: bool,
    /// Which configured project is the DUT. Required by `--reflash dut|both`
    /// and meaningless otherwise — a study is not project-shaped
    /// (`design.md` §5's own "no `project` param" note on `run_study`), but
    /// *rebuilding the DUT's firmware* is, and there is nowhere else for the
    /// build target to come from.
    pub project: Option<&'a str>,
    pub selection: crate::resolve::Selection<'a>,
}

/// `design.md` §3 decision 40's sequence: **check → build → flash → `POST
/// /study`**, all here because Core has no build system.
///
/// The two halves are sequenced differently, and the difference is decision
/// 40's verification asymmetry showing up as control flow rather than as
/// prose:
///
/// - **dev-bench is flashed and then read back.** Its version is genuinely
///   observable (`GET /dev-bench/hello`), so the verification is a
///   measurement and can only happen after the flash.
/// - **the DUT is verified and then flashed.** Nothing can be read back off
///   it, so the only check available is against the tree about to be built —
///   which means a study asking for a revision this tree isn't at fails
///   *without touching the board at all*, rather than after overwriting it.
///
/// Core gates independently on submit either way, so a mistake here cannot
/// let a stale bench through; what this adds is the choice, and the chance to
/// fail before doing something destructive.
pub async fn run_study(
    config: &crate::config::Config,
    core: &embarch_core_client::CoreClient,
    build_locks: &crate::build::BuildLocks,
    study: &embarch_study_designer::Study,
    request: RunStudyRequest<'_>,
) -> Result<RunStudyOutcome> {
    use embarch_study_designer::{requirement_satisfied, REQUIREMENT_ANY};

    let mut outcome = RunStudyOutcome::default();
    let required_bench = study.requires.dev_bench_version.as_str();
    let required_firmware = study.requires.firmware_version.as_str();

    let dut_project = dut_project(request.reflash, request.project)?;

    // ---- dev-bench: flash, then read back what the bench now says it is.
    if request.reflash.includes_dev_bench() {
        let dev_bench = config
            .dev_bench
            .as_ref()
            .context("[dev_bench] isn't configured, so there's nothing to reflash")?;
        let resolved = crate::dev_bench::resolve(dev_bench)?;
        let built = build_locks.run_build(&resolved.plan).await.context("dev-bench build failed")?;
        if !built.ready_to_flash() {
            anyhow::bail!(
                "dev-bench build did not produce a fresh artifact, so nothing was flashed: {}",
                build_failure_reason(&built)
            );
        }
        let path = built.artifact_path.display().to_string();
        core.flash(
            &resolved.chip,
            &path,
            &resolved.flash_format,
            resolved.base_address.as_deref(),
            resolved.probe_serial.as_deref(),
            false,
        )
        .await
        .context("dev-bench flash failed")?;
        // Flashing halts the core rather than starting it running
        // (`design.md` §3 decision 32's `reset_dev_bench` note), so a bench
        // that is never reset never replies to `Hello` and every check below
        // would time out against a chip sitting halted.
        core.reset(&resolved.chip, resolved.probe_serial.as_deref())
            .await
            .context("dev-bench reset after flash failed")?;
        outcome.reflashed.push(serde_json::json!({
            "target": "dev-bench",
            "artifact_path": path,
            "chip": resolved.chip,
        }));
    }

    // ---- dev-bench: the check. Skipped entirely when the study says `any`,
    // which is a real answer and not one worth opening the bench's serial
    // link to confirm.
    if required_bench != REQUIREMENT_ANY {
        let hello = core
            .dev_bench_hello()
            .await
            .context("couldn't ask dev-bench what firmware it is running")?;
        outcome.dev_bench_version = Some(hello.firmware_version.clone());
        if !requirement_satisfied(required_bench, &hello.firmware_version)
            && !request.allow_version_mismatch
        {
            let remedy = if request.reflash.includes_dev_bench() {
                "The bench was just reflashed from the local checkout and still reports something \
                 else, so the checkout isn't at the revision this study wants."
            } else {
                "Re-run with reflash = dev-bench to rebuild and reflash the bench from the local \
                 checkout as it stands."
            };
            anyhow::bail!(mismatch_message("dev-bench", required_bench, &hello.firmware_version, remedy));
        }
    }

    // ---- DUT: the check, then the build, then the flash.
    if request.reflash.includes_dut() {
        let project_name = dut_project.expect("checked above");
        let project = config.project(project_name)?;
        let command = version_command_for(project.version_command.as_ref());
        let version = derive_version(&project.source_path, &command).await?;

        if !requirement_satisfied(required_firmware, &version) && !request.allow_version_mismatch {
            anyhow::bail!(mismatch_message(
                "DUT firmware",
                required_firmware,
                &version,
                "Nothing was built and the board was not touched — the tree is where it was.",
            ));
        }

        let resolved = crate::resolve::resolve(project, request.selection, core).await?;
        let built = build_locks
            .run_build(&resolved.plan)
            .await
            .with_context(|| format!("build failed for '{}'", project.name))?;
        if !built.ready_to_flash() {
            anyhow::bail!(
                "build for '{}' did not produce a fresh artifact, so nothing was flashed: {}",
                project.name,
                build_failure_reason(&built)
            );
        }
        let path = built.artifact_path.display().to_string();
        core.flash(
            &resolved.chip,
            &path,
            &resolved.flash_format,
            resolved.base_address.as_deref(),
            resolved.probe_serial.as_deref(),
            false,
        )
        .await
        .with_context(|| format!("flash failed for '{}'", project.name))?;

        outcome.reflashed.push(serde_json::json!({
            "target": "dut",
            "project": project.name,
            "artifact_path": path,
            "chip": resolved.chip,
            "firmware_version": version,
        }));
        outcome.flashed_firmware_version = Some(version);
    }

    // ---- submit. Core gates again, independently, and is the enforcement
    // point; everything above is convenience and early failure.
    let run = embarch_core_client::StudyRunOptions {
        allow_version_mismatch: request.allow_version_mismatch,
        flashed_firmware_version: outcome.flashed_firmware_version.clone(),
    };
    let response = core.post_study(study, &run).await?;
    outcome.study_id = response.study_id;
    Ok(outcome)
}

fn build_failure_reason(outcome: &crate::build::BuildOutcome) -> &'static str {
    if outcome.timed_out {
        "build timed out"
    } else if outcome.exit_code != Some(0) {
        "build failed"
    } else {
        "build succeeded but no fresh artifact was found"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_core_client::version::{reject_tree_mutating_command, DEFAULT_VERSION_COMMAND};

    #[test]
    fn the_four_reflash_targets_round_trip_and_nothing_else_parses() {
        for target in [
            ReflashTarget::None,
            ReflashTarget::DevBench,
            ReflashTarget::Dut,
            ReflashTarget::Both,
        ] {
            assert_eq!(ReflashTarget::parse(target.as_str()).unwrap(), target);
        }
        assert_eq!(ReflashTarget::default(), ReflashTarget::None);
        for bad in ["", "dev_bench", "DUT", "all", "yes"] {
            assert!(ReflashTarget::parse(bad).is_err(), "{bad} must not parse");
        }
    }

    #[test]
    fn both_covers_each_half_and_none_covers_neither() {
        assert!(!ReflashTarget::None.includes_dev_bench());
        assert!(!ReflashTarget::None.includes_dut());
        assert!(ReflashTarget::DevBench.includes_dev_bench());
        assert!(!ReflashTarget::DevBench.includes_dut());
        assert!(!ReflashTarget::Dut.includes_dev_bench());
        assert!(ReflashTarget::Dut.includes_dut());
        assert!(ReflashTarget::Both.includes_dev_bench());
        assert!(ReflashTarget::Both.includes_dut());
    }

    /// **The test `design.md` §3 decision 40 exists for.** It fails the
    /// moment anything in this crate's reflash path acquires a way to move an
    /// engineer's working tree — including through a `version_command` typed
    /// into a config file, which is the likeliest way it would come back.
    #[test]
    fn the_reflash_path_never_moves_the_tree() {
        // The list itself now lives with the rule (`embarch-core-client`'s
        // `version` module); what this crate pins is that *its* reflash path
        // is still governed by it, named out in full so the coupling is
        // visible from here.
        for subcommand in [
            "checkout", "switch", "reset", "restore", "clean", "stash", "merge", "rebase",
            "cherry-pick", "apply", "am", "pull", "revert", "worktree", "submodule",
            "sparse-checkout",
        ] {
            let command = vec!["git".to_string(), subcommand.to_string(), "v1.2.3".to_string()];
            let err = reject_tree_mutating_command(&command)
                .expect_err("`git {subcommand}` must be refused");
            assert!(format!("{err}").contains("never moves"), "{err}");
        }
        // Flags before the subcommand don't smuggle one past the check.
        assert!(reject_tree_mutating_command(&[
            "git".into(),
            "-C".into(),
            "/somewhere".into(),
            "checkout".into(),
            "main".into(),
        ])
        .is_err());
        // Nor does an absolute path to git.
        assert!(reject_tree_mutating_command(&[
            "/usr/bin/git".into(),
            "checkout".into(),
            "main".into(),
        ])
        .is_err());
    }

    #[test]
    fn a_read_only_git_command_is_allowed() {
        for command in [
            vec!["git".to_string(), "describe".to_string()],
            DEFAULT_VERSION_COMMAND.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            vec!["git".into(), "rev-parse".into(), "--short".into(), "HEAD".into()],
            vec!["cat".into(), "VERSION".into()],
        ] {
            reject_tree_mutating_command(&command).unwrap();
        }
        assert!(reject_tree_mutating_command(&[]).is_err());
    }

    #[test]
    fn a_project_that_declares_nothing_gets_the_suites_own_convention() {
        assert_eq!(
            version_command_for(None),
            vec!["git", "describe", "--always", "--dirty", "--abbrev=8"]
        );
        assert_eq!(version_command_for(Some(&vec![])), version_command_for(None));
        let declared = vec!["cat".to_string(), "VERSION".to_string()];
        assert_eq!(version_command_for(Some(&declared)), declared);
    }

    #[tokio::test]
    async fn a_version_command_that_would_move_the_tree_is_refused_before_it_runs() {
        let dir = std::env::temp_dir();
        let err = derive_version(&dir, &["git".into(), "checkout".into(), "main".into()])
            .await
            .expect_err("must refuse");
        assert!(format!("{err}").contains("never moves"), "{err}");
    }

    #[tokio::test]
    async fn a_blank_version_is_an_error_not_a_recorded_fact() {
        let dir = std::env::temp_dir();
        let err = derive_version(&dir, &["true".to_string()]).await.expect_err("must refuse");
        assert!(format!("{err}").contains("no output"), "{err}");
    }

    #[test]
    fn reflashing_the_dut_needs_a_project_and_nothing_else_does() {
        assert_eq!(dut_project(ReflashTarget::None, None).unwrap(), None);
        assert_eq!(dut_project(ReflashTarget::DevBench, None).unwrap(), None);
        // A project passed where it means nothing is ignored, not an error —
        // an operator who always passes it should not have to remember to
        // stop.
        assert_eq!(dut_project(ReflashTarget::DevBench, Some("dut-repo")).unwrap(), None);

        assert_eq!(dut_project(ReflashTarget::Dut, Some("dut-repo")).unwrap(), Some("dut-repo"));
        assert_eq!(dut_project(ReflashTarget::Both, Some("dut-repo")).unwrap(), Some("dut-repo"));

        for target in [ReflashTarget::Dut, ReflashTarget::Both] {
            let err = dut_project(target, None).expect_err("must name what's missing");
            assert!(format!("{err}").contains("which project is the DUT"), "{err}");
        }
    }

    #[test]
    fn a_mismatch_message_names_both_revisions_and_who_owns_the_checkout() {
        let msg = mismatch_message("DUT firmware", "g1a2b3c", "gdeadbee", "Rebuild the tree.");
        assert!(msg.contains("g1a2b3c"), "{msg}");
        assert!(msg.contains("gdeadbee"), "{msg}");
        assert!(msg.contains("will not check out"), "{msg}");
        assert!(msg.contains("allow_version_mismatch"), "{msg}");
    }
}
