//! The two interop exports of a Test run (`embarch-api` decision 145;
//! `notes/hil/06-report-and-publish.md`, "Publish"): JUnit XML and OCP T&V v2.0
//! JSON Lines, mapped from each test's verdict by that note's table.
//!
//! **Both are pure functions of [`TestExportInputs`]**, which is
//! `report-data.json` and the `measurements.json` of each study run the zip
//! carries, so `verify` can regenerate them byte for byte. Nothing here reads a
//! clock, a path or the environment. A Study's exports are `publish_export`'s
//! and are not touched.
//!
//! Where 06 leaves a choice open: a measurement's `U` and `k` are read from its
//! `uncertainty` as a Study's are; the OCP words are `COMPLETE`/`ERROR`/`SKIP`
//! for status and `PASS`/`FAIL`/`NOT_APPLICABLE` for result; and a skipped test
//! is a gap unless its reason is `not_applicable` or `not_selected`, so a
//! reason this crate does not know fails a CI gate rather than passing it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::publish_export::{measurement_line, property, qualifiers, rfc3339_ms, xml_escape};

/// Everything a Test run's export is made from.
pub struct TestExportInputs<'a> {
    pub report_data: &'a Value,
    /// Each study run's `measurements.json` by study id; a study that declared
    /// none is absent.
    pub measurements: &'a BTreeMap<String, Value>,
}

/// A test's verdict as the exports see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapped {
    Pass,
    Fail,
    Error,
    Inconclusive,
    Cancelled,
    /// Skipped for a gap.
    NotVerified,
    /// Skipped as not applicable or not selected.
    Skipped,
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn map_verdict(verdict: &Value, reason: &Value) -> Mapped {
    match verdict.as_str().unwrap_or("skipped") {
        "pass" => Mapped::Pass,
        "fail" => Mapped::Fail,
        "error" => Mapped::Error,
        "inconclusive" => Mapped::Inconclusive,
        "cancelled" => Mapped::Cancelled,
        _ => match reason.as_str() {
            Some("not_applicable" | "not_selected") => Mapped::Skipped,
            _ => Mapped::NotVerified,
        },
    }
}

/// OCP status and result for a mapped verdict (06's table).
fn ocp_words(m: Mapped) -> (&'static str, &'static str) {
    match m {
        Mapped::Pass => ("COMPLETE", "PASS"),
        Mapped::Fail => ("COMPLETE", "FAIL"),
        Mapped::Skipped => ("SKIP", "NOT_APPLICABLE"),
        _ => ("ERROR", "NOT_APPLICABLE"),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

/// A test as both exports read it.
struct Case<'a> {
    id: &'a str,
    bench: &'a str,
    mapped: Mapped,
    verdict: &'a str,
    reason: Option<&'a str>,
    requirements: Vec<String>,
    /// Requirements this test is under whose roll-up is `not_verified`.
    not_verified: Vec<String>,
    /// `requirement id -> "N of M tests run"` where the roll-up says so.
    n_of_m: BTreeMap<String, String>,
    qualifiers: Vec<String>,
    measurements: Vec<&'a Value>,
    t: &'a Value,
}

fn cases<'a>(i: &'a TestExportInputs) -> Vec<Case<'a>> {
    let reqs = i.report_data["requirements"].as_object();
    i.report_data["tests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let id = text(t, "test_id");
            let mut requirements = strings(&t["requirements"]);
            let mut not_verified = Vec::new();
            let mut n_of_m = BTreeMap::new();
            let mut quals: BTreeSet<String> = strings(&t["qualifiers"]).into_iter().collect();
            for (rid, r) in reqs.into_iter().flatten() {
                if !strings(&r["tests"]).iter().any(|x| x == id) {
                    continue;
                }
                if !requirements.contains(rid) {
                    requirements.push(rid.clone());
                }
                if r["verdict"] == "not_verified" {
                    not_verified.push(rid.clone());
                }
                if let (Some(n), Some(m)) = (r["n_of_m"]["n"].as_u64(), r["n_of_m"]["m"].as_u64()) {
                    n_of_m.insert(rid.clone(), format!("{n} of {m} tests run"));
                }
                quals.extend(strings(&r["qualifiers"]));
            }
            requirements.sort();
            let measurements: Vec<&Value> = strings(&t["study_ids"])
                .iter()
                .filter_map(|s| i.measurements.get(s))
                .flat_map(|m| m["measurements"].as_array().into_iter().flatten())
                .collect();
            for m in &measurements {
                let wrapper = json!({ "measurements": [m] });
                quals.extend(qualifiers(&Value::Null, Some(&wrapper)).into_iter().map(str::to_string));
            }
            Case {
                id,
                bench: t["bench"].as_str().unwrap_or(""),
                mapped: map_verdict(&t["verdict"], &t["reason"]),
                verdict: t["verdict"].as_str().unwrap_or("skipped"),
                reason: t["reason"].as_str(),
                requirements,
                not_verified,
                n_of_m,
                qualifiers: quals.into_iter().collect(),
                measurements,
                t,
            }
        })
        .collect()
}

