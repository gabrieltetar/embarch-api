//! A build's verdict: what a caller reads instead of the log (embarch-api
//! decision 102).
//!
//! The log is kept whole on disk ([`crate::build::BuildOutcome::log_path`]);
//! this is the part of it a person or an agent acts on. Every field is read
//! out of the build's own output by matching the line shapes the Zephyr
//! toolchain prints — gcc and clang diagnostics, Kconfig, CMake, `dtc` and
//! devicetree errors, the linker's `--print-memory-usage` table, ztest and
//! twister result lines. A line this does not recognise costs a field, never
//! a wrong build: the exit code and the freshness check still decide success.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// How many distinct warnings are quoted; the rest are only counted.
pub const MAX_WARNINGS_LISTED: usize = 10;
/// A quoted line longer than this is cut, with `…` marking the cut.
pub const MAX_LINE_BYTES: usize = 240;
/// The first error block stops after this many lines…
pub const MAX_ERROR_LINES: usize = 20;
/// …or this many bytes, whichever comes first.
pub const MAX_ERROR_BYTES: usize = 3000;
/// How many test result lines are listed before the rest are only counted.
pub const MAX_TEST_LINES: usize = 100;

/// One linker memory region's use, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// What [`parse`] found in one build's log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    /// Distinct warnings: gcc repeats a header's warning once per
    /// translation unit, so identical lines count once.
    pub warnings: usize,
    /// The first [`MAX_WARNINGS_LISTED`] distinct warnings, as
    /// `file:line: message`.
    pub first_warnings: Vec<String>,
    /// The first error and the lines that belong to it, with everything the
    /// build printed after it (the cascade) dropped.
    pub first_error: Option<String>,
    /// The last linker memory table in the log, by region name. A sysbuild
    /// log carries one table per image; the last one printed wins.
    pub memory: BTreeMap<String, Region>,
    /// One line per test case (`PASS suite.case`), or per twister scenario
    /// when the log has no per-case lines. Empty when the log has no tests.
    pub tests: Vec<String>,
    /// Test lines beyond [`MAX_TEST_LINES`], counted but not listed.
    pub tests_not_listed: usize,
    pub first_failing_assertion: Option<String>,
}

/// Reads a build's whole log, in the order it was printed. `source` is
/// stripped from the front of every quoted path, so a quoted line names
/// `app/src/main.c` rather than the absolute path.
pub fn parse(log: &str, source: Option<&Path>) -> Verdict {
    let prefix = source.map(|p| {
        let mut s = p.display().to_string();
        if !s.ends_with('/') && !s.ends_with('\\') {
            s.push(std::path::MAIN_SEPARATOR);
        }
        s
    });
    let shorten = |line: &str| -> String { clip(&strip_prefix(line.trim(), prefix.as_deref())) };
    let lines: Vec<&str> = log.lines().collect();

    let mut verdict = Verdict::default();
    let mut seen_warnings = std::collections::HashSet::new();
    let mut ztest_lines: Vec<String> = Vec::new();
    let mut twister_lines: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(w) = warning(line, lines.get(i + 1).copied()) {
            let w = shorten(&w);
            if seen_warnings.insert(w.clone()) && verdict.first_warnings.len() < MAX_WARNINGS_LISTED {
                verdict.first_warnings.push(w);
            }
        }
        if verdict.first_error.is_none() && is_error_start(line) {
            verdict.first_error = Some(error_block(&lines, i, &shorten));
        }
        if is_memory_header(line) {
            verdict.memory = memory_table(&lines[i + 1..]);
        }
        if let Some(t) = ztest_case(line) {
            ztest_lines.push(t);
        } else if let Some(t) = twister_scenario(line) {
            twister_lines.push(t);
        }
        if verdict.first_failing_assertion.is_none() && line.contains("Assertion failed at") {
            verdict.first_failing_assertion = Some(shorten(line));
        }
        i += 1;
    }
    verdict.warnings = seen_warnings.len();

    let mut tests = if ztest_lines.is_empty() { twister_lines } else { ztest_lines };
    let mut seen = std::collections::HashSet::new();
    tests.retain(|t| seen.insert(t.clone()));
    if tests.len() > MAX_TEST_LINES {
        verdict.tests_not_listed = tests.len() - MAX_TEST_LINES;
        tests.truncate(MAX_TEST_LINES);
    }
    verdict.tests = tests;
    verdict
}

fn strip_prefix(line: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(p) if !p.is_empty() => line.replace(p, ""),
        _ => line.to_string(),
    }
}

