//! The three recorded acceptance criteria that belong to `embarch_api::build`
//! rather than to the HTTP client:
//!
//! 4. the two-pipe drain invariant — a child writing heavily to one of
//!    stdout/stderr while barely touching the other must not hang,
//! 5. truncation on a UTF-8 character boundary, never mid-codepoint — now at
//!    **both** cuts, since the log is kept head-and-tail (decision 18),
//! 6. an untouched pre-existing artifact **not** counted as fresh,
//! 7. a non-UTF-8 byte anywhere in a stream costing only its own line, not
//!    everything drained after it.
//!
//! The other three live in `tests/core_client_http.rs`.
//!
//! # Two levels, on purpose
//!
//! Criteria 5 and 6 are pinned twice: once directly against
//! [`truncate_log`]/[`artifact_is_fresh`], and once end-to-end through
//! [`BuildLocks::run_build`] with a real child process. The direct tests are
//! exact (a byte offset chosen so that removing the boundary search
//! *panics*) and run on every platform; the end-to-end tests prove the rules
//! are actually wired into the build path, and need a POSIX shell, so they
//! are `#[cfg(unix)]`.
//!
//! Decision 19's `target.json` is pinned the same two ways, and for the
//! same reason: the doc claimed the file for months while nothing wrote it,
//! so a direct round-trip test alone would only prove the writer works, not
//! that a build ever calls it.
//!
//! Criterion 4 has no direct form — the invariant is a property of spawning
//! two concurrent drain tasks around one `child.wait()`, not of any single
//! function — so it exists only in the `#[cfg(unix)]` end-to-end form.
//! **This is the suite's one platform gap**: on Windows these four tests do
//! not run, and nothing else covers them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use embarch_api::build::{
    artifact_is_fresh, truncate_log, write_target_manifest, TargetManifest, OUTPUT_CAP_BYTES,
    OUTPUT_HEAD_BYTES, TARGET_MANIFEST_NAME,
};

// ---------------------------------------------------------------------------
// Criterion 5, exactly — truncation on a UTF-8 character boundary
// ---------------------------------------------------------------------------

/// Splits a truncated log at its marker. The marker's own text carries
/// numbers that depend on the input, so tests match its fixed delimiters and
/// then read the numbers back out of it rather than restating a whole line
/// that would have to be recomputed per fixture.
fn split_at_marker(out: &str) -> (&str, &str, &str) {
    let (head, rest) = out.split_once("\n...[").unwrap_or_else(|| {
        // Char-wise, not a byte slice: a multibyte fixture would panic here
        // instead of reporting the failure it was written to report.
        let start: String = out.chars().take(40).collect();
        panic!("truncation marker missing; log starts {start:?}")
    });
    let (marker, tail) = rest
        .split_once("]...\n")
        .expect("truncation marker was never closed");
    (head, marker, tail)
}

/// The one invariant every over-cap case must satisfy, asserted from the
/// output alone: the retained halves fit the cap (the split does **not**
/// double it), and the marker's arithmetic is true rather than decorative.
fn assert_within_cap_and_marker_is_honest(out: &str, original_len: usize) {
    let (head, marker, tail) = split_at_marker(out);
    assert!(
        head.len() + tail.len() <= OUTPUT_CAP_BYTES,
        "kept {} + {} bytes, over the {OUTPUT_CAP_BYTES}-byte cap",
        head.len(),
        tail.len()
    );
    let dropped: usize = marker
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("marker does not start with a byte count: {marker:?}"));
    assert_eq!(
        head.len() + dropped + tail.len(),
        original_len,
        "the marker's counts do not add back up to the original log"
    );
    assert!(
        marker.contains(&format!("first {}", head.len()))
            && marker.contains(&format!("last {}", tail.len())),
        "the marker does not say what was actually kept: {marker:?}"
    );
}

