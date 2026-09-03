//! The three recorded acceptance criteria that belong to `embarch_api::build`
//! rather than to the HTTP client:
//!
//! 4. the two-pipe drain invariant — a child writing heavily to one of
//!    stdout/stderr while barely touching the other must not hang,
//! 5. truncation on a UTF-8 character boundary, never mid-codepoint,
//! 6. an untouched pre-existing artifact **not** counted as fresh.
//!
//! The other three live in `tests/core_client_http.rs`.
//!
//! # Two levels, on purpose
//!
//! Criteria 5 and 6 are pinned twice: once directly against
//! [`truncate_tail`]/[`artifact_is_fresh`], and once end-to-end through
//! [`BuildLocks::run_build`] with a real child process. The direct tests are
//! exact (a byte offset chosen so that removing the boundary search
//! *panics*) and run on every platform; the end-to-end tests prove the rules
//! are actually wired into the build path, and need a POSIX shell, so they
//! are `#[cfg(unix)]`.
//!
//! Criterion 4 has no direct form — the invariant is a property of spawning
//! two concurrent drain tasks around one `child.wait()`, not of any single
//! function — so it exists only in the `#[cfg(unix)]` end-to-end form.
//! **This is the suite's one platform gap**: on Windows these four tests do
//! not run, and nothing else covers them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use embarch_api::build::{artifact_is_fresh, truncate_tail, OUTPUT_CAP_BYTES};

// ---------------------------------------------------------------------------
// Criterion 5, exactly — truncation on a UTF-8 character boundary
// ---------------------------------------------------------------------------

/// `truncate_tail` keeps the last `OUTPUT_CAP_BYTES`, which means it cuts at
/// an offset it did not choose. `String::replace_range` **panics** on an
/// offset inside a codepoint, so "cut on a character boundary" is not a
/// tidiness rule — it is what stops a large build log from taking the MCP
/// server down.
///
/// The fixture is chosen so the naive offset is genuinely mid-codepoint.
/// `OUTPUT_CAP_BYTES` is 65536 and 65536 ≡ 1 (mod 3), so in a run of 3-byte
/// characters the offset `len - 65536` can never land on a boundary — which
/// makes `"€"` the character that exposes a missing guard, and makes a run of
/// 4-byte emoji (65536 ≡ 0 mod 4) useless for it. Removing the
/// `is_char_boundary` search from `build.rs` turns this test into a panic.
#[test]
fn truncation_cuts_on_a_character_boundary_not_mid_codepoint() {
    // 40000 × 3 bytes = 120000. 120000 − 65536 = 54464, and 54464 ≡ 2 (mod 3):
    // inside a '€'. The guard advances to 54465, keeping 65535 bytes.
    let truncated = truncate_tail("€".repeat(40_000));

    let marker = format!("...[truncated to last {OUTPUT_CAP_BYTES} bytes]...\n");
    let tail = truncated
        .strip_prefix(&marker)
        .unwrap_or_else(|| panic!("truncation marker missing; got {:?}", &truncated[..80]));

    assert_eq!(
        tail.len(),
        OUTPUT_CAP_BYTES - 1,
        "the cut moved to the wrong boundary"
    );
    assert!(
        tail.chars().all(|c| c == '€'),
        "the retained tail is not a clean run of whole characters"
    );
}

/// The same, with a mixed-width string, so the pin does not depend on the
/// whole buffer being one character wide.
#[test]
fn truncation_cuts_on_a_character_boundary_in_mixed_width_text() {
    // 4 bytes of emoji, then 120000 bytes of '€': len 120004, naive offset
    // 54468, which is 54464 bytes into the '€' run — again ≡ 2 (mod 3).
    let truncated = truncate_tail(format!("🛰{}", "€".repeat(40_000)));

    let marker = format!("...[truncated to last {OUTPUT_CAP_BYTES} bytes]...\n");
    let tail = truncated
        .strip_prefix(&marker)
        .expect("truncation marker missing");

    assert!(
        tail.chars().all(|c| c == '€'),
        "the retained tail is not a clean run of whole characters"
    );
}

/// Where the offset already is a boundary, nothing moves: exactly the cap is
/// kept. Without this the test above could be satisfied by a version that
/// over-trimmed on every input.
#[test]
fn an_ascii_log_keeps_exactly_the_cap() {
    let truncated = truncate_tail("a".repeat(100_000));
    let marker = format!("...[truncated to last {OUTPUT_CAP_BYTES} bytes]...\n");
    let tail = truncated
        .strip_prefix(&marker)
        .expect("truncation marker missing");
    assert_eq!(tail.len(), OUTPUT_CAP_BYTES);
}

/// Under the cap, the text is returned untouched and unmarked.
#[test]
fn a_short_log_is_not_touched() {
    let short = "west build: ok\n".to_string();
    assert_eq!(truncate_tail(short.clone()), short);
}

// ---------------------------------------------------------------------------
// Criterion 6, exactly — an untouched pre-existing artifact is not fresh
// ---------------------------------------------------------------------------