/// Cuts `s` to [`MAX_LINE_BYTES`] on a character boundary.
fn clip(s: &str) -> String {
    if s.len() <= MAX_LINE_BYTES {
        return s.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `path:12:5` and `path:12` both become `path:12`; `dtc`'s
/// `path:12.4-56` does too.
fn file_line(loc: &str) -> String {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((head, last)) = loc.rsplit_once(':') else { return loc.to_string() };
    if numeric(last) {
        // `file:line:col` drops the column; `file:line` (or `C:\x:line`) stays.
        return match head.rsplit_once(':') {
            Some((_, line)) if numeric(line) => head.to_string(),
            _ => loc.to_string(),
        };
    }
    let line: String = last.chars().take_while(char::is_ascii_digit).collect();
    if line.is_empty() {
        loc.to_string()
    } else {
        format!("{head}:{line}")
    }
}

/// A warning in one of the shapes the toolchain prints, as `file:line:
/// message`. `next` is the following line, which carries a CMake warning's
/// message.
fn warning(line: &str, next: Option<&str>) -> Option<String> {
    let t = line.trim();
    if let Some(i) = t.find(": warning: ") {
        return Some(format!("{}: {}", file_line(&t[..i]), &t[i + ": warning: ".len()..]));
    }
    if let Some(msg) = t.strip_prefix("warning: ") {
        // Kconfig's own warnings carry no location of their own.
        return Some(format!("Kconfig: {msg}"));
    }
    if let Some(rest) = t.strip_prefix("CMake Warning") {
        let loc = rest
            .split_once(" at ")
            .and_then(|(_, at)| at.rsplit_once(" (").map(|(loc, _)| file_line(loc)))
            .unwrap_or_else(|| "CMake".to_string());
        let msg = next.map(str::trim).filter(|m| !m.is_empty()).unwrap_or("(no message)");
        return Some(format!("{loc}: {msg}"));
    }
    if let Some(i) = t.find(": Warning (") {
        return Some(format!("{}: {}", file_line(&t[..i]), &t[i + 2..]));
    }
    None
}

/// Lines that start an error. The cascade lines a failed build always ends
/// with — ninja's `FAILED:` and `build stopped`, `collect2`'s exit status,
/// west's `command exited with status` — never do.
fn is_error_start(line: &str) -> bool {
    let t = line.trim_start();
    let cascade = t.starts_with("FAILED: ")
        || t.starts_with("ninja: ")
        || t.contains("collect2: error: ld returned")
        || t.contains("command exited with status")
        || t.starts_with("-- Configuring incomplete");
    if cascade {
        return false;
    }
    t.contains(": error: ")
        || t.contains(": fatal error: ")
        || t.starts_with("error: ")
        || t.starts_with("fatal error: ")
        || t.starts_with("CMake Error")
        || t.contains("devicetree error:")
        || t.starts_with("Error: ")
        || t.starts_with("FATAL ERROR: ")
        || t.starts_with("ERROR: ")
        || t.contains("undefined reference to")
        || t.contains("will not fit in region")
        || t.contains("overflowed by ")
}

/// The error starting at `lines[start]` and the lines that belong to it:
/// the indented lines after it (a compiler's source excerpt, a CMake or
/// Kconfig message body) and any `note:`. A gcc `In function` line just
/// ahead of it is kept; so are the Kconfig warnings an `Aborting due to
/// Kconfig warnings` error is about, since that line alone says nothing.
fn error_block(lines: &[&str], start: usize, shorten: &dyn Fn(&str) -> String) -> String {
    let mut block: Vec<String> = Vec::new();
    let first = lines[start].trim();
    if first.contains("Aborting due to Kconfig warnings") {
        let mut j = start;
        let mut lead = Vec::new();
        while j > 0 && lead.len() < MAX_WARNINGS_LISTED {
            let prev = lines[j - 1];
            let p = prev.trim_start();
            let belongs = p.starts_with("warning:") || p.contains(": warning:") || prev.starts_with(' ');
            if !belongs || p.is_empty() {
                break;
            }
            lead.push(shorten(prev));
            j -= 1;
        }
        lead.reverse();
        block.extend(lead);
    } else if start > 0 && lines[start - 1].contains("In function") {
        block.push(shorten(lines[start - 1]));
    }
    block.push(shorten(lines[start]));
    let mut bytes: usize = block.iter().map(String::len).sum();
    for line in &lines[start + 1..] {
        let continues = (line.starts_with(' ') || line.starts_with('\t')) && !line.trim().is_empty()
            || line.contains(": note: ");
        if !continues || block.len() >= MAX_ERROR_LINES {
            break;
        }
        let s = clip(&strip_leading_prefix(line, shorten));
        bytes += s.len() + 1;
        if bytes > MAX_ERROR_BYTES {
            break;
        }
        block.push(s);
    }
    block.join("\n")
}

/// Like `shorten`, but a source excerpt keeps its indentation: the caret
/// under a gcc excerpt points at a column.
fn strip_leading_prefix(line: &str, shorten: &dyn Fn(&str) -> String) -> String {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    format!("{indent}{}", shorten(line))
}

fn is_memory_header(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Memory region") && t.contains("Used Size")
}

/// The rows under a `Memory region  Used Size  Region Size  %age Used`
/// header: `FLASH:  45680 B  1 MB  4.36%`.
fn memory_table(rows: &[&str]) -> BTreeMap<String, Region> {
    let mut table = BTreeMap::new();
    for row in rows {
        let f: Vec<&str> = row.split_whitespace().collect();
        let [name, used, used_unit, total, total_unit, pct] = f.as_slice() else { break };
        let Some(name) = name.strip_suffix(':') else { break };
        if !pct.ends_with('%') {
            break;
        }
        let (Some(used), Some(total)) = (bytes(used, used_unit), bytes(total, total_unit)) else { break };
        table.insert(name.to_string(), Region { used_bytes: used, total_bytes: total });
    }
    table
}

fn bytes(value: &str, unit: &str) -> Option<u64> {
    let v: f64 = value.parse().ok()?;
    let scale: f64 = match unit {
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((v * scale).round() as u64)
}

/// ztest's summary line: ` - FAIL - [suite.case] duration = 0.001 seconds`.
fn ztest_case(line: &str) -> Option<String> {
    let t = line.trim_start().strip_prefix("- ")?;
    let (status, rest) = t.split_once(" - [")?;
    if !matches!(status, "PASS" | "FAIL" | "SKIP" | "BLOCK") {
        return None;
    }
    let (name, _) = rest.split_once(']')?;
    Some(format!("{status} {name}"))
}

/// twister's per-scenario line: `INFO - 3/8 native_sim tests/x/test.y
/// FAILED : Failed (native 0.012s)`.
fn twister_scenario(line: &str) -> Option<String> {
    let f: Vec<&str> = line.split_whitespace().collect();
    let i = f.iter().position(|tok| {
        tok.split_once('/')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()))
    })?;
    let (platform, scenario, status) = (f.get(i + 1)?, f.get(i + 2)?, f.get(i + 3)?);
    let status = status.trim_end_matches(':');
    if !matches!(status, "PASSED" | "FAILED" | "ERROR" | "SKIPPED" | "FILTERED" | "BLOCKED") {
        return None;
    }
    Some(format!("{status} {platform} {scenario}"))
}

/// The record a build leaves beside its log, read back by a later build's
/// `vs` and by a no-relink build that printed no memory table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    pub success: bool,
    pub memory: BTreeMap<String, Region>,
    /// Seconds since the Unix epoch when the build finished.
    pub finished_at: u64,
}

