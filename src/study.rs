//! Shared helper for the `run_study` MCP tool / `run-study` CLI subcommand
//! (`tools.rs`, `cli.rs`): recomputing all three of a `Study`'s integrity
//! seals immediately before submission to embarch-core.

use embarch_study_designer::{protocols_crc, steps_crc, streams_crc, Study};

/// Why a `Study` couldn't be resealed — which of the three seals failed to
/// compute, rather than one opaque "too large".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResealError {
    Step(embarch_study_designer::StepTooLargeError),
    StreamTap(embarch_study_designer::StreamTapTooLargeError),
    Protocol(embarch_study_designer::ProtocolTooLargeError),
}

impl std::fmt::Display for ResealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResealError::Step(_) => write!(f, "one step's postcard encoding was too large to compute steps_crc over"),
            ResealError::StreamTap(_) => write!(f, "one stream tap's postcard encoding was too large to compute streams_crc over"),
            ResealError::Protocol(_) => write!(f, "one protocol definition's postcard encoding was too large to compute protocols_crc over"),
        }
    }
}

impl std::error::Error for ResealError {}

/// Overwrites `study.steps_crc`, `study.streams_crc` and
/// `study.protocols_crc` with freshly computed values over `study.steps`,
/// `study.streams` and `study.protocols`, regardless of whatever values
/// (including missing/zero ones) were already present in the submitted JSON
/// — `embarch-study-designer/design.md` §3 decision 26: a seal is filled in
/// by whoever *submits* a `Study`, unconditionally, not trusted from the
/// caller. Idempotent: a caller that already computed correct values is
/// unaffected.
///
/// **All three seals.** `streams` got its own in decision 39's 2026-08-25
/// amendment and `protocols` in decision 58 — and this function was not
/// extended past the pair it was first written against, so until 2026-08-27
/// every study carrying a non-empty `Study.protocols` was rejected `400` by
/// Core unless the submitter computed the third seal by hand, which no
/// documented authoring path does. Found by submitting the suite's first
/// real protocol manifest ([embarch-decision-reversals.md] row 76). The
/// lesson generalises past this function: a seal added as a deliberate
/// *sibling* of existing ones has to be added everywhere the set is
/// enumerated, and "both" in a doc comment is the kind of hardcoded arity
/// that silently becomes false.
///
/// They are recomputed independently, exactly as Core checks them
/// independently — that is what lets a failure name which third is at fault.
///
/// Errors only if a single `Step`/`StreamTap`/`ProtocolDef`'s postcard
/// encoding doesn't fit the corresponding scratch buffer — should be
/// unreachable given `embarch-study-designer`'s configured `limits`, but
/// surfaced as an error rather than assumed impossible.
pub fn reseal_study(study: &mut Study) -> Result<(), ResealError> {
    study.steps_crc = steps_crc(&study.steps).map_err(ResealError::Step)?;
    study.streams_crc = streams_crc(&study.streams).map_err(ResealError::StreamTap)?;
    study.protocols_crc = protocols_crc(&study.protocols).map_err(ResealError::Protocol)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::{Action, BleRole};

    fn study_with_crc(crc: u32) -> Study {
        let mut steps = embarch_study_designer::bounded::StepList::new();
        steps
            .push(embarch_study_designer::Step {
                name: heapless::String::try_from("connect").unwrap(),
                action: Action::BleConnect {
                    role: BleRole::Central,
                    target_address: None,
                    target_name: None,
                },
                timeout_ms: 1_000,
                continue_on_fail: false,
                delay_before_ms: 0,
            })
            .unwrap();

        Study {

            decoders: Default::default(),
            name: heapless::String::try_from("t").unwrap(),
            // `embarch-study-designer/design.md` §3 decision 40: mandatory,
            // with "any" an explicit legal value. These cases are about
            // `steps_crc` and have nothing to say about which builds a study
            // needs, so they say so.
            requires: embarch_study_designer::Requirements::any(),
            steps,
            streams: heapless::Vec::new(),
            steps_crc: crc,
            streams_crc: crc,
            // `embarch-study-designer/design.md` §3 decision 58: these cases
            // are about `steps_crc` and run no protocol.
            protocols: Default::default(),
            protocols_crc: 0,
            dev_bench_log_level: Default::default(),
        }
    }

    #[test]
    fn overwrites_a_missing_or_zero_crc() {
        let mut study = study_with_crc(0);
        reseal_study(&mut study).unwrap();
        assert_ne!(study.steps_crc, 0);
        assert_eq!(study.steps_crc, steps_crc(&study.steps).unwrap());
    }

    /// `streams_crc` is overwritten too, not just `steps_crc` — and a study with
    /// no taps reseals to 0, which is the genuine CRC of an empty list
    /// rather than a value left untouched.
    #[test]
    fn overwrites_a_stale_streams_crc_too() {
        let mut study = study_with_crc(0xDEAD_BEEF);
        assert_eq!(study.streams_crc, 0xDEAD_BEEF);
        reseal_study(&mut study).unwrap();
        assert_eq!(study.streams_crc, streams_crc(&study.streams).unwrap());
        assert_eq!(study.streams_crc, 0);
    }

    #[test]
    fn overwrites_a_stale_incorrect_crc_too() {
        let mut study = study_with_crc(0xDEAD_BEEF);
        let correct = steps_crc(&study.steps).unwrap();
        assert_ne!(correct, 0xDEAD_BEEF);
        reseal_study(&mut study).unwrap();
        assert_eq!(study.steps_crc, correct);
    }

    #[test]
    fn recomputation_is_idempotent_on_an_already_correct_crc() {
        let mut study = study_with_crc(0);
        reseal_study(&mut study).unwrap();
        let first = study.steps_crc;
        reseal_study(&mut study).unwrap();
        assert_eq!(study.steps_crc, first);
    }

    /// Exercises exactly what `run-study`'s CLI path does with a
    /// hand-authored `--study-file`, minus the actual HTTP call: read the
    /// file, deserialize into `Study`, reseal both CRCs. Kept as a
    /// crate-internal unit test rather than a `tests/` integration test
    /// since embarch-api is a bin-only crate with no lib target for an
    /// integration test to import `Study`/`reseal_study` from.
    #[test]
    fn self_test_fixture_round_trips_end_to_end() {
        // Kept on a dedicated, generously-sized stack, for a reason that
        // shrank: `embarch-study-designer/design.md` §3 decision 46 replaced
        // `Study.steps`' 64-slot inline array with a heap `Vec` (this crate
        // enables the `alloc` feature by name), which is the "noted for
        // follow-up" this comment used to end on. `Study` is no longer the
        // ~77 KB value it was. What still justifies the big stack is
        // `Step`'s own large `Action` variants, and `StudyResult.steps` --
        // still a 64-slot inline array of ~20 KB `StepResult`s, which is the
        // next instance of exactly this defect.
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

        reseal_study(&mut study).expect("all three seals should compute cleanly");
        assert_ne!(study.steps_crc, 0);
        assert_eq!(study.steps_crc, steps_crc(&study.steps).unwrap());
        assert_eq!(study.streams_crc, streams_crc(&study.streams).unwrap());
        assert_eq!(study.protocols_crc, protocols_crc(&study.protocols).unwrap());
    }

    /// The third seal is recomputed over a **non-empty** `protocols` list.
    ///
    /// This is the case [embarch-decision-reversals.md] row 76 is about, and
    /// the reason it is asserted against a real `ProtocolDef` rather than the
    /// empty default: `protocols_crc(&[])` is 0, and `protocols_crc` was
    /// missing from `reseal_study` entirely — so every test that only ever
    /// resealed a protocol-free study passed against a function that never
    /// touched the field. An empty list cannot distinguish "recomputed to 0"
    /// from "never written", which is exactly how this went unnoticed until
    /// the first study carrying a real manifest was rejected `400` by Core.
    #[test]
    fn overwrites_a_stale_protocols_crc_over_a_real_protocol() {
        let mut study = study_with_crc(0);
        let protocol = embarch_study_designer::eap::ProtocolDef {
            name: heapless::String::try_from("drain").unwrap(),
            sources: heapless::Vec::new(),
            frames: heapless::Vec::new(),
            session: heapless::Vec::new(),
            states: Default::default(),
        };
        study.protocols.push(protocol).unwrap();
        study.protocols_crc = 0xDEAD_BEEF;

        reseal_study(&mut study).unwrap();

        let expected = protocols_crc(&study.protocols).unwrap();
        assert_ne!(expected, 0, "a real protocol must not seal to the empty-list CRC");
        assert_eq!(study.protocols_crc, expected);
    }
}