/// The rule, stated directly. `build_start` is moved rather than the file's
/// mtime, because setting an mtime needs a dependency and the arithmetic is
/// the same either way: what matters is the gap between the two, and its
/// sign.
#[test]
fn a_pre_existing_artifact_that_was_not_rewritten_is_not_fresh() {
    let dir = TempDir::new("freshness");
    let artifact = dir.path().join("firmware.hex");
    std::fs::write(&artifact, b"left over from the last build").expect("could not write fixture");

    // A build that started five seconds after this file was last written and
    // never touched it. Five seconds is well outside FRESHNESS_CLOCK_GRACE.
    let build_start = SystemTime::now() + Duration::from_secs(5);

    assert!(
        !artifact_is_fresh(&artifact, true, build_start),
        "a stale artifact was reported fresh — a flash would have written last build's firmware"
    );
}

/// The counterpart: an artifact that did not exist before the build is fresh
/// by definition, whatever its mtime says.
#[test]
fn an_artifact_created_by_this_build_is_fresh() {
    let dir = TempDir::new("freshness-new");
    let artifact = dir.path().join("firmware.hex");
    std::fs::write(&artifact, b"just built").expect("could not write fixture");

    assert!(artifact_is_fresh(
        &artifact,
        false,
        SystemTime::now() + Duration::from_secs(5)
    ));
}

/// A pre-existing artifact the build *did* rewrite is fresh. Without this,
/// "never fresh" would pass the test above and break every rebuild.
#[test]
fn a_pre_existing_artifact_that_was_rewritten_is_fresh() {
    let dir = TempDir::new("freshness-rewrite");
    let artifact = dir.path().join("firmware.hex");
    std::fs::write(&artifact, b"left over").expect("could not write fixture");

    let build_start = SystemTime::now();
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&artifact, b"rebuilt").expect("could not rewrite fixture");

    assert!(artifact_is_fresh(&artifact, true, build_start));
}

/// An artifact path that names nothing is not fresh — the metadata read
/// fails, and a failed read must not be read as a pass.
#[test]
fn a_missing_artifact_is_not_fresh() {
    let dir = TempDir::new("freshness-missing");
    assert!(!artifact_is_fresh(
        &dir.path().join("never-written.hex"),
        true,
        SystemTime::now()
    ));
}

// ---------------------------------------------------------------------------
// End-to-end, through a real child process
// ---------------------------------------------------------------------------