fn run_qualifiers(rd: &Value) -> String {
    strings(&rd["summary"]["qualifiers"]).join(",")
}

fn case_properties(out: &mut String, c: &Case) {
    out.push_str("    <properties>\n");
    property(out, "verdict", c.verdict);
    property(out, "reason", c.reason.unwrap_or(""));
    property(out, "definition_hash", text(c.t, "definition_hash"));
    property(out, "dut_image", c.t["dut_image"].as_str().unwrap_or(""));
    property(out, "requirements", &c.requirements.join(","));
    property(out, "not_verified_requirements", &c.not_verified.join(","));
    for (r, n) in &c.n_of_m {
        property(out, &format!("n_of_m.{r}"), n);
    }
    property(out, "qualifiers", &c.qualifiers.join(","));
    for m in &c.measurements {
        let n = text(m, "name");
        property(out, &format!("measurement.{n}"), &measurement_line(m));
        property(out, &format!("measurement.{n}.U"), &m["uncertainty"]["U"].to_string());
        property(out, &format!("measurement.{n}.by"), text(m, "by"));
        property(out, &format!("measurement.{n}.bench_image"), m["bench_image"].as_str().unwrap_or("none"));
        property(out, &format!("measurement.{n}.u_source"), text(m, "u_source"));
    }
    out.push_str("    </properties>\n");
}

