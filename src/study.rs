//! Shared helper for the `run_study` MCP tool / `run-study` CLI subcommand
//! (`tools.rs`, `cli.rs`): recomputing `Study.steps_crc` immediately before
//! submission to embarch-core.

use embarch_study_designer::{steps_crc, StepTooLargeError, Study};

/// Overwrites `study.steps_crc` with a freshly computed value over
/// `study.steps`, regardless of whatever value (including a missing/zero
/// one) was already present in the submitted JSON —
/// `embarch-study-designer/design.md` §3 decision 26: `steps_crc` is filled
/// in by whoever *submits* a `Study`, unconditionally, not trusted from the
/// caller. Idempotent: a caller that already computed a correct value is
/// unaffected.
///
/// Errors only if a single `Step`'s postcard encoding doesn't fit
/// `steps_crc`'s internal scratch buffer (`StepTooLargeError`) — should be
/// unreachable given `embarch-study-designer`'s configured `limits`, but
/// surfaced as an error rather than assumed impossible.
pub fn recompute_steps_crc(study: &mut Study) -> Result<(), StepTooLargeError> {
    study.steps_crc = steps_crc(&study.steps)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::{Action, BleRole};

    fn study_with_crc(crc: u32) -> Study {
        let mut steps: heapless::Vec<embarch_study_designer::Step, { embarch_study_designer::limits::MAX_STEPS_PER_STUDY }> =
            heapless::Vec::new();
        steps
            .push(embarch_study_designer::Step {
                name: heapless::String::try_from("connect").unwrap(),
                action: Action::BleConnect {
                    role: BleRole::Central,
                    target_address: None,
                    target_name: None,
                },
                timeout_ms: 1_000,
                power_sample: None,
                continue_on_fail: false,
                delay_before_ms: 0,
            })
            .unwrap();

        Study {
            name: heapless::String::try_from("t").unwrap(),
            // `embarch-study-designer/design.md` §3 decision 40: mandatory,
            // with "any" an explicit legal value. These cases are about
            // `steps_crc` and have nothing to say about which builds a study
            // needs, so they say so.
            requires: embarch_study_designer::Requirements::any(),
            steps,
            validations: heapless::Vec::new(),
            streams: heapless::Vec::new(),
            steps_crc: crc,
        }
    }

    #[test]
    fn overwrites_a_missing_or_zero_crc() {
        let mut study = study_with_crc(0);
        recompute_steps_crc(&mut study).unwrap();
        assert_ne!(study.steps_crc, 0);
        assert_eq!(study.steps_crc, steps_crc(&study.steps).unwrap());
    }

    #[test]
    fn overwrites_a_stale_incorrect_crc_too() {
        let mut study = study_with_crc(0xDEAD_BEEF);
        let correct = steps_crc(&study.steps).unwrap();
        assert_ne!(correct, 0xDEAD_BEEF);
        recompute_steps_crc(&mut study).unwrap();
        assert_eq!(study.steps_crc, correct);
    }

    #[test]
    fn recomputation_is_idempotent_on_an_already_correct_crc() {
        let mut study = study_with_crc(0);
        recompute_steps_crc(&mut study).unwrap();
        let first = study.steps_crc;
        recompute_steps_crc(&mut study).unwrap();
        assert_eq!(study.steps_crc, first);
    }

    /// Exercises exactly what `run-study`'s CLI path does with a
    /// hand-authored `--study-file`, minus the actual HTTP call: read the
    /// file, deserialize into `Study`, recompute `steps_crc`. Kept as a
    /// crate-internal unit test rather than a `tests/` integration test
    /// since embarch-api is a bin-only crate with no lib target for an
    /// integration test to import `Study`/`recompute_steps_crc` from.
    #[test]
    fn self_test_fixture_round_trips_end_to_end() {
        // Run on a dedicated, generously-sized stack: `Study` embeds a
        // `heapless::Vec<Step, MAX_STEPS_PER_STUDY>` — a fixed-size *inline*
        // array sized for all 64 slots regardless of how many the fixture
        // actually populates, and `Step`'s `Action` variants are large too
        // (`GattOperation::Write`'s 512-byte payload) — so a debug-profile,
        // unoptimized `serde_json::from_str::<Study>` recurses through
        // sizable stack frames. That overflows libtest's default per-test
        // thread stack (2 MiB) even for this two-step fixture, though not
        // the larger stack a normal process main thread gets on Linux.
        // Worth flagging: the same shape applies to `run_study`/`run-study`
        // parsing a real `--study-file`/MCP payload in production, and a
        // small-default-stack platform (e.g. Windows' 1 MiB) could hit this
        // for real — out of scope to fix in embarch-study-designer itself
        // here, but noted for follow-up.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(run_self_test_fixture_round_trip)
            .expect("failed to spawn test thread")
            .join()
            .expect("self_test_fixture_round_trips_end_to_end body panicked");
    }

    fn run_self_test_fixture_round_trip() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/self_test_study.json"
        ))
        .expect("fixture should be readable");

        let mut study: Study =
            serde_json::from_str(&raw).expect("fixture should match Study's schema");
        assert_eq!(study.name.as_str(), "dev-bench-self-test");
        assert_eq!(study.steps.len(), 2);
        assert_eq!(study.steps[0].name.as_str(), "advertise-short");
        match &study.steps[0].action {
            Action::BleAdvertise { local_name, adv_interval_ms, .. } => {
                assert_eq!(local_name.as_deref(), Some("embarch-selftest"));
                assert_eq!(*adv_interval_ms, 100);
            }
            other => panic!("expected BleAdvertise, got {other:?}"),
        }

        recompute_steps_crc(&mut study).expect("steps_crc should compute cleanly");
        assert_ne!(study.steps_crc, 0);
        assert_eq!(study.steps_crc, steps_crc(&study.steps).unwrap());
    }
}