/// `truncate_log` cuts at two offsets it did not choose. Slicing a `str` at
/// an offset inside a codepoint **panics**, so "cut on a character boundary"
/// is not a tidiness rule — it is what stops a large build log from taking
/// the MCP server down.
///
/// The fixture puts **both** naive offsets mid-`€`, which takes some care
/// because the two constants are congruent differently. In a run of 3-byte
/// characters starting at offset 0: the head offset is `OUTPUT_HEAD_BYTES` =
/// 16384 ≡ 1 (mod 3), so it is always mid-character — good. The tail offset
/// is `len - 49152` and 49152 ≡ 0 (mod 3), so against a pure `'€'` run it
/// would always land *on* a boundary and prove nothing. The trailing ASCII
/// `'X'` is what shifts it: `len` becomes 120001, the naive tail offset 70849
/// ≡ 1 (mod 3), and the guard has to advance to 70851. Removing either
/// `is_char_boundary` walk from `build.rs` turns this test into a panic.
#[test]
fn truncation_cuts_on_a_character_boundary_at_both_ends() {
    let original = format!("{}X", "€".repeat(40_000));
    let original_len = original.len();
    assert_eq!(original_len, 120_001);

    let out = truncate_log(original);
    let (head, _, tail) = split_at_marker(&out);

    // Head: rounded down from 16384 to 16383, three whole '€' short of the
    // budget by one byte.
    assert_eq!(head.len(), 16_383, "the head cut moved to the wrong boundary");
    assert!(
        head.chars().all(|c| c == '€'),
        "the retained head is not a clean run of whole characters"
    );

    // Tail: rounded up from 70849 to 70851, so it starts on a whole '€' and
    // still reaches the real end of the log.
    assert_eq!(
        tail.len(),
        original_len - 70_851,
        "the tail cut moved to the wrong boundary"
    );
    assert!(tail.starts_with('€'), "the tail begins mid-character");
    assert!(tail.ends_with('X'), "the end of the log was lost");

    assert_within_cap_and_marker_is_honest(&out, original_len);
}

/// The point of the split, stated as a test: a build that fails immediately
/// and then emits megabytes of cascade still hands back its **first** error.
/// A tail-only cap fails this outright, which is what makes it the pin for
/// decision 18 rather than for the boundary arithmetic.
#[test]
fn the_first_error_survives_a_log_that_is_mostly_cascade() {
    let first_error = "app/src/main.c:12:5: error: 'sensor_channel' undeclared\n";
    let out = truncate_log(format!("{first_error}{}", "cascade noise\n".repeat(30_000)));

    assert!(
        out.starts_with(first_error),
        "the first error was scrolled off the top; log starts {:?}",
        &out[..80]
    );
}

/// Where both offsets already are boundaries, neither moves: exactly the cap
/// is kept, split 16 KB / 48 KB. Without this the tests above could be
/// satisfied by a version that over-trimmed on every input.
#[test]
fn an_ascii_log_keeps_exactly_the_cap() {
    let out = truncate_log("a".repeat(100_000));
    let (head, _, tail) = split_at_marker(&out);
    assert_eq!(head.len(), OUTPUT_HEAD_BYTES);
    assert_eq!(tail.len(), OUTPUT_CAP_BYTES - OUTPUT_HEAD_BYTES);
    assert_eq!(head.len() + tail.len(), OUTPUT_CAP_BYTES);
    assert_within_cap_and_marker_is_honest(&out, 100_000);
}

/// One byte over the cap is still a cut, and still an honest one — the kept
/// halves must not overlap and re-report a byte twice. This is the edge where
/// a head budget and a tail budget summing to the cap could collide.
#[test]
fn a_log_one_byte_over_the_cap_drops_exactly_one_byte() {
    let original_len = OUTPUT_CAP_BYTES + 1;
    let out = truncate_log("a".repeat(original_len));
    let (head, _, tail) = split_at_marker(&out);
    assert_eq!(head.len() + tail.len(), OUTPUT_CAP_BYTES);
    assert_within_cap_and_marker_is_honest(&out, original_len);
}

/// Under the cap, the text is returned untouched and unmarked.
#[test]
fn a_short_log_is_not_touched() {
    let short = "west build: ok\n".to_string();
    assert_eq!(truncate_log(short.clone()), short);
}