pub fn read_record(path: &Path) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Per region present in both: `this - other`, in bytes.
pub fn memory_delta(this: &BTreeMap<String, Region>, other: &BTreeMap<String, Region>) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (name, region) in this {
        if let Some(theirs) = other.get(name) {
            let delta = region.used_bytes as i64 - theirs.used_bytes as i64;
            out.insert(name.clone(), serde_json::json!({ "delta_bytes": delta }));
        }
    }
    serde_json::Value::Object(out)
}


/// The default answer of `build` and `build_and_flash` (decision 102): the
/// verdict, not the log. The caller adds `target`, and `pristine` and `vs`
/// when they apply.
pub fn verdict_json(outcome: &crate::build::BuildOutcome) -> serde_json::Value {
    let v = &outcome.verdict;
    let mut out = serde_json::json!({
        "success": outcome.build_succeeded(),
        "exit_code": outcome.exit_code,
        "timed_out": outcome.timed_out,
        "seconds": (outcome.seconds * 10.0).round() / 10.0,
        "warnings": v.warnings,
        "first_warnings": v.first_warnings,
        "first_error": v.first_error,
        "memory": v.memory,
        "artifact_path": outcome.artifact_path.display().to_string(),
        "artifact_fresh": outcome.artifact_fresh,
        "log_path": outcome.log_path.as_ref().map(|p| p.display().to_string()),
    });
    if outcome.memory_from_previous_build {
        out["memory_from_previous_build"] = serde_json::Value::Bool(true);
    }
    if !v.tests.is_empty() || v.first_failing_assertion.is_some() {
        out["tests"] = serde_json::json!(v.tests);
        if v.tests_not_listed > 0 {
            out["tests_not_listed"] = serde_json::json!(v.tests_not_listed);
        }
        out["first_failing_assertion"] = serde_json::json!(v.first_failing_assertion);
    }
    out
}