/// The JUnit XML: one `<testsuite>` per bench, one `<testcase>` per test.
pub fn junit_xml(i: &TestExportInputs) -> String {
    let rd = i.report_data;
    let all = cases(i);
    let mut benches: BTreeMap<&str, Vec<&Case>> = BTreeMap::new();
    for c in &all {
        benches.entry(c.bench).or_default().push(c);
    }
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n");
    for (bench, cs) in &benches {
        let n = |f: &dyn Fn(Mapped) -> bool| cs.iter().filter(|c| f(c.mapped)).count();
        let failures = n(&|m| matches!(m, Mapped::Fail | Mapped::Inconclusive));
        let errors = n(&|m| matches!(m, Mapped::Error | Mapped::Cancelled | Mapped::NotVerified));
        let skipped = n(&|m| m == Mapped::Skipped);
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" errors=\"{errors}\" failures=\"{failures}\" skipped=\"{skipped}\">\n",
            xml_escape(bench),
            cs.len()
        ));
        out.push_str("    <properties>\n");
        property(&mut out, "run_id", text(rd, "run_id"));
        property(&mut out, "revision", &rd["revision"].to_string());
        property(&mut out, "run_status", text(rd, "run_status"));
        property(&mut out, "suite", text(&rd["suite"], "id"));
        property(&mut out, "suite_version", text(&rd["suite"], "version"));
        property(&mut out, "unit_key", rd["unit"]["unit_key"].as_str().unwrap_or(""));
        property(&mut out, "verdict", text(&rd["summary"], "verdict"));
        property(&mut out, "attestation", rd["attestation"]["label"].as_str().unwrap_or(""));
        property(&mut out, "qualifiers", &run_qualifiers(rd));
        out.push_str("    </properties>\n");
        for c in cs {
            let child = match c.mapped {
                Mapped::Pass => None,
                Mapped::Fail => Some("failure".to_string()),
                Mapped::Error => Some("error".to_string()),
                Mapped::Inconclusive => Some("failure type=\"inconclusive\"".to_string()),
                Mapped::Cancelled => Some("error type=\"cancelled\"".to_string()),
                Mapped::NotVerified => Some("error type=\"not_verified\"".to_string()),
                Mapped::Skipped => Some("skipped".to_string()),
            };
            out.push_str(&format!("    <testcase name=\"{}\" classname=\"{}\">\n", xml_escape(c.id), xml_escape(bench)));
            if let Some(tag) = child {
                out.push_str(&format!("      <{tag} message=\"{}\"/>\n", xml_escape(c.reason.unwrap_or(c.verdict))));
            }
            case_properties(&mut out, c);
            out.push_str("    </testcase>\n");
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

/// The OCP T&V v2.0 JSON Lines: a run start, a step per test holding its
/// measurements, a run end. Times come from the measurements' `at_utc_ms`
/// (`1970-01-01` where there is none), so the file is reproducible.
pub fn ocp_jsonl(i: &TestExportInputs) -> String {
    let rd = i.report_data;
    let all = cases(i);
    let mut seq = 0u64;
    let mut lines: Vec<Value> = Vec::new();
    let first_ms =
        all.iter().flat_map(|c| c.measurements.iter()).filter_map(|m| m["at_utc_ms"].as_u64()).min().unwrap_or(0);
    let mut push = |ts: u64, body: Value| {
        let mut line = json!({ "sequenceNumber": seq, "timestamp": rfc3339_ms(ts) });
        line.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        lines.push(line);
        seq += 1;
    };
    let quals = strings(&rd["summary"]["qualifiers"]);
    push(
        first_ms,
        json!({ "testRunArtifact": { "testRunStart": {
            "name": rd["suite"]["id"],
            "version": rd["suite"]["version"],
            "commandLine": "",
            "parameters": {},
            "dutInfo": { "dutInfoId": "dut", "name": rd["unit"]["unit_key"] },
            "metadata": {
                "run_id": rd["run_id"], "revision": rd["revision"], "run_status": rd["run_status"],
                "verdict": rd["summary"]["verdict"], "attestation": rd["attestation"]["label"], "qualifiers": quals,
            },
        } } }),
    );
    let mut cursor = first_ms;
    for (n, c) in all.iter().enumerate() {
        let id = format!("step-{n}");
        let at: Vec<u64> = c.measurements.iter().filter_map(|m| m["at_utc_ms"].as_u64()).collect();
        let t0 = at.iter().copied().min().unwrap_or(cursor).max(cursor);
        push(
            t0,
            json!({ "testStepArtifact": { "testStepId": id, "testStepStart": { "name": c.id } } }),
        );
        for m in &c.measurements {
            let ts = m["at_utc_ms"].as_u64().unwrap_or(t0).max(t0);
            push(
                ts,
                json!({ "testStepArtifact": { "testStepId": id, "measurement": {
                    "name": m["name"], "value": m["value"], "unit": m["unit"],
                    "metadata": {
                        "U": m["uncertainty"]["U"], "k": m["uncertainty"]["k"],
                        "by": m["by"], "bench_image": m["bench_image"], "u_source": m["u_source"],
                        "declared_by": m["declared_by"], "incomplete": m["incomplete"],
                        "qualifiers": c.qualifiers,
                    },
                } } }),
            );
        }
        let t1 = at.iter().copied().max().unwrap_or(t0).max(t0);
        cursor = t1;
        let (status, result) = ocp_words(c.mapped);
        let mut meta = json!({
            "verdict": c.verdict, "reason": c.reason, "bench": c.bench, "requirements": c.requirements,
            "not_verified_requirements": c.not_verified, "n_of_m": c.n_of_m, "qualifiers": c.qualifiers,
        });
        if c.mapped == Mapped::NotVerified {
            meta["not_verified"] = json!(true);
        }
        push(t1, json!({ "testStepArtifact": { "testStepId": id, "testStepEnd": { "status": status, "result": result, "metadata": meta } } }));
    }
    let gap = all.iter().any(|c| c.mapped == Mapped::NotVerified);
    let (status, result) = if text(rd, "run_status") != "completed" {
        ("ERROR", "NOT_APPLICABLE")
    } else {
        match text(&rd["summary"], "verdict") {
            "pass" => ("COMPLETE", "PASS"),
            "fail" => ("COMPLETE", "FAIL"),
            "skipped" if !gap => ("SKIP", "NOT_APPLICABLE"),
            _ => ("ERROR", "NOT_APPLICABLE"),
        }
    };
    push(
        cursor,
        json!({ "testRunArtifact": { "testRunEnd": {
            "status": status, "result": result,
            "metadata": { "run_status": rd["run_status"], "verdict": rd["summary"]["verdict"], "not_verified": gap },
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
        json!({
            "run_id": "r1", "revision": 1, "run_status": "completed", "suite": { "id": "usb-pd", "version": "1.2" },
            "unit": { "unit_key": "hw-42" }, "attestation": { "label": "agent-scoped only" },
            "summary": { "verdict": "fail", "qualifiers": ["unit_identity_not_read"] },
            "requirements": {
                "R1": { "verdict": "not_verified", "qualifiers": [], "n_of_m": null, "tests": ["gap", "na"] },
                "R2": { "verdict": "pass", "qualifiers": ["n_of_m_run"], "n_of_m": { "n": 3, "m": 5 }, "tests": ["ok"] },
            },
            "tests": [
                { "test_id": "ok", "bench": "b1", "verdict": "pass", "reason": null, "requirements": ["R2"],
                  "qualifiers": ["bench_firmware_unreleased"], "study_ids": ["s1"] },
                { "test_id": "bad", "bench": "b1", "verdict": "fail", "reason": "limit", "requirements": [], "qualifiers": [], "study_ids": [] },
                { "test_id": "err", "bench": "b1", "verdict": "error", "reason": "port_lost", "requirements": [], "qualifiers": [], "study_ids": [] },
                { "test_id": "inc", "bench": "b2", "verdict": "inconclusive", "reason": "guard_band", "requirements": [], "qualifiers": [], "study_ids": [] },
                { "test_id": "can", "bench": "b2", "verdict": "cancelled", "reason": null, "requirements": [], "qualifiers": [], "study_ids": [] },
                { "test_id": "gap", "bench": "b2", "verdict": "skipped", "reason": "requires_not_met", "requirements": ["R1"], "qualifiers": [], "study_ids": [] },
                { "test_id": "na", "bench": "b2", "verdict": "skipped", "reason": "not_applicable", "requirements": ["R1"], "qualifiers": [], "study_ids": [] },
            ],
        })
    }

    fn ms() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "s1".to_string(),
            json!({ "measurements": [{ "name": "rate", "value": 12.5, "unit": "Hz", "uncertainty": { "U": 0.5, "k": 2 },
                "by": "dut", "bench_image": "unverified", "u_source": "study", "declared_by": [], "at_utc_ms": 5000 }] }),
        );
        m
    }

    fn case_xml(x: &str, name: &str) -> String {
        let start = x.find(&format!("<testcase name=\"{name}\"")).unwrap();
        x[start..x[start..].find("</testcase>").unwrap() + start].to_string()
    }

    fn end_of(o: &str, step: &str) -> Value {
        let l: Vec<Value> = o.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let name_line = l.iter().position(|v| v["testStepArtifact"]["testStepStart"]["name"] == step).unwrap();
        let id = l[name_line]["testStepArtifact"]["testStepId"].clone();
        l.into_iter().find(|v| v["testStepArtifact"]["testStepId"] == id && v["testStepArtifact"]["testStepEnd"].is_object()).unwrap()
            ["testStepArtifact"]["testStepEnd"]
            .clone()
    }

    #[test]
    fn each_row_of_the_table_maps_to_its_junit_element_and_ocp_words() {
        let (rd, m) = (rd(), ms());
        let i = TestExportInputs { report_data: &rd, measurements: &m };
        let x = junit_xml(&i);
        let rows = [
            ("ok", None, "COMPLETE", "PASS"),
            ("bad", Some("<failure message=\"limit\"/>"), "COMPLETE", "FAIL"),
            ("err", Some("<error message=\"port_lost\"/>"), "ERROR", "NOT_APPLICABLE"),
            ("inc", Some("<failure type=\"inconclusive\" message=\"guard_band\"/>"), "ERROR", "NOT_APPLICABLE"),
            ("can", Some("<error type=\"cancelled\" message=\"cancelled\"/>"), "ERROR", "NOT_APPLICABLE"),
            ("gap", Some("<error type=\"not_verified\" message=\"requires_not_met\"/>"), "ERROR", "NOT_APPLICABLE"),
            ("na", Some("<skipped message=\"not_applicable\"/>"), "SKIP", "NOT_APPLICABLE"),
        ];
        let o = ocp_jsonl(&i);
        for (name, child, status, result) in rows {
            let c = case_xml(&x, name);
            match child {
                Some(e) => assert!(c.contains(e), "{name}: {c}"),
                None => assert!(!c.contains("<failure") && !c.contains("<error") && !c.contains("<skipped"), "{name}: {c}"),
            }
            let end = end_of(&o, name);
            assert_eq!((end["status"].as_str().unwrap(), end["result"].as_str().unwrap()), (status, result), "{name}");
        }
        assert_eq!(end_of(&o, "gap")["metadata"]["not_verified"], true);
        assert_eq!(end_of(&o, "inc")["metadata"]["verdict"], "inconclusive");
        assert_eq!(end_of(&o, "can")["metadata"]["verdict"], "cancelled");
    }

    #[test]
    fn one_testsuite_per_bench_and_a_requirements_not_verified_rides_each_of_its_tests() {
        let (rd, m) = (rd(), ms());
        let x = junit_xml(&TestExportInputs { report_data: &rd, measurements: &m });
        assert_eq!(x.matches("<testsuite ").count(), 2, "{x}");
        assert!(x.contains("<testsuite name=\"b1\" tests=\"3\" errors=\"1\" failures=\"1\" skipped=\"0\">"), "{x}");
        assert!(x.contains("<testsuite name=\"b2\" tests=\"4\" errors=\"2\" failures=\"1\" skipped=\"1\">"), "{x}");
        for t in ["gap", "na"] {
            assert!(case_xml(&x, t).contains("name=\"not_verified_requirements\" value=\"R1\""), "{t}");
        }
    }

    #[test]
    fn a_qualifier_is_a_junit_property_and_ocp_metadata_on_its_test() {
        let (rd, m) = (rd(), ms());
        let i = TestExportInputs { report_data: &rd, measurements: &m };
        let x = junit_xml(&i);
        let c = case_xml(&x, "ok");
        // its own, its requirement's, and the ones a measurement implies
        for q in ["bench_firmware_unreleased", "n_of_m_run", "bench_firmware_unverified", "rests_on_dut_report", "declared_by_study"] {
            assert!(c.contains(q), "{q}: {c}");
        }
        assert!(c.contains("name=\"n_of_m.R2\" value=\"3 of 5 tests run\""), "{c}");
        assert!(c.contains("name=\"measurement.rate\" value=\"12.5 Hz \u{b1}0.5 (k=2)\""), "{c}");
        let o = ocp_jsonl(&i);
        let end = end_of(&o, "ok");
        assert!(end["metadata"]["qualifiers"].as_array().unwrap().iter().any(|q| q == "bench_firmware_unreleased"));
        let meas: Value =
            o.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|v| v["testStepArtifact"]["measurement"].is_object()).unwrap();
        let md = &meas["testStepArtifact"]["measurement"];
        assert_eq!((md["value"].clone(), md["unit"].clone(), md["metadata"]["U"].clone()), (json!(12.5), json!("Hz"), json!(0.5)));
        assert!(md["metadata"]["qualifiers"].as_array().unwrap().iter().any(|q| q == "rests_on_dut_report"));
    }

    #[test]
    fn the_run_start_and_end_and_purity() {
        let (rd, m) = (rd(), ms());
        let i = TestExportInputs { report_data: &rd, measurements: &m };
        let o = ocp_jsonl(&i);
        let first: Value = serde_json::from_str(o.lines().next().unwrap()).unwrap();
        assert_eq!(first["testRunArtifact"]["testRunStart"]["name"], "usb-pd");
        let last: Value = serde_json::from_str(o.lines().last().unwrap()).unwrap();
        assert_eq!(last["testRunArtifact"]["testRunEnd"]["status"], "COMPLETE");
        assert_eq!(last["testRunArtifact"]["testRunEnd"]["result"], "FAIL");
        assert_eq!(last["testRunArtifact"]["testRunEnd"]["metadata"]["not_verified"], true);
        assert_eq!(o, ocp_jsonl(&i));
        assert_eq!(junit_xml(&i), junit_xml(&i));
    }

    #[test]
    fn an_unknown_skip_reason_is_a_gap_so_a_default_gate_fails_on_it() {
        assert_eq!(map_verdict(&json!("skipped"), &json!("not_run")), Mapped::NotVerified);
        assert_eq!(map_verdict(&json!("skipped"), &Value::Null), Mapped::NotVerified);
        assert_eq!(map_verdict(&json!("skipped"), &json!("not_selected")), Mapped::Skipped);
    }
}