/// Exactly at the cap is still untouched — the boundary of "not touched" is
/// `<=`, and an off-by-one here would mark a log that lost nothing.
#[test]
fn a_log_exactly_at_the_cap_is_not_touched() {
    let exact = "a".repeat(OUTPUT_CAP_BYTES);
    assert_eq!(truncate_log(exact.clone()), exact);
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
/// comes back cut at both ends and intact rather than panicking the task
/// that captured it — and the head the split exists to keep is really there
/// after a real child process, not only in the direct test.
#[cfg(unix)]
#[tokio::test]
async fn a_multibyte_build_log_survives_the_cap_end_to_end() {
    let dir = TempDir::new("multibyte");
    let big = dir.path().join("big.txt");
    // A trailing newline matters: `drain_stream` re-adds one per line, and
    // the byte arithmetic that puts both naive cuts inside a '€' depends on
    // the total length. 120000 bytes of '€' + "\n" + "end" + "\n" = 120005.
    // Head: 16384 ≡ 1 (mod 3), rounded down to 16383. Tail: 120005 − 49152 =
    // 70853 ≡ 2 (mod 3), rounded up to 70854. Neither guard is a no-op here.
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

    let (head, _, tail) = split_at_marker(&outcome.stdout);
    assert_eq!(head.len(), 16_383, "the head cut moved to the wrong boundary");
    assert!(
        head.starts_with('€') && head.chars().all(|c| c == '€'),
        "the retained head is not a clean run of whole characters"
    );
    assert!(
        tail.starts_with('€'),
        "the retained tail does not begin on a whole character"
    );
    assert!(tail.ends_with("end\n"), "the end of the log was lost");
    assert_within_cap_and_marker_is_honest(&outcome.stdout, 120_005);
}

/// The defect this task exists to close: `next_line()` (used until this
/// task) returns `Err(InvalidData)` on the first non-UTF-8 byte, and the old
/// drain treated that identically to a clean EOF — so a stray non-UTF-8 byte
/// anywhere in a build log silently truncated everything after it, with no
/// error and no marker. Both `error:` lines here must survive, and the
/// invalid line's replacement must be visible rather than merely absent.
#[cfg(unix)]
#[tokio::test]
async fn a_non_utf8_byte_does_not_truncate_the_rest_of_the_log() {
    let dir = TempDir::new("non-utf8");
    // `\377`/`\376` are octal for 0xff/0xfe — on their own line, between the
    // two `error:` lines the old drain would stop at the first and lose the
    // second.
    let plan = shell_plan(
        dir.path(),
        "printf 'error: bad\\n\\377\\376\\nerror: the real one\\n'",
        &dir.path().join("firmware.hex"),
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        embarch_api::build::BuildLocks::new().run_build(&plan),
    )
    .await
    .expect("run_build never returned")
    .expect("run_build failed");

    assert_eq!(outcome.exit_code, Some(0), "the fixture child did not exit 0");
    assert!(
        outcome.stdout.contains("error: bad"),
        "the line before the bad byte was lost; stdout was {:?}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("error: the real one"),
        "the line after the bad byte was silently dropped; stdout was {:?}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("non-UTF-8"),
        "the lossy substitution was not surfaced anywhere in the capture; stdout was {:?}",
        outcome.stdout
    );
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
// Decision 19 — the build directory's `target.json`
// ---------------------------------------------------------------------------

/// The round trip that makes the file worth writing: whatever `resolve`
/// descriptor went in comes back out equal, so `cat target.json` answers
/// "what produced this directory" exactly and not approximately.
#[test]
fn a_target_manifest_round_trips_the_resolved_selection() {
    let dir = TempDir::new("manifest-round-trip");
    std::fs::create_dir_all(dir.path()).expect("could not create the build directory");

    // Every axis a directory name loses: a `-` inside three different
    // fields, two absent axes, and the `extra_args` that exists in the name
    // only as a hash.
    let target = serde_json::json!({
        "project": "example-sensor-fw",
        "board": "nrf54l15dk/nrf54l15/cpuapp",
        "soc": "nrf54l15",
        "cpucluster": "cpuapp",
        "variant": serde_json::Value::Null,
        "revision": serde_json::Value::Null,
        "app": "ble-shell",
        "snippets": ["wdt31"],
        "extra_args": ["-DCONFIG_LOG=n", "--pristine=always"],
    });

    let wrote = write_target_manifest(&TargetManifest {
        dir: dir.path().to_path_buf(),
        target: target.clone(),
    })
    .expect("writing the manifest failed");
    assert!(wrote, "an existing build directory got no manifest");

    let text = std::fs::read_to_string(dir.path().join(TARGET_MANIFEST_NAME))
        .expect("target.json was not written");
    let mut read_back: serde_json::Value =
        serde_json::from_str(&text).expect("target.json was not valid JSON");

    // Stamped by the one serializer, like every other JSON object this
    // crate emits — a consumer in another repo reads this file, so it needs
    // the same shape guarantee a `--json` object gives.
    assert_eq!(
        read_back[embarch_api::json_out::SCHEMA_VERSION_FIELD],
        serde_json::json!(embarch_api::json_out::SCHEMA_VERSION),
        "target.json carries no schema_version"
    );
    read_back
        .as_object_mut()
        .expect("target.json was not an object")
        .remove(embarch_api::json_out::SCHEMA_VERSION_FIELD);

    assert_eq!(read_back, target, "the manifest did not round-trip");
    assert!(text.ends_with('\n'), "target.json should end in a newline");
}

/// The file is evidence *about* a build directory, so it never conjures one.
/// This is the half a consumer depends on: an absent `target.json` has to
/// mean "unattributable", which it cannot if this crate writes one wherever
/// a plan happens to point.
#[test]
fn no_manifest_is_written_where_no_build_directory_exists() {
    let dir = TempDir::new("manifest-no-dir");
    let absent = dir.path().join("never-built");

    let wrote = write_target_manifest(&TargetManifest {
        dir: absent.clone(),
        target: serde_json::json!({ "project": "example-sensor-fw" }),
    })
    .expect("an absent directory should not be an error");

    assert!(!wrote, "a manifest was written for a directory that does not exist");
    assert!(!absent.exists(), "the build directory was created out of nothing");
}

/// Wired in, not merely available — the defect this closes was a written
/// claim with no caller. A build that produces its directory leaves a
/// `target.json` in it, through the same `run_build` path a real build takes.
#[cfg(unix)]
#[tokio::test]
async fn a_real_build_leaves_a_target_manifest_in_its_build_directory() {
    let dir = TempDir::new("manifest-e2e");
    let build_dir = dir.path().join("nrf54l15dk-ble-shell");
    std::fs::create_dir_all(&build_dir).expect("could not create the build directory");
    let artifact = build_dir.join("zephyr.hex");

    let mut plan = shell_plan(
        dir.path(),
        "printf built > nrf54l15dk-ble-shell/zephyr.hex",
        &artifact,
    );
    plan.manifest = Some(TargetManifest {
        dir: build_dir.clone(),
        target: sample_descriptor(),
    });

    let outcome = embarch_api::build::BuildLocks::new()
        .run_build(&plan)
        .await
        .expect("run_build failed");
    assert!(outcome.ready_to_flash(), "the fixture build should have succeeded");

    let text = std::fs::read_to_string(build_dir.join(TARGET_MANIFEST_NAME))
        .expect("a successful build left no target.json");
    let mut read_back: serde_json::Value =
        serde_json::from_str(&text).expect("target.json was not valid JSON");
    read_back
        .as_object_mut()
        .expect("target.json was not an object")
        .remove(embarch_api::json_out::SCHEMA_VERSION_FIELD);
    assert_eq!(read_back, sample_descriptor());
}

/// A *failed* build's directory is attributable too. It is the case that
/// matters most in a listing — a directory nobody can explain is usually one
/// whose build died — and it is why the write happens after the command
/// rather than only on success.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_build_still_leaves_its_directory_attributable() {
    let dir = TempDir::new("manifest-failed");
    let build_dir = dir.path().join("nrf54l15dk-ble-shell");
    std::fs::create_dir_all(&build_dir).expect("could not create the build directory");

    let mut plan = shell_plan(dir.path(), "exit 3", &build_dir.join("zephyr.hex"));
    plan.manifest = Some(TargetManifest {
        dir: build_dir.clone(),
        target: sample_descriptor(),
    });

    let outcome = embarch_api::build::BuildLocks::new()
        .run_build(&plan)
        .await
        .expect("run_build failed");
    assert!(!outcome.build_succeeded(), "the fixture build should have failed");
    assert!(
        build_dir.join(TARGET_MANIFEST_NAME).exists(),
        "a failed build left its directory unattributable"
    );
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
        manifest: None,
    }
}

/// The shape `resolve::resolve_zephyr` hands to a real build: the descriptor
/// of a target whose directory name is genuinely lossy — `nrf54l15dk` and
/// `ble-shell` both contain `-`, and `extra_args` survives only as a hash.
#[cfg(unix)]
fn sample_descriptor() -> serde_json::Value {
    serde_json::json!({
        "project": "example-sensor-fw",
        "board": "nrf54l15dk/nrf54l15/cpuapp",
        "soc": "nrf54l15",
        "cpucluster": "cpuapp",
        "variant": serde_json::Value::Null,
        "revision": serde_json::Value::Null,
        "app": "ble-shell",
        "snippets": ["wdt31"],
        "extra_args": ["-DCONFIG_LOG=n"],
    })
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