/// The same verdict for a person: one status line, then only what needs
/// reading — warnings, the error, test lines, the log's path.
pub fn verdict_text(project: &str, outcome: &crate::build::BuildOutcome, vs: Option<&serde_json::Value>) -> String {
    let v = &outcome.verdict;
    let status = if outcome.timed_out {
        "TIMED OUT".to_string()
    } else if outcome.build_succeeded() {
        "ok".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("FAILED (exit {code})"),
            None => "FAILED (no exit code)".to_string(),
        }
    };
    let mut line = format!("build {status} '{project}' in {:.1}s: {} warning(s)", outcome.seconds, v.warnings);
    for (name, r) in &v.memory {
        if name == "FLASH" || name == "RAM" {
            line.push_str(&format!(", {name} {}/{} B", r.used_bytes, r.total_bytes));
        }
    }
    if outcome.memory_from_previous_build {
        line.push_str(" (memory from the previous build; nothing relinked)");
    }
    if outcome.build_succeeded() {
        let fresh = if outcome.artifact_fresh { "fresh" } else { "NOT fresh" };
        line.push_str(&format!("; artifact {fresh}: {}", outcome.artifact_path.display()));
    }
    let mut out = vec![line];
    if let Some(vs) = vs {
        out.push(vs_text(vs));
    }
    out.extend(v.first_warnings.iter().map(|w| format!("warning: {w}")));
    if let Some(e) = &v.first_error {
        out.push(e.clone());
    }
    out.extend(v.tests.iter().cloned());
    if let Some(a) = &v.first_failing_assertion {
        out.push(a.clone());
    }
    if let Some(p) = &outcome.log_path {
        out.push(format!("log: {}", p.display()));
    }
    out.join("\n")
}

fn vs_text(vs: &serde_json::Value) -> String {
    let other = vs["build_variant"].as_str().unwrap_or("?");
    if let Some(e) = vs["error"].as_str() {
        return format!("vs {other}: {e}");
    }
    // FLASH and RAM, as in the status line; the JSON carries every region.
    let deltas: Vec<String> = vs["delta"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| *k == "FLASH" || *k == "RAM")
                .map(|(k, d)| format!("{k} {:+} B", d["delta_bytes"].as_i64().unwrap_or(0)))
                .collect()
        })
        .unwrap_or_default();
    format!("vs {other}: {}", if deltas.is_empty() { "no region in common".to_string() } else { deltas.join(", ") })
}

/// The other build `vs` names, read **before** this build runs, so `vs`
/// naming the variant being built compares against its own previous build
/// rather than against the record this build is about to write.
pub struct VsBaseline {
    name: String,
    record: std::result::Result<Record, String>,
}

impl VsBaseline {
    pub fn read(
        project: &crate::config::ProjectConfig,
        selection: crate::resolve::Selection<'_>,
        other: &str,
    ) -> VsBaseline {
        let choice = crate::resolve::BuildChoice { build_variant: Some(other), pristine: false };
        let record = crate::resolve::resolve_build_with(project, selection, choice)
            .map_err(|e| format!("{e:#}"))
            .and_then(|b| {
                let (_, path) = crate::build::log_location(&b.plan);
                read_record(&path)
                    .filter(|r| !r.memory.is_empty())
                    .ok_or_else(|| format!("no memory figures recorded for '{other}' ({}); build it first", path.display()))
            });
        VsBaseline { name: other.to_string(), record }
    }

