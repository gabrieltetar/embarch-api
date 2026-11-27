//! `verify`'s checks 4, 6 and 9 (`embarch-api` decisions 122 to 125;
//! `notes/hil/06-report-and-publish.md`, "Verify"): the PDF's text, the report's
//! other claims, and the pictures at their source.
//!
//! Check 4 reads the PDF with the shared crate's extractor and number grammar
//! (`embarch-study-designer/interfaces/hil-pdf.md`); its constants are the doc's,
//! assumed until a Typst-rendered PDF measured them (decision 123).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use embarch_study_designer::number_grammar::{numbers_in_text, numbers_in_word, NumberToken};
use embarch_study_designer::pdf_text::{extract_pages, words, Page, Word};
use serde_json::{json, Value};
use typst::syntax::{ast, parse, SyntaxNode};

use crate::publish::{MEASUREMENTS_NAME, PACKAGES_DIR, REPORT_DATA_NAME, REPORT_PDF};
use crate::publish_oracle::{self, Kind, Source};
use crate::report_pdf::{plot_ticks, vendored_files, APPENDIX_FILE, REPORT_FILE};
use crate::report_whitelist::{self, Plot};
use crate::verify::{adoptions, json_of, segment_lines, sha, Check};

/// An A4 page's width in points: the compile sets no paper, so Typst's default.
/// A Letter page is 8.5 points wider, inside the footer's tolerance.
const PAGE_WIDTH_PT: f64 = 595.28;
const MM_PT: f64 = 72.0 / 25.4;
/// The footer's page number: centre within 5 mm of the page's, baseline 8 to 16
/// mm above its bottom edge. `hil-pdf.md`'s band is 8 to 16 mm, measured on one
/// Typst page (decision 123): a Typst-rendered page sets the baseline at 14.96 mm
/// (11 pt text) and a little above 15 mm with the appendix's 9 pt.
const FOOTER_CENTRE_MM: f64 = 5.0;
const FOOTER_BASE_MIN_MM: f64 = 8.0;
const FOOTER_BASE_MAX_MM: f64 = 16.0;
/// How many unmatched numbers a check lists before it says how many more.
const LIST_CAP: usize = 12;

type Key = (bool, String, i32);

fn key(t: &NumberToken) -> Key {
    let n = t.value.normalised();
    (n.negative, n.coefficient, n.exp10)
}

fn text_of(files: &BTreeMap<String, Vec<u8>>, name: &str) -> Option<String> {
    files.get(name).map(|b| String::from_utf8_lossy(b).into_owned())
}

fn strings_and_numbers(v: &Value, strings: &mut HashSet<String>, numbers: &mut Vec<f64>) {
    match v {
        Value::String(s) => {
            strings.insert(s.clone());
        }
        Value::Number(n) => numbers.extend(n.as_f64()),
        Value::Array(a) => a.iter().for_each(|x| strings_and_numbers(x, strings, numbers)),
        Value::Object(o) => o.values().for_each(|x| strings_and_numbers(x, strings, numbers)),
        _ => {}
    }
}

fn string_literals(src: &str) -> Vec<String> {
    fn walk(n: &SyntaxNode, out: &mut Vec<String>) {
        if let Some(s) = n.cast::<ast::Str>() {
            out.push(s.get().to_string());
        }
        n.children().for_each(|c| walk(c, out));
    }
    let mut out = Vec::new();
    walk(&parse(src), &mut out);
    out
}

fn is_study(files: &BTreeMap<String, Vec<u8>>) -> bool {
    files.keys().any(|n| n.starts_with(PACKAGES_DIR))
}

