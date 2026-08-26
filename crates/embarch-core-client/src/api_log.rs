//! Where `embarch-api`'s own rolling logfile lives, and how to read it back
//! — the one fact `embarch-api` (which writes the file) and `embarch-ui`
//! (whose Debug tab tails it) must never disagree about.
//!
//! `embarch-api/design.md` §3 decision 43 and `embarch-ui/design.md` §3
//! decision 13. Core's `/logs/recent`+`/logs/stream` pattern doesn't
//! transfer here: `embarch-api` is spawned per Claude Code session as an MCP
//! server, or run once as a CLI and gone, so there is no process to ask and
//! — for the case that actually motivated this — no process left at all by
//! the time someone wants to know what an agent did twenty minutes ago. A
//! file outlives its writer; an endpoint does not.
//!
//! **Why this module is in *this* crate.** It has nothing to do with
//! reaching Core over HTTP, which is what the rest of this crate is for.
//! But `embarch-core-client` is the only crate `embarch-api` and
//! `embarch-ui` both depend on, and a path that two repos resolve
//! independently is a path they will eventually resolve differently — the
//! duplication `embarch-topology/design.md` decisions 2/8/14 exist to
//! prevent. One definition, two call sites, chosen over a tidier home.
//!
//! **Per-user, not machine-wide — a correction to decision 43 as written**
//! (2026-08-25). That decision said "the machine data dir," the
//! `%ProgramData%\embarch` / `/var/lib/embarch` location `embarch-core`'s
//! `token_store.rs` established. That works for Core, which runs as a
//! Windows service; it does not work for `embarch-api`, which runs as the
//! engineer — `/var/lib` is `root`-owned, and creating a subdirectory there
//! is permission-denied for a normal user (verified on this bench, not
//! assumed). Every alternative was worse: hard-failing makes logging depend
//! on a one-time `sudo`, and probing "machine dir if writable, else
//! per-user" lets the writer and the reader land in different places. The
//! per-user directory is deterministic, always writable, and matches
//! `embarch-api/design.md` §3 decision 1's scope exactly — one engineer,
//! one stack, no multi-tenancy to be machine-wide *for*.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `tracing-appender`'s `filename_prefix` for the file `embarch-api` writes
/// — each day's file is named `<prefix>.<yyyy-MM-dd>`, matching the scheme
/// `embarch-core`'s own `logs.rs` uses for `core.log`.
pub const LOG_FILE_PREFIX: &str = "api.log";

/// `<per-user data dir>/embarch/api/logs`. Not created here — the writer
/// (`tracing-appender`) creates it on first use, and a reader treats a
/// missing directory as "nothing logged yet," not an error.
pub fn log_dir() -> Result<PathBuf> {
    Ok(user_data_dir()?.join("api").join("logs"))
}

#[cfg(windows)]
fn user_data_dir() -> Result<PathBuf> {
    let local_app_data = std::env::var("LOCALAPPDATA")
        .context("LOCALAPPDATA environment variable is not set")?;
    Ok(PathBuf::from(local_app_data).join("embarch"))
}

/// `$XDG_DATA_HOME/embarch`, or `$HOME/.local/share/embarch` — the XDG
/// default, spelled out rather than pulling in a crate for two lines.
#[cfg(unix)]
fn user_data_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return Ok(PathBuf::from(xdg).join("embarch"));
        }
    }
    let home = std::env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home).join(".local").join("share").join("embarch"))
}

/// The most recent day's file among `candidates`. `tracing-appender`'s date
/// suffix is ISO (`yyyy-MM-dd`), so lexicographic order agrees with
/// chronological order and no date parsing is needed — the same trick
/// `embarch-core`'s `logs::latest_log_file` uses on its own directory.
fn latest_log_file(candidates: &[PathBuf]) -> Option<&PathBuf> {
    let prefix = format!("{LOG_FILE_PREFIX}.");
    candidates
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .max_by_key(|path| path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
}

/// The last `tail` lines of the current day's file.
///
/// **An absent directory or an absent file is `Ok(vec![])`, not an error**,
/// and that is the common case rather than an edge one: an engineer can
/// perfectly well open the Debug tab on a machine where `embarch-api` has
/// never been invoked. Only a directory that exists and cannot be read is a
/// real failure.
pub fn read_recent(tail: usize) -> Result<Vec<String>> {
    read_recent_in(&log_dir()?, tail)
}

/// [`read_recent`] against an arbitrary directory — split out so this is
/// testable against a real temp directory without depending on the
/// machine's own `log_dir()` resolution or mutating process-wide env vars a
/// parallel test could observe. Same split `embarch-core`'s `logs.rs` makes
/// for its own `FollowState::poll_in`.
fn read_recent_in(dir: &Path, tail: usize) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let candidates = read_dir_candidates(dir)?;
    let Some(latest) = latest_log_file(&candidates) else {
        return Ok(Vec::new());
    };
    let contents = std::fs::read_to_string(latest)
        .with_context(|| format!("failed to read log file {}", latest.display()))?;
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].iter().map(|l| l.to_string()).collect())
}

fn read_dir_candidates(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(dir)
        .with_context(|| format!("failed to read log directory {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_under_the_per_user_data_dir_and_never_var_lib() {
        // The whole point of the correction to decision 43: this must not
        // resolve anywhere a normal user can't write.
        let dir = log_dir().expect("log_dir should resolve");
        assert!(dir.ends_with("api/logs") || dir.ends_with(r"api\logs"), "{dir:?}");
        assert!(!dir.starts_with("/var/lib"), "{dir:?}");
    }

    #[test]
    fn latest_log_file_picks_the_largest_iso_date_and_ignores_other_files() {
        let candidates = vec![
            PathBuf::from("/logs/api.log.2026-08-23"),
            PathBuf::from("/logs/api.log.2026-08-25"),
            PathBuf::from("/logs/api.log.2026-08-24"),
            // Core's own file, if the two ever shared a directory, plus
            // whatever else happens to be lying around.
            PathBuf::from("/logs/core.log.2026-08-26"),
            PathBuf::from("/logs/token"),
        ];
        assert_eq!(
            latest_log_file(&candidates),
            Some(&PathBuf::from("/logs/api.log.2026-08-25"))
        );
    }

    #[test]
    fn latest_log_file_is_none_when_nothing_matches() {
        assert_eq!(latest_log_file(&[PathBuf::from("/logs/token")]), None);
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "embarch-api-log-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn read_recent_on_a_machine_that_has_never_run_embarch_api_is_empty_not_an_error() {
        let absent = temp_dir("absent");
        assert!(!absent.exists());
        assert!(read_recent_in(&absent, 100).unwrap().is_empty());
    }

    #[test]
    fn read_recent_reads_only_the_current_days_file_and_only_its_tail() {
        let dir = temp_dir("tail");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{LOG_FILE_PREFIX}.2026-08-24")), "yesterday\n").unwrap();
        std::fs::write(
            dir.join(format!("{LOG_FILE_PREFIX}.2026-08-25")),
            "one\ntwo\nthree\n",
        )
        .unwrap();

        assert_eq!(read_recent_in(&dir, 2).unwrap(), vec!["two".to_string(), "three".to_string()]);
        // A tail larger than the file returns everything, not an error.
        assert_eq!(read_recent_in(&dir, 500).unwrap().len(), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_with_no_matching_file_is_empty_not_an_error() {
        let dir = temp_dir("no-match");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("core.log.2026-08-25"), "not ours\n").unwrap();
        assert!(read_recent_in(&dir, 100).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
