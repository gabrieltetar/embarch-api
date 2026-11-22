//! The two interop exports of a Study run (`embarch-api` decision 113;
//! `embarch-study-designer/interfaces/hil-run.md`, "A Study's export and
//! check"): JUnit XML and OCP T&V v2.0 JSON Lines.
//!
//! **Both are pure functions of [`ExportInputs`]**, which is the signed data a
//! zip carries (`events.json`'s step outcomes, `measurements.json`,
//! `report-data.json`): `verify`'s check 7 regenerates them and compares bytes,
//! so nothing here reads a clock, a path or the environment.
//!
//! **A Study has no verdict.** No `<failure>`, no `<skipped>`, no PASS or FAIL
//! `result`: a failed step is an `<error>`, and the run's own status
//! (`completed`, `step_failed`) travels as a property and as metadata.

use serde_json::{json, Value};

/// One step of the run, as `events.json` records it.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub name: String,
    /// `Pass`, `Fail` or `TimedOut`.
    pub outcome: String,
    pub reason: Option<String>,
    pub started_utc_ms: Option<u64>,
    pub ended_utc_ms: Option<u64>,
}

/// Everything an export is made from.
pub struct ExportInputs<'a> {
    /// The study's name, slugged (the suite's name and the OCP run's name).
    pub slug: &'a str,
    pub run_id: &'a str,
    pub report_data: &'a Value,
    /// `measurements.json`, absent when the study declared none.
    pub measurements: Option<&'a Value>,
    pub steps: &'a [StepRow],
}

/// The qualifiers milestone 1 can derive from the signed data, sorted
/// (`hil-shapes.md`'s closed list; the others need data a milestone 1 Core
/// does not produce).
pub fn qualifiers(report_data: &Value, measurements: Option<&Value>) -> Vec<&'static str> {
    let mut q = std::collections::BTreeSet::new();
    let ms = measurements.and_then(|m| m["measurements"].as_array()).cloned().unwrap_or_default();
    for m in &ms {
        if m["bench_image"] == "unverified" {
            q.insert("bench_firmware_unverified");
        }
        if m["by"] == "dut" {
            q.insert("rests_on_dut_report");
        }
        if m["u_source"] == "study" {
            q.insert("declared_by_study");
        }
        if m["declared_by"].as_array().is_some_and(|d| d.iter().any(|x| x["scope"] == "agent")) {
            q.insert("declared_by_agent");
        }
    }
    if report_data["adopted"]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x["attribution"] == "relayed by the agent"))
    {
        q.insert("relayed");
    }
    q.into_iter().collect()
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// A measurement as one line: `value unit ±U (k=…)`, the value `null` when the
/// reducer could not parse one.
pub fn measurement_line(m: &Value) -> String {
    let value = match &m["value"] {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let unit = text(m, "unit");
    let mut out = if unit == "1" || unit.is_empty() { value } else { format!("{value} {unit}") };
    let u = &m["uncertainty"];
    if !u.is_null() {
        out.push_str(&format!(" \u{b1}{} (k={})", u["U"], u["k"]));
    }
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters, even escaped.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

fn property(out: &mut String, name: &str, value: &str) {
    out.push_str(&format!("    <property name=\"{}\" value=\"{}\"/>\n", xml_escape(name), xml_escape(value)));
}

/// The JUnit XML: one `<testsuite>`, one `<testcase>` per step.
pub fn junit_xml(i: &ExportInputs) -> String {
    let errors = i.steps.iter().filter(|s| s.outcome != "Pass").count();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" errors=\"{}\" failures=\"0\" skipped=\"0\">\n",
        xml_escape(i.slug),
        i.steps.len(),
        errors
    ));
    out.push_str("  <properties>\n");
    property(&mut out, "run_id", i.run_id);
    property(&mut out, "run_status", text(i.report_data, "run_status"));
    property(&mut out, "attestation", i.report_data["attestation"]["label"].as_str().unwrap_or(""));
    property(&mut out, "qualifiers", &qualifiers(i.report_data, i.measurements).join(","));
    for m in i.measurements.and_then(|m| m["measurements"].as_array()).into_iter().flatten() {
        let n = text(m, "name");
        property(&mut out, &format!("measurement.{n}"), &measurement_line(m));
        property(&mut out, &format!("measurement.{n}.by"), text(m, "by"));
        property(&mut out, &format!("measurement.{n}.bench_image"), m["bench_image"].as_str().unwrap_or("none"));
        property(&mut out, &format!("measurement.{n}.u_source"), text(m, "u_source"));
        let scopes: Vec<String> = m["declared_by"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| format!("{}:{}", text(d, "what"), text(d, "scope")))
            .collect();
        property(&mut out, &format!("measurement.{n}.declared_by"), &scopes.join(","));
    }
    out.push_str("  </properties>\n");
    for s in i.steps {
        match s.outcome.as_str() {
            "Pass" => out.push_str(&format!("  <testcase name=\"{}\"/>\n", xml_escape(&s.name))),
            other => {
                let kind = if other == "TimedOut" { "step_timed_out" } else { "step_failed" };
                let msg = s.reason.as_deref().unwrap_or(other);
                out.push_str(&format!(
                    "  <testcase name=\"{}\">\n    <error type=\"{kind}\" message=\"{}\"/>\n  </testcase>\n",
                    xml_escape(&s.name),
                    xml_escape(msg)
                ));
            }
        }
    }
    out.push_str("</testsuite>\n");
    out
}

