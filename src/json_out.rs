//! The one place a `--json` value becomes text.
//!
//! `embarch-doc/embarch-api/decisions.md` decision 24 promised
//! `schema_version` on every `--json` object "from the start rather than
//! after a consumer depends on an unversioned shape". It was never built:
//! from the first commit until 2026-09-03 the string did not appear in this
//! crate at all, while [`interfaces/tools.md`] told a caller to read it.
//! Decision 50 is the fix, and this module is the mechanism.
//!
//! The rule it enforces: **every JSON object this crate emits — CLI
//! `--json`, each NDJSON line of `study-status --follow`, and every MCP tool
//! result whose content is JSON — carries `schema_version`.** That is a
//! stronger promise than the doc's "the `--json` object", deliberately: a
//! reader of a live NDJSON feed has to know the shape from the first line,
//! not from the `summary` line that arrives after the study finishes.
//!
//! It is enforced structurally rather than by convention. `stamped` is not
//! optional — the only functions here that produce text call it, and
//! `cli.rs`'s own `no_json_reaches_stdout_except_through_json_out` test
//! fails if `cli.rs` or `tools.rs` grows a JSON serializer of its own. A
//! new emitter cannot forget the field without either routing through here
//! or breaking that test.

use serde_json::{Map, Value};

/// The version stamped on every object this module emits.
///
/// **`1` means "the shape as of 2026-09-03"**, not "unchanged since this
/// crate's first commit" — the surface moved repeatedly while unversioned,
/// and no honest earlier number exists. Bumped **by hand only**, and only
/// on a rename, a removal or a retype of a field (decision 24); adding a
/// field is not a bump, since a reader that ignores unknown fields is
/// unaffected.
pub const SCHEMA_VERSION: u32 = 1;

/// The field name, in one place so the tests and the stamper cannot drift.
pub const SCHEMA_VERSION_FIELD: &str = "schema_version";

/// Adds (or overwrites) `schema_version` on `value`.
///
/// A non-object is wrapped as `{"schema_version": N, "value": …}` rather
/// than passed through, so the promise is total instead of "total for the
/// shapes that happen to exist today". Nothing in this surface emits a
/// non-object — every emitter builds a `serde_json::json!({…})` — so the
/// wrap is a guard against a future one, not a shape any caller sees.
pub fn stamped(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.insert(SCHEMA_VERSION_FIELD.to_string(), Value::from(SCHEMA_VERSION));
            Value::Object(map)
        }
        other => {
            let mut map = Map::new();
            map.insert(SCHEMA_VERSION_FIELD.to_string(), Value::from(SCHEMA_VERSION));
            map.insert("value".to_string(), other);
            Value::Object(map)
        }
    }
}

/// One stamped object, pretty-printed: the single-object `--json` form every
/// subcommand but `study-status --follow` uses, and every MCP tool result
/// whose content is JSON.
pub fn pretty(value: Value) -> String {
    serde_json::to_string_pretty(&stamped(value)).unwrap_or_else(serialize_failure)
}

/// One stamped object, compact and newline-free: one NDJSON record of
/// `study-status --follow` (decision 47).
pub fn line(value: Value) -> String {
    serde_json::to_string(&stamped(value)).unwrap_or_else(serialize_failure)
}

/// The same value rendered for a **human** reader, deliberately *not*
/// stamped: `list-targets`' human mode prints its result as indented JSON,
/// which is text for a person, not the `--json` surface a script parses.
///
/// It lives here anyway so that `to_string_pretty` appears in exactly one
/// file — which is what makes the guard test a simple grep rather than a
/// judgement about which call site was which.
pub fn human_render(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Hand-built rather than serialized, because the serializer is what just
/// failed. Carries `schema_version` like everything else, so a caller
/// parsing this last-resort object is not handed the one shape that lacks
/// the field it was told to read.
fn serialize_failure(e: serde_json::Error) -> String {
    let escaped = e.to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{{\"{SCHEMA_VERSION_FIELD}\": {SCHEMA_VERSION}, \"success\": false, \
         \"error\": \"failed to serialize result: {escaped}\"}}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_version_of(rendered: &str) -> Option<u64> {
        serde_json::from_str::<Value>(rendered)
            .ok()?
            .get(SCHEMA_VERSION_FIELD)?
            .as_u64()
    }

    #[test]
    fn stamps_an_object() {
        let value = stamped(serde_json::json!({ "success": true, "projects": [] }));
        assert_eq!(
            value[SCHEMA_VERSION_FIELD],
            serde_json::json!(SCHEMA_VERSION)
        );
        // The payload is untouched — stamping adds, it does not rebuild.
        assert_eq!(value["success"], serde_json::json!(true));
        assert_eq!(value["projects"], serde_json::json!([]));
    }

    #[test]
    fn overwrites_a_stale_schema_version_rather_than_trusting_the_caller() {
        // An emitter that hand-wrote the field (or a Core response that
        // carried its own, differently-meaning `schema_version` — Core's
        // `/dev-bench/hello` has one) must not be able to publish a version
        // this crate did not mean.
        let value = stamped(serde_json::json!({ "schema_version": 99, "success": true }));
        assert_eq!(
            value[SCHEMA_VERSION_FIELD],
            serde_json::json!(SCHEMA_VERSION)
        );
    }

    #[test]
    fn wraps_a_non_object_rather_than_dropping_the_stamp() {
        let value = stamped(serde_json::json!([1, 2, 3]));
        assert_eq!(
            value[SCHEMA_VERSION_FIELD],
            serde_json::json!(SCHEMA_VERSION)
        );
        assert_eq!(value["value"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn pretty_carries_the_stamp() {
        let rendered = pretty(serde_json::json!({ "success": true }));
        assert_eq!(schema_version_of(&rendered), Some(SCHEMA_VERSION as u64));
    }

    #[test]
    fn line_carries_the_stamp_and_stays_one_line() {
        // NDJSON's whole contract: one record, one line. A pretty-printed
        // record here would silently corrupt the feed for a line-oriented
        // reader.
        let rendered = line(serde_json::json!({ "type": "summary", "success": true }));
        assert!(!rendered.contains('\n'), "NDJSON record spans lines: {rendered}");
        assert_eq!(schema_version_of(&rendered), Some(SCHEMA_VERSION as u64));
    }

    #[test]
    fn human_render_is_not_stamped() {
        let rendered = human_render(&serde_json::json!({ "targets": [] }));
        assert!(
            !rendered.contains(SCHEMA_VERSION_FIELD),
            "human text picked up the machine surface's version stamp: {rendered}"
        );
    }

    #[test]
    fn the_serialize_failure_fallback_is_itself_valid_stamped_json() {
        // Reached only when serialization fails, which is exactly when a
        // hand-built string is easiest to get wrong. Pin it.
        let rendered = serialize_failure(serde_json::from_str::<Value>("{").unwrap_err());
        assert_eq!(schema_version_of(&rendered), Some(SCHEMA_VERSION as u64));
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["success"], serde_json::json!(false));
    }
}