/// Criterion 4. A child that fills one pipe while barely touching the other
/// deadlocks any parent that drains the two in sequence: the child blocks
/// writing into the full pipe, so it never exits, so the *other* pipe never
/// reaches EOF, so the parent waits forever. `run_build_locked` spawns both
/// drains before waiting, which is what this pins.
///
/// Run in both directions. A parent that drained stdout to completion first
/// hangs on the heavy-stderr case and passes the heavy-stdout one; a parent
/// that drained stderr first does the reverse. Only concurrent draining
/// passes both.
///
/// The outer `tokio::time::timeout` is what turns the regression into a
/// failing test rather than a hung test runner. The plan's own
/// `timeout_secs` is deliberately far larger, so it cannot be what rescues
/// the test.
#[cfg(unix)]
#[tokio::test]
async fn a_child_that_floods_one_pipe_and_trickles_the_other_does_not_hang() {
    // ~140 KB, comfortably past a 64 KB pipe buffer in either direction.
    const LINES: usize = 2_000;
    const PAD: &str = "0123456789012345678901234567890123456789012345678901234567890";

    for (label, script, heavy_is_stderr) in [
        (
            "heavy stderr, one stdout line",
            format!(
                "i=0; while [ $i -lt {LINES} ]; do echo \"padding $i {PAD}\" >&2; \
                 i=$((i+1)); done; echo 'the one stdout line'"
            ),
            true,
        ),
        (
            "heavy stdout, one stderr line",
            format!(
                "i=0; while [ $i -lt {LINES} ]; do echo \"padding $i {PAD}\"; \
                 i=$((i+1)); done; echo 'the one stderr line' >&2"
            ),
            false,
        ),
    ] {
        let dir = TempDir::new("two-pipe");
        let plan = shell_plan(dir.path(), &script, &dir.path().join("firmware.hex"));

        let outcome = tokio::time::timeout(
            Duration::from_secs(60),
            embarch_api::build::BuildLocks::new().run_build(&plan),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: run_build never returned — the drain deadlocked"))
        .unwrap_or_else(|error| panic!("{label}: run_build failed: {error:#}"));

        assert_eq!(outcome.exit_code, Some(0), "{label}: child did not exit 0");
        assert!(!outcome.timed_out, "{label}: the build timed out");

        let (heavy, light, light_text) = if heavy_is_stderr {
            (&outcome.stderr, &outcome.stdout, "the one stdout line")
        } else {
            (&outcome.stdout, &outcome.stderr, "the one stderr line")
        };

        // Reaching the last line means the drain ran to EOF rather than
        // stopping once the child was reaped.
        assert!(
            heavy.contains(&format!("padding {}", LINES - 1)),
            "{label}: the heavy stream was cut short before its last line"
        );
        assert!(
            light.contains(light_text),
            "{label}: the quiet stream's single line was lost"
        );
    }
}

/// Criterion 5, wired in: a build whose log is multibyte and over the cap
/// comes back truncated and intact rather than panicking the task that
/// captured it.
#[cfg(unix)]
#[tokio::test]
async fn a_multibyte_build_log_survives_the_cap_end_to_end() {
    let dir = TempDir::new("multibyte");
    let big = dir.path().join("big.txt");
    // A trailing newline matters: `drain_stream` re-adds one per line, and
    // the byte arithmetic that puts the naive cut inside a '€' depends on
    // the total length. 120000 bytes of '€' + "\n" + "end" + "\n" = 120005,
    // and 120005 − 65536 = 54469 ≡ 1 (mod 3).
    std::fs::write(&big, format!("{}\n", "€".repeat(40_000))).expect("could not write fixture");

    let plan = shell_plan(
        dir.path(),
        &format!("cat {}; echo end", big.display()),
        &dir.path().join("firmware.hex"),
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        embarch_api::build::BuildLocks::new().run_build(&plan),
    )
    .await
    .expect("run_build never returned")
    .expect("run_build failed");

    let marker = format!("...[truncated to last {OUTPUT_CAP_BYTES} bytes]...\n");
    let tail = outcome
        .stdout
        .strip_prefix(&marker)
        .expect("a 120 KB log came back untruncated");
    assert!(
        tail.starts_with('€'),
        "the retained tail does not begin on a whole character"
    );
    assert!(tail.ends_with("end\n"), "the end of the log was lost");
}

/// Criterion 6, wired in. The build succeeds, exits 0, and leaves the
/// pre-existing artifact exactly where it was — and `ready_to_flash()` must
/// still say no, because flashing here would burn the previous build's
/// firmware while reporting success.
#[cfg(unix)]
#[tokio::test]
async fn a_build_that_does_not_rewrite_the_artifact_is_not_ready_to_flash() {
    let dir = TempDir::new("stale-artifact");
    let artifact = dir.path().join("firmware.hex");
    std::fs::write(&artifact, b"the previous build's firmware").expect("could not write fixture");

    // The freshness check carries a 500 ms clock-jitter grace; wait past it
    // so the artifact is unambiguously older than the build.
    tokio::time::sleep(Duration::from_millis(900)).await;

    let plan = shell_plan(
        dir.path(),
        "echo 'nothing to do, everything up to date'",
        &artifact,
    );
    let outcome = embarch_api::build::BuildLocks::new()
        .run_build(&plan)
        .await
        .expect("run_build failed");

    assert!(outcome.build_succeeded(), "the build itself should have passed");
    assert!(
        !outcome.artifact_fresh,
        "an untouched artifact was counted as fresh"
    );
    assert!(
        !outcome.ready_to_flash(),
        "a stale artifact was cleared for flashing"
    );

    // The positive control, same directory and same wait: rewrite it and the
    // identical plan now is ready to flash. Without this, `artifact_fresh`
    // hardwired to `false` would pass the assertions above.
    let rewrite = shell_plan(dir.path(), "printf rebuilt > firmware.hex", &artifact);
    let outcome = embarch_api::build::BuildLocks::new()
        .run_build(&rewrite)
        .await
        .expect("run_build failed");
    assert!(
        outcome.ready_to_flash(),
        "a freshly rewritten artifact was not cleared for flashing"
    );
}

/// A build that exits 0 without ever producing the artifact is not ready to
/// flash either — the `artifact_path.exists()` half of the same rule.
#[cfg(unix)]
#[tokio::test]
async fn a_build_that_never_produced_the_artifact_is_not_ready_to_flash() {
    let dir = TempDir::new("absent-artifact");
    let plan = shell_plan(dir.path(), "echo built nothing", &dir.path().join("firmware.hex"));

    let outcome = embarch_api::build::BuildLocks::new()
        .run_build(&plan)
        .await
        .expect("run_build failed");

    assert!(outcome.build_succeeded());
    assert!(!outcome.ready_to_flash(), "a missing artifact was cleared for flashing");
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn shell_plan(cwd: &Path, script: &str, artifact: &Path) -> embarch_api::build::BuildPlan {
    embarch_api::build::BuildPlan {
        lock_key: format!("test:{}", cwd.display()),
        cwd: cwd.to_path_buf(),
        command: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
        artifact_path: artifact.to_path_buf(),
        // Far longer than any test here needs, so a test that hangs is caught
        // by its own outer timeout and reported as the deadlock it is, rather
        // than being quietly rescued by the build timeout.
        timeout_secs: 300,
        env: std::collections::HashMap::new(),
    }
}

/// A scratch directory that removes itself. Hand-rolled rather than pulling
/// in `tempfile`, for the same reason the mock HTTP server is hand-rolled:
/// this is a dozen lines and the crate has no such dependency today.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "embarch-api-test-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("could not create the test's scratch directory");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
