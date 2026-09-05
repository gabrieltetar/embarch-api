//! The second half of `embarch-doc/embarch-api` decision 27: when a submitted
//! `Study` fails to deserialize, say **which field overflowed and what its
//! limit is**, instead of handing back `serde`'s raw error.
//!
//! Decision 27 moved capacity rejection *here* — this crate holds the file for
//! a `--study-file` submission, so it fails before Core's own field-naming
//! message ever runs. That made the rejection early and the message worse.
//! This closes the gap.
//!
//! **Diagnostic only, never a gate.** [`explain`] runs solely on the error
//! path, after `serde` has already refused the value, so a bound stated wrongly
//! in the table below can only produce a worse message — it can never reject a
//! study `serde` would have accepted. Core's checks remain the authoritative
//! gate for every other caller (decision 27).
//!
//! The table is deliberately partial: the containers and names a hand-author
//! actually writes. Anything it does not cover falls back to `serde`'s error,
//! which is what the caller used to get in every case.

use embarch_study_designer::limits::{
    MAX_DECODERS_PER_STUDY, MAX_DECODER_NAME_LEN, MAX_FIRMWARE_VERSION_LEN, MAX_NAME_LEN,
    MAX_PROTOCOLS_PER_STUDY, MAX_PROTOCOL_NAME_LEN, MAX_STEPS_PER_STUDY, MAX_STREAMS_PER_STUDY,
    MAX_STREAM_NAME_LEN, MAX_STUDY_NAME_LEN,
};
use serde_json::Value;

/// What a caller can actually do about any of these, appended once rather than
/// per overflow. The limits are compile-time buffer sizes shared with
/// dev-bench's firmware, so "raise the limit" is not a per-submission option
/// and saying so saves the caller looking.
const REMEDY: &str = "Shorten or split the study to fit — these are compile-time bounds \
                      shared with dev-bench's fixed-size buffers, and cannot be raised for one \
                      submission.";

/// Names every capacity bound `value` exceeds, or `None` if it exceeds none of
/// the ones checked (in which case the deserialize failure was a schema
/// mismatch, not an overflow, and `serde`'s own error is the better message).
///
/// Every offender is listed, not just the first: a hand-authored study that
/// overflowed one bound has usually overflowed its neighbours too, and one
/// round-trip per field is the thing this decision exists to avoid.
pub fn explain(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut over: Vec<String> = Vec::new();

    string_field(&mut over, obj.get("name"), "name", MAX_STUDY_NAME_LEN);

    if let Some(requires) = obj.get("requires").and_then(Value::as_object) {
        for field in ["dev_bench_version", "firmware_version"] {
            string_field(
                &mut over,
                requires.get(field),
                &format!("requires.{field}"),
                MAX_FIRMWARE_VERSION_LEN,
            );
        }
    }

    for (field, max_entries, max_name) in [
        ("steps", MAX_STEPS_PER_STUDY, MAX_NAME_LEN),
        ("streams", MAX_STREAMS_PER_STUDY, MAX_STREAM_NAME_LEN),
        ("protocols", MAX_PROTOCOLS_PER_STUDY, MAX_PROTOCOL_NAME_LEN),
        ("decoders", MAX_DECODERS_PER_STUDY, MAX_DECODER_NAME_LEN),
    ] {
        let Some(entries) = obj.get(field).and_then(Value::as_array) else {
            continue;
        };
        if entries.len() > max_entries {
            over.push(format!(
                "{field} has {} entries, limit {max_entries}",
                entries.len()
            ));
        }
        for (i, entry) in entries.iter().enumerate() {
            string_field(
                &mut over,
                entry.get("name"),
                &format!("{field}[{i}].name"),
                max_name,
            );
        }
    }

    if over.is_empty() {
        return None;
    }
    Some(format!(
        "{} over embarch-study-designer's capacity limits: {}. {REMEDY}",
        if over.len() == 1 { "one field is" } else { "fields are" },
        over.join("; ")
    ))
}

