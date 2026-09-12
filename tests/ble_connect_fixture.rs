//! `tasks/api/076`: `self_test_study.json` is two `BleAdvertise` steps and is
//! deliberately never changed to add a step that connects
//! (`self_test_study.json`'s own header comment in `tests/core_client_http.rs`
//! explains why: it is consumed by committed tests and the one study this
//! fleet has actually run). That leaves `studies-guide.md` §3b's advice —
//! set `target_address` or `target_name` on a `BleConnect` step — with no
//! authored example anywhere in the tree.
//!
//! This fixture is that example. It is never submitted to Core and backs
//! no other test; its only job is to round-trip through `Study` so it
//! cannot silently drift from the type it's meant to demonstrate.

use embarch_study_designer::{Action, Study};

/// `tests/fixtures/ble_connect_worked_example.json`, deserialized.
///
/// Shows the `target_address` form of `studies-guide.md` §3b's advice: a
/// `BleConnect` step with an explicit `BleAddress` (six bytes plus
/// `Public`/`Random` kind) and `target_name: null`.
///
/// The other form the guide names — `target_name`, matching an advertised
/// local name exactly (`embarch-study-designer` decision 43) — is not a
/// second step here; it is the same field on the same action with
/// `target_address` set to `null` instead, e.g.:
/// ```json
/// "action": { "BleConnect": { "role": "Central", "target_address": null, "target_name": "my-dut" } }
/// ```
fn ble_connect_worked_example() -> Study {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ble_connect_worked_example.json"
    ))
    .expect("the BleConnect worked-example fixture should be readable");
    serde_json::from_str(&raw)
        .expect("the BleConnect worked-example fixture should match Study's schema")
}

#[test]
fn ble_connect_worked_example_round_trips_and_sets_target_address() {
    let study = ble_connect_worked_example();
    assert_eq!(study.steps.len(), 1);
    let step = &study.steps[0];
    match &step.action {
        Action::BleConnect {
            target_address,
            target_name,
            ..
        } => {
            assert!(
                target_address.is_some(),
                "the worked example is the target_address form of the advice; \
                 an unset target_address here would defeat the point of the fixture"
            );
            assert!(target_name.is_none());
        }
        other => panic!("expected a BleConnect step, got {other:?}"),
    }
}