/// The tick texts the helper draws for each plot of the report, derived here
/// from the signed capture with the helper's own algorithm; no list is read
/// from the PDF.
fn tick_texts(files: &BTreeMap<String, Vec<u8>>, plots: &[Plot]) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for p in plots {
        let csv = text_of(files, &p.file).ok_or_else(|| format!("line {}: the plot reads {}, which the zip lacks", p.line, p.file))?;
        let mut lines = csv.lines();
        let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
        let col = |name: &str| header.iter().position(|h| *h == name).ok_or_else(|| format!("line {}: {} has no column {name:?}", p.line, p.file));
        let (xi, yi) = (col(&p.x)?, col(&p.y)?);
        let mut rows: Vec<(f64, f64)> = Vec::new();
        for l in lines.filter(|l| !l.is_empty()) {
            let cells: Vec<&str> = l.split(',').collect();
            let num = |i: usize| cells.get(i).and_then(|c| c.parse::<f64>().ok()).ok_or_else(|| format!("{}: a row has no number in column {i}", p.file));
            rows.push((num(xi)?, num(yi)?));
        }
        let n = rows.len();
        if n == 0 {
            return Err(format!("{} has no rows to plot", p.file));
        }
        let nb = p.bins.min(n).max(1);
        let (mut xs, mut mins, mut maxs) = (Vec::new(), Vec::new(), Vec::new());
        for b in 0..nb {
            let seg = &rows[b * n / nb..(b + 1) * n / nb];
            xs.push(seg[0].0);
            mins.push(seg.iter().map(|r| r.1).fold(f64::INFINITY, f64::min));
            maxs.push(seg.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max));
        }
        let span = |v: &[f64]| (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        let (xl, xh) = span(&xs);
        let (yl, _) = span(&mins);
        let (_, yh) = span(&maxs);
        for (lo, hi) in [(xl, xh), (yl, yh)] {
            let t = plot_ticks(lo, hi).map_err(|e| format!("{}: the plot's axis is refused ({e:?})", p.file))?;
            out.extend(t.texts);
        }
    }
    Ok(out)
}

/// The index of the word the footer rule admits on `page`, or why the page's
/// number is wrong.
fn footer_word(page: &Page, ws: &[Word]) -> Result<Option<usize>, String> {
    let cands: Vec<usize> = ws
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            let centre = (w.x + w.x_end) / 2.0;
            !w.text.is_empty()
                && w.text.chars().all(|c| c.is_ascii_digit())
                && (centre - PAGE_WIDTH_PT / 2.0).abs() <= FOOTER_CENTRE_MM * MM_PT
                && (FOOTER_BASE_MIN_MM * MM_PT..=FOOTER_BASE_MAX_MM * MM_PT).contains(&w.y)
        })
        .map(|(i, _)| i)
        .collect();
    match cands.as_slice() {
        [i] if ws[*i].text.parse::<u32>().ok() == Some(page.number) => Ok(Some(*i)),
        [i] => Err(format!("page {}: the footer reads {}, not the page's index {}", page.number, ws[*i].text, page.number)),
        _ => Ok(None),
    }
}

// ---- check 4 -----------------------------------------------------------------

