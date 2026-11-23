//! EMB-228: the HIL guide's Study section walks a BLE GATT data-acquisition
//! study, and `tests/fixtures/ble_gatt_rate_study.json` is that study. The
//! guide is the one place an agent learns to write one, so the example must
//! be a study Core would accept, not a sketch that drifts from the types.
//!
//! This runs the pre-flight rules Core's `validate_study` runs, each computed
//! by `embarch-study-designer`, over the fixture. The seal is the one rule
//! left out: `run-study` and the `run_study` tool reseal a study before they
//! submit it, so the fixture carries `steps_crc: 0` as every saved example does.
//! The fixture is never submitted to Core. Its address and UUIDs are
//! placeholders for the DUT's own, and its `seq` field stands for a counter
//! the DUT's packet may or may not carry.

use embarch_study_designer::{validate_measurements, validate_record_checks, validate_taps, Action, Study};

fn ble_gatt_rate_study() -> Study {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ble_gatt_rate_study.json"
    ))
    .expect("the BLE GATT rate fixture should be readable");
    serde_json::from_str(&raw).expect("the BLE GATT rate fixture should match Study's schema")
}

#[test]
fn ble_gatt_rate_study_passes_cores_preflight_rules() {
    let study = ble_gatt_rate_study();
    validate_taps(&study.streams, study.steps.len() as u32, study.decoders.len()).expect("taps");
    validate_record_checks(&study.record_checks).expect("record checks");
    validate_measurements(&study).expect("measurement declarations");
    study.requires.validate().expect("requires");
    embarch_study_designer::study::outpost_requirement_is_satisfiable(&study.requires, &study.streams)
        .expect("outpost requirement");
    study.validate_wait_budgets().expect("wait budgets");
    study.validate_monitor_windows().expect("monitor windows");
}

#[test]
fn ble_gatt_rate_study_connects_by_address_then_monitors_one_characteristic() {
    let study = ble_gatt_rate_study();
    assert_eq!(study.steps.len(), 2);
    assert!(matches!(
        &study.steps[0].action,
        Action::BleConnect { target_address: Some(_), .. }
    ));
    assert!(matches!(&study.steps[1].action, Action::GattMonitorSelected { targets } if targets.len() == 1));
    let names: Vec<&str> = study.measurements.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["notifications", "rate", "first_seq", "last_seq"]);
}

/// The guide's prose example, scanned by the same oracle `publish` runs: its
/// two numbers cite their measurements and the third carries a tag, so none
/// is listed as untagged or unbound in the appendix.
#[cfg(feature = "pdf")]
#[test]
fn the_guides_prose_example_cites_and_tags_every_number() {
    use embarch_api::publish_oracle::{rows, Kind, Source};
    let prose = "The DUT sent 2994 notifications [notifications] at 49.9 Hz [rate].\n\
                 Missing: 6 (derived), from last_seq minus first_seq plus one, less notifications.\n";
    let measurements = serde_json::json!({"measurements": [
        {"name": "notifications", "value": 2994.0, "unit": "1"},
        {"name": "rate", "value": 49.93, "unit": "Hz"},
        {"name": "first_seq", "value": 1.0, "unit": "1"},
        {"name": "last_seq", "value": 3000.0, "unit": "1"}
    ]});
    let found = rows(
        &[Source { name: "run.md", text: prose, typst: false }],
        Some(&measurements),
        &serde_json::json!({}),
    );
    let kinds: Vec<(String, Kind)> = found.iter().map(|r| (r.text.clone(), r.kind)).collect();
    assert_eq!(kinds.len(), 3, "{kinds:?}");
    assert_eq!(kinds[0].1, Kind::Cited, "{kinds:?}");
    assert_eq!(kinds[1].1, Kind::Cited, "{kinds:?}");
    assert_eq!(kinds[2].1, Kind::Tagged, "{kinds:?}");
}

/// The guide carries the study inline, since an adopter's agent has the
/// binary and not this repo; that copy is this fixture, value for value.
#[test]
fn the_guides_copy_of_the_study_is_the_fixture() {
    let guide = embarch_api_guide_text();
    let study = guide.split("## Study").nth(1).expect("the guide has a Study section");
    let block = study.split("```json\n").nth(1).and_then(|b| b.split("\n```").next())
        .expect("the Study section carries the example as a json block");
    let fixture = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ble_gatt_rate_study.json"))
        .unwrap();
    let a: serde_json::Value = serde_json::from_str(block).expect("the guide's example parses");
    let b: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    assert_eq!(a, b, "the guide's example study and the fixture have drifted");
}

fn embarch_api_guide_text() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/guide/hil-guide.md")).unwrap()
}