/// `heapless::String<N>` bounds **bytes**, not characters, so the count
/// reported is a byte count and says so — a name that fits in 32 characters
/// and not in 32 bytes is otherwise an unreadable rejection.
fn string_field(over: &mut Vec<String>, value: Option<&Value>, field: &str, max: usize) {
    let Some(text) = value.and_then(Value::as_str) else {
        return;
    };
    if text.len() > max {
        over.push(format!("{field} is {} bytes, limit {max}", text.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn study_with(steps: usize) -> Value {
        let steps: Vec<Value> = (0..steps)
            .map(|i| {
                json!({
                    "name": format!("s{i}"),
                    "action": { "BleAdvertise": {
                        "local_name": "embarch-selftest",
                        "service_uuids": [],
                        "adv_interval_ms": 100
                    } },
                    "timeout_ms": 1000,
                })
            })
            .collect();
        json!({
            "name": "t",
            "requires": { "dev_bench_version": "any", "firmware_version": "any" },
            "steps": steps,
            "steps_crc": 0,
        })
    }

    /// The case decision 27 is about: 65 steps against a 64-slot list. The
    /// message has to carry the field and the number, since `serde`'s does
    /// not.
    #[test]
    fn names_the_overflowing_list_and_its_limit() {
        let message = explain(&study_with(MAX_STEPS_PER_STUDY + 1)).expect("65 steps is over");
        assert!(message.contains("steps has 65 entries"), "{message}");
        assert!(message.contains("limit 64"), "{message}");
        assert!(message.contains("cannot be raised for one submission"), "{message}");
    }

    /// A study that fits says nothing, so the caller keeps `serde`'s error for
    /// a failure that was never about capacity. This is what keeps the module
    /// diagnostic rather than a second gate.
    #[test]
    fn a_study_within_every_bound_explains_nothing() {
        assert_eq!(explain(&study_with(MAX_STEPS_PER_STUDY)), None);
    }

    /// Byte counts, not character counts, because that is what
    /// `heapless::String<N>` bounds.
    #[test]
    fn an_over_long_name_is_reported_in_bytes() {
        let mut study = study_with(1);
        study["name"] = json!("é".repeat(MAX_STUDY_NAME_LEN / 2 + 1));
        let message = explain(&study).expect("66 bytes is over a 64-byte bound");
        assert!(message.contains("name is 66 bytes, limit 64"), "{message}");
    }

    /// A nested name names its own index, so a 64-step study says which step.
    #[test]
    fn a_nested_name_is_reported_with_its_index() {
        let mut study = study_with(3);
        study["steps"][2]["name"] = json!("x".repeat(MAX_NAME_LEN + 1));
        let message = explain(&study).expect("a 33-byte step name is over");
        assert!(message.contains("steps[2].name is 33 bytes, limit 32"), "{message}");
    }

    /// Every offender, not the first — the round-trip-per-field this exists to
    /// avoid.
    #[test]
    fn every_overflowing_field_is_listed_at_once() {
        let mut study = study_with(MAX_STEPS_PER_STUDY + 1);
        study["name"] = json!("n".repeat(MAX_STUDY_NAME_LEN + 1));
        study["requires"]["firmware_version"] = json!("v".repeat(MAX_FIRMWARE_VERSION_LEN + 1));
        let message = explain(&study).expect("three fields are over");
        assert!(message.starts_with("fields are over"), "{message}");
        assert!(message.contains("name is 65 bytes, limit 64"), "{message}");
        assert!(
            message.contains("requires.firmware_version is 33 bytes, limit 32"),
            "{message}"
        );
        assert!(message.contains("steps has 65 entries, limit 64"), "{message}");
    }

    /// The premise, asserted rather than assumed: `serde` really does refuse a
    /// 65-step study (`Bounded`'s `Deserialize` enforces `N` under `alloc`
    /// too), it really does so without naming `steps` or `64`, and `explain`
    /// really does turn that same value into a message that names both. If the
    /// first of those ever stopped holding, every other test here would still
    /// pass against a path nothing reaches.
    #[test]
    fn serde_refuses_the_oversized_study_and_says_neither_field_nor_limit() {
        let value = study_with(MAX_STEPS_PER_STUDY + 1);
        let raw = serde_json::to_string(&value).unwrap();
        let err = serde_json::from_str::<embarch_study_designer::Study>(&raw)
            .expect_err("65 steps must not deserialize");
        let raw_message = err.to_string();
        assert!(!raw_message.contains("steps"), "{raw_message}");
        assert!(!raw_message.contains("64"), "{raw_message}");

        let ours = explain(&value).expect("and this is what the caller gets instead");
        assert!(ours.contains("steps has 65 entries, limit 64"), "{ours}");

        // The study one step smaller is accepted, so the refusal is the
        // capacity bound and not something else about the fixture.
        let ok = serde_json::to_string(&study_with(MAX_STEPS_PER_STUDY)).unwrap();
        serde_json::from_str::<embarch_study_designer::Study>(&ok)
            .expect("64 steps is exactly the limit");
    }

    /// Not an object — a study sent as an array or a bare string — is a schema
    /// failure, and `serde`'s message for it is already the right one.
    #[test]
    fn a_non_object_explains_nothing() {
        assert_eq!(explain(&json!([1, 2, 3])), None);
        assert_eq!(explain(&json!("study")), None);
    }
}