pub(crate) fn check_pdf_text(files: &BTreeMap<String, Vec<u8>>) -> Check {
    let mut c = Check::new(4, "the PDF's text");
    let (Some(pdf), Some(rd)) = (files.get(REPORT_PDF), json_of(files, REPORT_DATA_NAME)) else {
        c.fail(format!("{REPORT_PDF} or {REPORT_DATA_NAME} is not in the zip, or is not readable"));
        return c;
    };
    let pages = match extract_pages(pdf) {
        Ok(p) => p,
        Err(e) => {
            c.fail(format!("{e}"));
            return c;
        }
    };
    let measurements = json_of(files, MEASUREMENTS_NAME);
    let events = json_of(files, "events.json");

    // What the signed data holds: strings (opaque words) and numbers.
    let mut opaque: HashSet<String> = HashSet::new();
    let mut leaves: Vec<f64> = Vec::new();
    strings_and_numbers(&rd, &mut opaque, &mut leaves);
    if let Some(e) = &events {
        strings_and_numbers(e, &mut opaque, &mut leaves);
    }

    // The sources, rescanned: the prose files and, for a Study's own report, report.typ.
    let study = is_study(files);
    let prose: Vec<(String, String)> =
        files.iter().filter(|(n, _)| n.starts_with("prose/")).map(|(n, b)| (n.clone(), String::from_utf8_lossy(b).into_owned())).collect();
    let report_typ = text_of(files, REPORT_FILE);
    let mut sources: Vec<Source> = prose.iter().map(|(n, t)| Source { name: n, text: t, typst: false }).collect();
    if let (true, Some(t)) = (study, &report_typ) {
        sources.push(Source { name: REPORT_FILE, text: t, typst: true });
    }
    let rows = publish_oracle::rows(&sources, measurements.as_ref(), &rd);
    let mut listed: HashSet<Key> = HashSet::new();
    for r in &rows {
        if r.kind == Kind::Mismatch {
            c.fail(format!("{} line {}: {} is cited as a measurement and does not match it ({})", r.file, r.line, r.text, r.detail));
        }
        if !r.text.starts_with('#') {
            for t in numbers_in_text(&r.text, &HashSet::new()) {
                listed.insert(key(&t));
            }
        }
    }

    // Axis ticks, derived from the signed series of each plot.
    let verdict = report_typ.as_deref().map(|t| report_whitelist::check(t, &report_whitelist::is_core_file));
    let plots = verdict.map(|v| v.plots).unwrap_or_default();
    let ticks = match tick_texts(files, &plots) {
        Ok(t) => t,
        Err(why) => {
            c.fail(why);
            BTreeSet::new()
        }
    };

    // The appendix's own numbers: embarch-api's file, which Core does not sign.
    let mut appendix: HashSet<Key> = HashSet::new();
    let mut own: Vec<String> = text_of(files, APPENDIX_FILE).map(|t| string_literals(&t)).unwrap_or_default();
    if !study {
        own.extend(report_typ.as_deref().map(string_literals).unwrap_or_default());
    }
    for lit in &own {
        for t in numbers_in_text(lit, &opaque) {
            appendix.insert(key(&t));
        }
    }

    let (mut total, mut by_measurement, mut by_data, mut by_listing, mut by_tick, mut footers, mut by_appendix) = (0, 0, 0, 0, 0, 0, 0);
    let mut appendix_only: BTreeSet<String> = BTreeSet::new();
    let mut unmatched: Vec<String> = Vec::new();
    for page in &pages {
        let ws = words(page);
        let footer = match footer_word(page, &ws) {
            Ok(f) => f,
            Err(why) => {
                c.fail(why);
                None
            }
        };
        if footer.is_some() {
            footers += 1;
        }
        for (i, w) in ws.iter().enumerate() {
            if Some(i) == footer {
                continue;
            }
            for t in numbers_in_word(&w.text, &opaque) {
                total += 1;
                let ascii = t.text.replace('\u{2212}', "-");
                let value = t.value.to_f64();
                if ticks.contains(&ascii) {
                    by_tick += 1;
                } else if publish_oracle::admits_number(measurements.as_ref(), value, t.sig_digits as usize) {
                    by_measurement += 1;
                } else if leaves.contains(&value) {
                    by_data += 1;
                } else if listed.contains(&key(&t)) {
                    by_listing += 1;
                } else if appendix.contains(&key(&t)) {
                    by_appendix += 1;
                    appendix_only.insert(t.text.clone());
                } else {
                    unmatched.push(format!("page {}: {} (in {:?})", page.number, t.text, w.text));
                }
            }
        }
    }
    for u in unmatched.iter().take(LIST_CAP) {
        c.fail(format!("{u} matches no signed measurement, no value report-data.json holds, no listed number, no axis tick and no appendix number"));
    }
    if unmatched.len() > LIST_CAP {
        c.fail(format!("and {} more numbers in the PDF that match nothing", unmatched.len() - LIST_CAP));
    }
    c.compared.push(format!(
        "{total} numbers in {} pages: {by_measurement} equal a signed measurement at the digits written, {by_data} a value the signed data holds, \
         {by_listing} a number the rescan of the prose and report.typ lists, {by_tick} an axis tick derived from the signed series, \
         {by_appendix} a number only the appendix prints, {footers} a footer's page number",
        pages.len()
    ));
    if by_appendix > 0 {
        c.partial(format!(
            "{} number(s) ({}) were admitted only because appendix.typ, embarch-api's own unsigned file, prints them: they rest on embarch-api, not on Core's signature",
            appendix_only.len(),
            appendix_only.iter().take(LIST_CAP).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    c.compared.push("citations and tags rescanned in the prose files and report.typ the zip carries".into());
    c
}

// ---- check 6 -----------------------------------------------------------------

fn count_outcomes(events: &Value) -> Value {
    let (mut pass, mut fail, mut timed_out, mut other) = (0u64, 0u64, 0u64, 0u64);
    for s in events["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        match &s["outcome"] {
            o if o == "Pass" => pass += 1,
            o if o == "TimedOut" => timed_out += 1,
            o if o.get("Fail").is_some() => fail += 1,
            _ => other += 1,
        }
    }
    json!({ "total": pass + fail + timed_out + other, "pass": pass, "fail": fail, "timed_out": timed_out, "other": other })
}

/// What `attestation` must be. A record whose `observed` carries a `release` key is a newer Core's: the report names its
/// mode and release and has no `core_attested`. Any other record keeps the milestone-1 shape.
fn attestation_claim(record: &Value, label: &str) -> Value {
    match record["predicate"]["observed"].get("release") {
        Some(release) => json!({ "label": label, "core_mode": record["predicate"]["observed"]["mode"], "release": release }),
        None => json!({ "label": label, "core_attested": false }),
    }
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

pub(crate) fn check_report_claims(files: &BTreeMap<String, Vec<u8>>, manifest: Option<&Value>, record: Option<&Value>, fingerprint: &str) -> Check {
    let mut c = Check::new(6, "the report's other claims");
    let (Some(rd), Some(events), Ok(lines), Some(manifest), Some(record)) =
        (json_of(files, REPORT_DATA_NAME), json_of(files, "events.json"), segment_lines(files), manifest, record)
    else {
        c.fail("report-data.json, events.json, the run-log segment, the signed manifest or the signed record cannot be read".into());
        return c;
    };
    let id = rd["study_id"].as_str().unwrap_or("");
    let (adopted, run) = adoptions(&lines, id);
    let Some(run) = run else {
        c.fail(format!("the segment holds no run entry for {id}"));
        return c;
    };
    let go_by = run["go_by"].as_str().unwrap_or("agent");
    let published_by = manifest["predicate"]["published_by"].as_str().unwrap_or("agent");
    let declared_person = record["predicate"]["observed"]["declarations"]["entries"]
        .as_array()
        .is_some_and(|e| e.iter().any(|d| d["scope"] == "person"));
    let person_acted = go_by == "person" || published_by == "person" || adopted.iter().any(|a| a["scope"] == "person") || declared_person;
    let label = if person_acted { "person's acts not protected" } else { "agent-scoped only" };
    let steps = count_outcomes(&events);
    let run_status = if run["clean"] == true { "completed" } else { "step_failed" };
    let mut observed = json!({ "image_status": record["predicate"]["observed"]["image_status"],
                               "declarations": record["predicate"]["observed"]["declarations"] });
    // Present exactly when the signed record carries `observed.call_log`; `count` is the range's length,
    // which check 8 ties to the segment's entries.
    let signed_log = &record["predicate"]["observed"]["call_log"];
    if !signed_log.is_null() {
        let count = match (signed_log["first_seq"].as_u64(), signed_log["last_seq"].as_u64()) {
            (Some(f), Some(l)) if l >= f => l - f + 1,
            _ => 0,
        };
        observed["call_log"] = json!({ "head": signed_log["head"], "first_seq": signed_log["first_seq"],
                                       "last_seq": signed_log["last_seq"], "count": count });
    }
    let expected = json!({
        "schema": 1, "study_id": id, "kind": run["kind"], "revision": manifest["predicate"]["revision"],
        "run_status": run_status, "steps": steps,
        "subject": { "study_name": events["study_name"], "provenance": events["provenance"] },
        "attestation": attestation_claim(record, label),
        "go_by": go_by, "published_by": published_by, "adopted": adopted,
        "observed": observed,
        "claimed": { "build": record["predicate"]["claimed"]["build"] },
        "core": { "version": manifest["predicate"]["core"]["version"], "keyid": fingerprint },
        "record_sha256": run["record_sha256"],
    });
    let mut differing: Vec<String> = Vec::new();
    for (k, want) in expected.as_object().into_iter().flatten() {
        let have = &rd[k.as_str()];
        let same = if k == "attestation" { true } else { have == want };
        if k == "attestation" {
            // Compare every key but `reason`, which is Core's wording; name each one that differs.
            let keys: std::collections::BTreeSet<&String> =
                want.as_object().into_iter().flat_map(|o| o.keys()).chain(have.as_object().into_iter().flat_map(|o| o.keys())).collect();
            for sub in keys.into_iter().filter(|s| s.as_str() != "reason" && have.get(s.as_str()) != want.get(s.as_str())) {
                differing.push(format!("attestation.{sub}"));
            }
        } else if !same {
            if k == "observed" {
                // Name the sub-key, so a call-log claim that disagrees is named as such.
                let keys: std::collections::BTreeSet<&String> =
                    want.as_object().into_iter().flat_map(|o| o.keys()).chain(have.as_object().into_iter().flat_map(|o| o.keys())).collect();
                for sub in keys.into_iter().filter(|s| have[s.as_str()] != want[s.as_str()]) {
                    differing.push(format!("observed.{sub}"));
                }
            } else {
                differing.push(k.clone());
            }
        }
    }
    for k in rd.as_object().into_iter().flat_map(|o| o.keys()) {
        if expected.get(k.as_str()).is_none() {
            differing.push(k.clone());
        }
    }
    if !differing.is_empty() {
        c.fail(format!("report-data.json's {} is not what the signed record, the run log's segment and the manifest give", differing.join(", ")));
    } else {
        c.compared.push(
            "every field of report-data.json recomputed from the signed record, events.json, the run log's segment and the signed manifest; \
             the attestation's explanatory text is Core's wording and is not compared"
                .into(),
        );
    }

    // What the PDF prints of it.
    let Some(pdf) = files.get(REPORT_PDF) else {
        c.fail(format!("{REPORT_PDF} is not in the zip"));
        return c;
    };
    let pages = match extract_pages(pdf) {
        Ok(p) => p,
        Err(e) => {
            c.fail(format!("the PDF cannot be read, so what it prints of report-data.json is not checked: {e}"));
            return c;
        }
    };
    let printed: String = squash(&pages.iter().flat_map(words).map(|w| w.text).collect::<Vec<_>>().join(" "));
    let n = |k: &str| expected["steps"][k].to_string();
    let phrase = format!("{} total, {} pass, {} fail, {} timed out, {} other", n("total"), n("pass"), n("fail"), n("timed_out"), n("other"));
    let claims: Vec<(&str, String)> = vec![
        ("the run's id", id.to_string()),
        ("the run's status", run_status.to_string()),
        ("the step counts", phrase),
        ("the attestation label", label.to_string()),
        ("who began the run", go_by.to_string()),
        ("who published it", published_by.to_string()),
        ("the revision", format!("This is revision {}.", expected["revision"])),
    ];
    for (what, text) in &claims {
        if printed.contains(&squash(text)) {
            c.compared.push(format!("the PDF prints {what}: {text:?}"));
        } else {
            c.fail(format!("the PDF does not print {what} as report-data.json gives it ({text:?})"));
        }
    }
    c
}

// ---- check 9 -----------------------------------------------------------------

pub(crate) fn check_pictures(files: &BTreeMap<String, Vec<u8>>) -> Check {
    let mut c = Check::new(9, "the pictures, at the source");
    let mut scanned = 0;
    for name in [REPORT_FILE, APPENDIX_FILE] {
        let Some(src) = text_of(files, name) else {
            c.fail(format!("{name} is not in the zip"));
            continue;
        };
        scanned += 1;
        let v = report_whitelist::check(&src, &report_whitelist::is_core_file);
        for viol in &v.violations {
            c.fail(format!("{name} line {}: {}", viol.line, viol.what));
        }
        for p in &v.plots {
            if files.contains_key(&p.file) {
                c.compared.push(format!("{name} line {}: a plot of {} (x {:?}, y {:?}, {} bins), a file Core signs", p.line, p.file, p.x, p.y, p.bins));
            } else {
                c.fail(format!("{name} line {}: the plot reads {}, which the zip lacks", p.line, p.file));
            }
        }
    }
    c.compared.push(format!("{scanned} Typst sources parsed and held to the whitelist of the syntax tree"));

    // Every package the zip carries, hashed against this checkout's list.
    let list: BTreeMap<String, String> = vendored_files().into_iter().map(|f| (format!("{PACKAGES_DIR}{}", f.path), f.sha256)).collect();
    let carried: Vec<&String> = files.keys().filter(|n| n.starts_with(PACKAGES_DIR)).collect();
    for name in &carried {
        match list.get(*name) {
            None => c.fail(format!("{name} is a Typst package file this build does not ship")),
            Some(h) if *h != sha(&files[*name]) => c.fail(format!("{name} differs from the package file this build ships")),
            _ => {}
        }
    }
    let imports = text_of(files, REPORT_FILE).is_some_and(|t| t.contains("@embarch/report"));
    if !carried.is_empty() || imports {
        for name in list.keys().filter(|n| !files.contains_key(*n)) {
            c.fail(format!("{name} is on this build's package list and the zip lacks it"));
        }
    }
    c.compared.push(format!("{} carried package files hashed against the {} this checkout ships", carried.len(), list.len()));
    c
}