    /// `{"build_variant", "delta": {region: {"delta_bytes"}}, "baseline_finished_at"}`,
    /// or `{"build_variant", "error"}`.
    pub fn compare(&self, outcome: &crate::build::BuildOutcome) -> serde_json::Value {
        match &self.record {
            Ok(r) => serde_json::json!({
                "build_variant": self.name,
                "delta": memory_delta(&outcome.verdict.memory, &r.memory),
                "baseline_finished_at": r.finished_at,
            }),
            Err(e) => serde_json::json!({ "build_variant": self.name, "error": e }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "/work/proj";

    fn v(log: &str) -> Verdict {
        parse(log, Some(Path::new(SRC)))
    }

    #[test]
    fn a_clean_zephyr_build_gives_memory_and_no_error() {
        let log = "\
[1/150] Generating include/generated/version.h
[150/150] Linking C executable zephyr/zephyr.elf
Memory region         Used Size  Region Size  %age Used
           FLASH:       45680 B         1 MB      4.36%
             RAM:        8448 B       256 KB      3.22%
        IDT_LIST:          0 GB        32 KB      0.00%
Generating files from /work/proj/build/zephyr/zephyr.elf for board: nrf54l15dk
";
        let r = v(log);
        assert_eq!(r.first_error, None);
        assert_eq!(r.warnings, 0);
        assert_eq!(r.memory["FLASH"], Region { used_bytes: 45680, total_bytes: 1024 * 1024 });
        assert_eq!(r.memory["RAM"], Region { used_bytes: 8448, total_bytes: 256 * 1024 });
        assert_eq!(r.memory["IDT_LIST"].used_bytes, 0);
    }

    #[test]
    fn warnings_are_deduplicated_counted_and_quoted_as_file_line_message() {
        let log = "\
/work/proj/app/src/main.c:12:5: warning: unused variable 'x' [-Wunused-variable]
/work/proj/app/inc/a.h:3:1: warning: 'f' defined but not used [-Wunused-function]
/work/proj/app/inc/a.h:3:1: warning: 'f' defined but not used [-Wunused-function]
warning: UART_CONSOLE (defined at drivers/console/Kconfig:42) was assigned the value 'y' but got the value 'n'
/work/proj/app/prj.conf:7: warning: attempt to assign the value 'y' to the undefined symbol FOO
";
        let r = v(log);
        assert_eq!(r.warnings, 4);
        assert_eq!(r.first_warnings[0], "app/src/main.c:12: unused variable 'x' [-Wunused-variable]");
        assert!(r.first_warnings[2].starts_with("Kconfig: UART_CONSOLE"));
        assert_eq!(r.first_warnings[3], "app/prj.conf:7: attempt to assign the value 'y' to the undefined symbol FOO");
    }

    #[test]
    fn only_ten_warnings_are_quoted_but_all_are_counted() {
        let log: String = (0..25).map(|i| format!("/work/proj/a.c:{i}:1: warning: w{i}\n")).collect();
        let r = v(&log);
        assert_eq!(r.warnings, 25);
        assert_eq!(r.first_warnings.len(), MAX_WARNINGS_LISTED);
    }

    #[test]
    fn the_first_compiler_error_keeps_its_excerpt_and_drops_the_cascade() {
        let log = "\
[12/150] Building C object CMakeFiles/app.dir/src/main.c.obj
FAILED: CMakeFiles/app.dir/src/main.c.obj
/opt/sdk/bin/arm-zephyr-eabi-gcc -DKERNEL ... -c /work/proj/app/src/main.c
/work/proj/app/src/main.c: In function 'main':
/work/proj/app/src/main.c:31:46: error: expected ';' before 'return'
   31 |         printk(\"hello\")
      |                       ^
      |                       ;
/work/proj/app/src/main.c:40:1: error: a cascade nobody should chase
ninja: build stopped: subcommand failed.
FATAL ERROR: command exited with status 1: /usr/bin/cmake --build /work/proj/build
";
        let r = v(log);
        let e = r.first_error.unwrap();
        assert!(e.starts_with("app/src/main.c: In function 'main':\napp/src/main.c:31:46: error: expected ';'"), "{e}");
        assert!(e.contains("      |                       ^"), "{e}");
        assert!(!e.contains("cascade"), "{e}");
        assert!(!e.contains("ninja"), "{e}");
    }

    #[test]
    fn a_cmake_error_keeps_its_indented_message() {
        let log = "\
-- Board: nrf54l15dk
CMake Error at /work/zephyr/cmake/modules/extensions.cmake:2465 (message):
  BOARD is not being defined on the CMake command-line, in the environment or
  by the app.
Call Stack (most recent call first):
  /work/zephyr/cmake/modules/boards.cmake:100 (zephyr_check_cache)
-- Configuring incomplete, errors occurred!
";
        let e = v(log).first_error.unwrap();
        assert!(e.starts_with("CMake Error at"), "{e}");
        assert!(e.contains("by the app."), "{e}");
        assert!(!e.contains("Call Stack"), "{e}");
    }

    #[test]
    fn a_kconfig_abort_carries_the_warnings_it_aborted_on() {
        let log = "\
Parsing /work/zephyr/Kconfig
warning: FOO (defined at /work/proj/Kconfig:3) was assigned the value 'y' but got the value 'n'
error: Aborting due to Kconfig warnings
";
        let e = v(log).first_error.unwrap();
        assert_eq!(e.lines().count(), 2, "{e}");
        assert!(e.starts_with("warning: FOO"), "{e}");
    }

    #[test]
    fn devicetree_and_linker_errors_are_errors() {
        let dt = "devicetree error: /work/proj/app/board.overlay:4 (column 1): parse error: expected ';'\n";
        assert!(v(dt).first_error.unwrap().starts_with("devicetree error: app/board.overlay:4"));
        let ld = "\
/opt/sdk/bin/ld.bfd: zephyr/zephyr_pre0.elf section `text' will not fit in region `FLASH'
/opt/sdk/bin/ld.bfd: region `FLASH' overflowed by 1234 bytes
collect2: error: ld returned 1 exit status
";
        assert!(v(ld).first_error.unwrap().contains("will not fit in region"));
    }

    #[test]
    fn a_long_line_is_clipped_on_a_character_boundary() {
        let long = format!("/work/proj/a.c:1:1: error: {}", "é".repeat(400));
        let e = v(&long).first_error.unwrap();
        assert!(e.len() <= MAX_LINE_BYTES + "…".len());
        assert!(e.ends_with('…'));
    }

    #[test]
    fn ztest_cases_are_one_line_each_with_the_first_failing_assertion() {
        let log = "\
START - test_ok
 PASS - test_ok in 0.001 seconds
START - test_bad
    Assertion failed at /work/proj/tests/main.c:42: proto_test_bad: (len == 4) is false
 FAIL - test_bad in 0.001 seconds
------ TESTSUITE SUMMARY START ------
SUITE FAIL -  50.00% [proto]: pass = 1, fail = 1, skip = 0, total = 2 duration = 0.002 seconds
 - PASS - [proto.test_ok] duration = 0.001 seconds
 - FAIL - [proto.test_bad] duration = 0.001 seconds
------ TESTSUITE SUMMARY END ------
";
        let r = v(log);
        assert_eq!(r.tests, vec!["PASS proto.test_ok", "FAIL proto.test_bad"]);
        assert_eq!(
            r.first_failing_assertion.as_deref(),
            Some("Assertion failed at tests/main.c:42: proto_test_bad: (len == 4) is false")
        );
    }

    #[test]
    fn twister_scenarios_stand_in_when_there_are_no_case_lines() {
        let log = "\
INFO    - 1/2 native_sim                tests/proto/proto.basic                  PASSED (native 0.010s)
ERROR   - 2/2 native_sim                tests/proto/proto.edge                   FAILED : Failed (native 0.012s)
INFO    - 1 of 2 executed test configurations passed (50.00%)
";
        let r = v(log);
        assert_eq!(r.tests, vec!["PASSED native_sim tests/proto/proto.basic", "FAILED native_sim tests/proto/proto.edge"]);
    }

    #[test]
    fn a_log_with_no_tests_lists_none() {
        assert!(v("[1/1] Linking\n").tests.is_empty());
    }

    #[test]
    fn a_memory_delta_covers_the_regions_both_builds_have() {
        let mut a = BTreeMap::new();
        a.insert("FLASH".to_string(), Region { used_bytes: 1500, total_bytes: 4096 });
        a.insert("RAM".to_string(), Region { used_bytes: 100, total_bytes: 512 });
        let mut b = BTreeMap::new();
        b.insert("FLASH".to_string(), Region { used_bytes: 1000, total_bytes: 4096 });
        let d = memory_delta(&a, &b);
        assert_eq!(d["FLASH"]["delta_bytes"], 500);
        assert!(d.get("RAM").is_none());
    }

    #[test]
    fn file_line_handles_columns_none_and_dtc_ranges() {
        assert_eq!(file_line("a/b.c:12:5"), "a/b.c:12");
        assert_eq!(file_line("a/prj.conf:7"), "a/prj.conf:7");
        assert_eq!(file_line("zephyr.dts:12.4-56"), "zephyr.dts:12");
        assert_eq!(file_line("C:\\w\\a.c:3:1"), "C:\\w\\a.c:3");
    }
}