/// A UTC RFC 3339 time with millisecond precision, from epoch milliseconds.
pub fn rfc3339_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        ms % 1000
    )
}

/// Howard Hinnant's days-to-civil conversion.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The OCP T&V v2.0 JSON Lines: a run start, a step artifact per step (start,
/// then its measurements are in the run's measurement records), a measurement
/// record per measurement, and a run end. Times come from the signed data
/// (a step's stamps, a measurement's `at_utc_ms`), `1970-01-01` where there is
/// none, so the file is reproducible.
pub fn ocp_jsonl(i: &ExportInputs) -> String {
    let mut seq = 0u64;
    let mut lines: Vec<Value> = Vec::new();
    let first_ms = i
        .steps
        .iter()
        .filter_map(|s| s.started_utc_ms)
        .min()
        .or_else(|| {
            i.measurements
                .and_then(|m| m["measurements"].as_array())
                .and_then(|a| a.iter().filter_map(|m| m["at_utc_ms"].as_u64()).min())
        })
        .unwrap_or(0);
    let last_ms = i
        .steps
        .iter()
        .filter_map(|s| s.ended_utc_ms)
        .max()
        .unwrap_or(first_ms);
    let status = text(i.report_data, "run_status");
    let quals = qualifiers(i.report_data, i.measurements);
    let mut push = |ts: u64, body: Value| {
        let mut line = json!({ "sequenceNumber": seq, "timestamp": rfc3339_ms(ts) });
        line.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        lines.push(line);
        seq += 1;
    };
    push(
        first_ms,
        json!({ "testRunArtifact": { "testRunStart": {
            "name": i.slug,
            "version": "1",
            "commandLine": "",
            "parameters": {},
            "dutInfo": { "dutInfoId": "dut", "name": i.report_data["subject"]["study_name"] },
            "metadata": {
                "run_id": i.run_id,
                "run_status": status,
                "attestation": i.report_data["attestation"]["label"],
                "qualifiers": quals,
            },
        } } }),
    );
    for (n, s) in i.steps.iter().enumerate() {
        let id = format!("step-{n}");
        let t0 = s.started_utc_ms.unwrap_or(first_ms);
        push(t0, json!({ "testStepArtifact": { "testStepId": id, "testStepStart": { "name": s.name } } }));
        let t1 = s.ended_utc_ms.unwrap_or(t0);
        let st = if s.outcome == "Pass" { "COMPLETE" } else { "ERROR" };
        push(
            t1,
            json!({ "testStepArtifact": { "testStepId": id, "testStepEnd": {
                "status": st,
                "result": "NOT_APPLICABLE",
                "metadata": { "outcome": s.outcome, "reason": s.reason },
            } } }),
        );
    }
    for (n, m) in i.measurements.and_then(|m| m["measurements"].as_array()).into_iter().flatten().enumerate() {
        let at = m["at_utc_ms"].as_u64().unwrap_or(first_ms);
        push(
            at,
            json!({ "testStepArtifact": { "testStepId": format!("measurement-{n}"), "measurement": {
                "name": m["name"],
                "value": m["value"],
                "unit": m["unit"],
                "metadata": {
                    "U": m["uncertainty"]["U"], "k": m["uncertainty"]["k"],
                    "by": m["by"], "bench_image": m["bench_image"], "u_source": m["u_source"],
                    "declared_by": m["declared_by"], "incomplete": m["incomplete"],
                    "attestation": i.report_data["attestation"]["label"],
                    "qualifiers": quals,
                },
            } } }),
        );
    }
    let end = if status == "completed" { "COMPLETE" } else { "ERROR" };
    push(
        last_ms,
        json!({ "testRunArtifact": { "testRunEnd": {
            "status": end,
            "result": "NOT_APPLICABLE",
            "metadata": { "run_status": status },
        } } }),
    );
    let mut out = String::new();
    for l in lines {
        out.push_str(&l.to_string());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd() -> Value {
        json!({ "run_status": "step_failed", "subject": { "study_name": "gatt rate" },
                "attestation": { "label": "agent-scoped only" },
                "adopted": [{ "attribution": "relayed by the agent" }] })
    }

    fn steps() -> Vec<StepRow> {
        vec![
            StepRow { name: "connect".into(), outcome: "Pass".into(), reason: None, started_utc_ms: Some(1_000), ended_utc_ms: Some(2_000) },
            StepRow { name: "read <x>".into(), outcome: "TimedOut".into(), reason: None, started_utc_ms: None, ended_utc_ms: None },
        ]
    }

    #[test]
    fn a_study_export_never_says_failure_or_skipped_or_a_result_of_pass() {
        let rd = rd();
        let st = steps();
        let i = ExportInputs { slug: "gatt-rate", run_id: "r", report_data: &rd, measurements: None, steps: &st };
        let x = junit_xml(&i);
        assert!(x.contains("<error type=\"step_timed_out\""), "{x}");
        assert!(x.contains("name=\"read &lt;x&gt;\""), "{x}");
        assert!(!x.contains("<failure") && !x.contains("<skipped"), "{x}");
        let o = ocp_jsonl(&i);
        assert!(!o.contains("\"PASS\"") && !o.contains("\"FAIL\""), "{o}");
        assert!(o.lines().last().unwrap().contains("\"status\":\"ERROR\""));
        assert_eq!(o, ocp_jsonl(&i), "pure function of its inputs");
    }

    #[test]
    fn times_are_civil_and_utc() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_763_700_000_123), "2025-11-21T04:40:00.123Z");
    }
}
