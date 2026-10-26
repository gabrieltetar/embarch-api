//! `bench_exchange` / `bench-exchange` (`tasks/api/131`, decision 89): run a
//! sequence of console commands and return every listened console as one
//! filtered timeline, in one call.
//!
//! **Core's receive time when Core sends it, else the client's.** A capture
//! read from a Core with `embarch-core` decision 94 carries each line's
//! first-byte time (`started_utc_ms + rx_ms`), and an exchange reply its
//! `started_utc_ms`; those place a row exactly. Against an older Core the row
//! falls back to the moment a poll first saw the line (a ceiling, within
//! `poll_ms`, decision 89). A write is always stamped by the client just
//! before its request. Every answer says which basis each signal used
//! (`time_basis`), so a mixed timeline never reads as exact.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use embarch_core_client::CoreClient;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;

/// Per-signal basis, assembled by `time_basis`.
const BASIS_HEAD: &str = "ms since the captures started; a write is stamped by this client just before its request";

/// Says which basis each row used. `core` and `polled` count rows by signal
/// (a signal can have both when Core sent times for only part of a capture).
pub fn time_basis(core: &BTreeMap<String, usize>, polled: &BTreeMap<String, usize>) -> String {
    let mut out = String::from(BASIS_HEAD);
    let list = |m: &BTreeMap<String, usize>| m.keys().cloned().collect::<Vec<_>>().join(", ");
    if !core.is_empty() {
        out.push_str(&format!("; received rows of [{}] are Core's first-byte receive times (exact to the port's read latency, wall clocks aligned at capture start)", list(core)));
    }
    if !polled.is_empty() {
        out.push_str(&format!("; received rows of [{}] are stamped when a poll first saw them (a ceiling, within poll_ms): Core stamps no per-line time", list(polled)));
    }
    if core.is_empty() && polled.is_empty() {
        out.push_str("; no received rows");
    }
    out
}

pub const DEFAULT_POLL_MS: u64 = 100;
const MIN_POLL_MS: u64 = 20;
/// Core's default read deadline for an exchange (decision 79), used only to
/// size a capture's `max_s`.
const CORE_DEFAULT_TIMEOUT_MS: u64 = 2000;
const CORE_MAX_CAPTURE_S: u64 = 600;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct Step {
    /// A declared signal on a direct route.
    pub signal: String,
    /// The line to send; a newline is added when missing.
    pub write: String,
    /// Stop reading the reply when this appears (a shell prompt).
    #[serde(default)]
    pub until: Option<String>,
    /// Read deadline for this step; Core's default and cap apply.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Wait this long before this step (ms).
    #[serde(default)]
    pub delay_ms: Option<u64>,
    /// false for a console with echo off, as in signal_exchange.
    #[serde(default)]
    pub match_after_echo: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct Filter {
    /// Keep only lines containing one of these (case-sensitive substring).
    #[serde(default)]
    pub include: Vec<String>,
    /// Drop lines containing any of these (case-sensitive substring).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Zephyr log-level floor: err, wrn, inf or dbg. "wrn" keeps <err> and
    /// <wrn> lines and drops <inf> and <dbg>; a line with no level tag is kept.
    #[serde(default)]
    pub level: Option<String>,
    /// Keep at most this many lines of the signal; the rest are counted.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct Request {
    /// Commands run in order; may be empty when `snapshot` or `listen` is given.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Signals recorded from before the first step until tail_ms after the last.
    #[serde(default)]
    pub listen: Vec<String>,
    /// Keep recording this long after the last step (ms, default 0).
    #[serde(default)]
    pub tail_ms: Option<u64>,
    /// Per-signal filters, keyed by signal name; "*" applies to any signal
    /// without its own entry.
    #[serde(default)]
    pub filters: HashMap<String, Filter>,
    /// A snapshot declared under `[projects.snapshots.<name>]`: its steps run
    /// after `steps`, its `listen` joins this one's.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Which project's snapshot; optional when only one project declares it.
    #[serde(default)]
    pub project: Option<String>,
    /// How often listened consoles are read (ms, default 100, minimum 20).
    #[serde(default)]
    pub poll_ms: Option<u64>,
}

// ---------------------------------------------------------------- timeline

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Write,
    Recv,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub ms: u64,
    pub seq: usize,
    pub signal: String,
    pub kind: Kind,
    pub text: String,
    /// Placed by Core's receive time, not this client's clock.
    pub core: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub filtered: usize,
    pub capped: usize,
}

/// Splits text into non-empty lines, dropping the CR of a CRLF. Escape codes
/// stay (decision 89).
pub fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()).map(str::to_string).collect()
}

fn level_rank(tag: &str) -> Option<u8> {
    match tag.trim_matches(|c| c == '<' || c == '>').to_ascii_lowercase().as_str() {
        "err" | "error" => Some(1),
        "wrn" | "warn" | "warning" => Some(2),
        "inf" | "info" => Some(3),
        "dbg" | "debug" => Some(4),
        _ => None,
    }
}

/// The Zephyr log level a line carries, searched anywhere in it so a leading
/// `\x1b[1;33m` does not hide it.
pub fn line_level(line: &str) -> Option<u8> {
    [("<err>", 1), ("<wrn>", 2), ("<inf>", 3), ("<dbg>", 4)]
        .iter()
        .filter_map(|(tag, rank)| line.find(tag).map(|at| (at, *rank)))
        .min_by_key(|(at, _)| *at)
        .map(|(_, r)| r)
}

impl Filter {
    pub fn validate(&self, signal: &str) -> Result<(), String> {
        if let Some(l) = &self.level {
            if level_rank(l).is_none() {
                return Err(format!("filters.{signal}.level \"{l}\" is not one of err, wrn, inf, dbg"));
            }
        }
        Ok(())
    }

    fn keeps(&self, line: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|p| line.contains(p.as_str())) {
            return false;
        }
        if self.exclude.iter().any(|p| line.contains(p.as_str())) {
            return false;
        }
        if let (Some(floor), Some(lvl)) = (self.level.as_deref().and_then(level_rank), line_level(line)) {
            if lvl > floor {
                return false;
            }
        }
        true
    }
}

/// Orders rows by time (ties by arrival order), applies each signal's filter
/// to its received rows (writes are never filtered), then its line cap.
pub fn build_timeline(mut rows: Vec<Row>, filters: &HashMap<String, Filter>) -> (Vec<Row>, BTreeMap<String, Dropped>) {
    rows.sort_by_key(|r| (r.ms, r.seq));
    let mut dropped: BTreeMap<String, Dropped> = BTreeMap::new();
    let mut kept_per_signal: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.kind == Kind::Recv {
            let f = filters.get(&row.signal).or_else(|| filters.get("*"));
            if let Some(f) = f {
                if !f.keeps(&row.text) {
                    dropped.entry(row.signal.clone()).or_default().filtered += 1;
                    continue;
                }
                if let Some(cap) = f.max_lines {
                    let n = kept_per_signal.entry(row.signal.clone()).or_default();
                    if *n >= cap {
                        dropped.entry(row.signal.clone()).or_default().capped += 1;
                        continue;
                    }
                    *n += 1;
                }
            }
        }
        out.push(row);
    }
    (out, dropped)
}

/// `+1234 dut> help` for a write, `+1250 dut| text` for a received line.
pub fn render(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| format!("+{} {}{} {}", r.ms, r.signal, if r.kind == Kind::Write { '>' } else { '|' }, r.text))
        .collect::<Vec<_>>()
        .join("\n")
}

// ----------------------------------------------------------- orchestration

struct Listener {
    signal: String,
    id: String,
    next: u64,
    partial: String,
    done: bool,
    state: String,
    total_bytes: u64,
    gaps: Vec<embarch_core_client::CaptureGap>,
    note: Option<String>,
    rows: Vec<Row>,
    /// UTC ms the capture started, when Core said (decision 94).
    started_utc_ms: Option<u64>,
    /// Receive times (`rx_ms`) of lines whose first byte arrived, oldest
    /// first, not yet paired with a completed line.
    rx_queue: std::collections::VecDeque<u64>,
}

/// Maps Core's UTC clock onto this timeline: `utc_origin` is the earliest
/// capture start, which lands at `anchor_ms` on the client's clock.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    utc_origin: u64,
    anchor_ms: u64,
}

impl Anchor {
    fn ms(&self, utc: u64) -> u64 {
        self.anchor_ms + utc.saturating_sub(self.utc_origin)
    }
}

/// A step the exchange answered, as the caller sees it. `reply_in` says where
/// the reply text is.
fn step_json(n: usize, s: &Step, r: &embarch_core_client::SignalExchangeResponse, listened: bool) -> Value {
    let mut v = json!({
        "n": n, "signal": s.signal, "elapsed_ms": r.elapsed_ms, "until_seen": r.until_seen,
        "truncated": r.truncated, "bytes_read": r.bytes_read.unwrap_or(r.text.len()),
        "reply_in": if listened { "capture rows" } else { "timeline rows" },
    });
    if let Some(w) = r.written {
        v["written"] = json!(w);
    }
    if r.port_lost == Some(true) {
        v["port_lost"] = json!(true);
    }
    if let Some(e) = r.echo_seen {
        v["echo_seen"] = json!(e);
    }
    v
}

/// Resolves a request against config: appends the snapshot's steps and
/// listen signals. Errors name the snapshot and the choices.
pub fn resolve(config: &Config, mut req: Request) -> Result<Request, String> {
    if let Some(name) = req.snapshot.clone() {
        let mut found = Vec::new();
        for p in &config.projects {
            if req.project.as_deref().is_some_and(|w| w != p.name) {
                continue;
            }
            if let Some(s) = p.snapshots.get(&name) {
                found.push((p.name.clone(), s.clone()));
            }
        }
        let snap = match found.len() {
            1 => found.remove(0).1,
            0 => {
                let mut known: Vec<String> = config
                    .projects
                    .iter()
                    .flat_map(|p| p.snapshots.keys().map(move |k| format!("{}/{}", p.name, k)))
                    .collect();
                known.sort();
                return Err(format!(
                    "no snapshot '{name}'{}; declared: {}",
                    req.project.as_deref().map(|p| format!(" in project '{p}'")).unwrap_or_default(),
                    if known.is_empty() { "none".to_string() } else { known.join(", ") }
                ));
            }
            _ => {
                let names: Vec<String> = found.into_iter().map(|(p, _)| p).collect();
                return Err(format!("snapshot '{name}' is declared in several projects ({}); pass project", names.join(", ")));
            }
        };
        for s in snap.steps {
            req.steps.push(Step {
                signal: s.signal,
                write: s.write,
                until: s.until,
                timeout_ms: s.timeout_ms,
                delay_ms: s.delay_ms,
                match_after_echo: None,
            });
        }
        for l in snap.listen {
            if !req.listen.contains(&l) {
                req.listen.push(l);
            }
        }
        if req.tail_ms.is_none() {
            req.tail_ms = snap.tail_ms;
        }
    }
    if req.steps.is_empty() && req.listen.is_empty() {
        return Err("nothing to do: give steps, listen or a snapshot".to_string());
    }
    for (sig, f) in &req.filters {
        f.validate(sig)?;
    }
    for (i, s) in req.steps.iter().enumerate() {
        if s.signal.trim().is_empty() || s.write.trim().is_empty() {
            return Err(format!("step {} needs a non-empty signal and write", i + 1));
        }
    }
    Ok(req)
}

/// Reads one listener's capture until Core has nothing more, stamping each
/// complete line `ms`. `last` also flushes an unterminated tail (a prompt).
async fn drain(core: &CoreClient, l: &mut Listener, ms: u64, seq: &mut usize, last: bool, anchor: Option<Anchor>) {
    if l.done {
        return;
    }
    loop {
        let r = if last {
            core.signal_capture_stop(&l.signal, Some(l.next), None, Some(&l.id)).await
        } else {
            core.signal_capture_read(&l.signal, Some(l.next), None, Some(&l.id)).await
        };
        match r {
            Ok(resp) => {
                l.partial.push_str(&resp.text);
                l.next = resp.next_offset.max(l.next);
                l.state = resp.state.clone();
                l.total_bytes = resp.total_bytes;
                l.gaps = resp.gaps.clone();
                if resp.started_utc_ms.is_some() {
                    l.started_utc_ms = resp.started_utc_ms;
                }
                // `lines` entries pair with lines in order: one per line start.
                l.rx_queue.extend(resp.lines.iter().flatten().map(|x| x.rx_ms));
                while let Some(at) = l.partial.find('\n') {
                    let line: String = l.partial.drain(..=at).collect();
                    push_line(l, &line, ms, seq, anchor);
                }
                if last {
                    let tail = std::mem::take(&mut l.partial);
                    push_line(l, &tail, ms, seq, anchor);
                    l.done = true;
                    return;
                }
                if resp.text.is_empty() || l.next >= resp.total_bytes {
                    return;
                }
            }
            Err(e) => {
                l.note = Some(format!("{} capture read failed: {e:#}", if last { "stop" } else { "poll" }));
                l.done = true;
                return;
            }
        }
    }
}

/// Adds one raw line's rows, placed by its Core receive time when Core sent
/// one (and an anchor exists), else at `ms`. A raw line is always one queue
/// entry, even when it is blank and yields no row.
fn push_line(l: &mut Listener, raw: &str, ms: u64, seq: &mut usize, anchor: Option<Anchor>) {
    if raw.is_empty() {
        return;
    }
    let rx = l.rx_queue.pop_front();
    let placed = match (anchor, l.started_utc_ms, rx) {
        (Some(a), Some(s), Some(rx)) => Some(a.ms(s + rx)),
        _ => None,
    };
    for t in split_lines(raw) {
        l.rows.push(Row { ms: placed.unwrap_or(ms), seq: *seq, signal: l.signal.clone(), kind: Kind::Recv, text: t, core: placed.is_some() });
        *seq += 1;
    }
}

pub async fn run(core: &CoreClient, req: Request) -> Result<Value, String> {
    let poll = Duration::from_millis(req.poll_ms.unwrap_or(DEFAULT_POLL_MS).max(MIN_POLL_MS));
    let tail = Duration::from_millis(req.tail_ms.unwrap_or(0));

    // Every signal must be declared, checked before anything is written.
    let declared: HashSet<String> = core
        .list_signals()
        .await
        .map_err(|e| format!("list_signals failed: {e:#}"))?
        .into_iter()
        .map(|s| s.name)
        .collect();
    let mut wanted: Vec<&str> = req.steps.iter().map(|s| s.signal.as_str()).chain(req.listen.iter().map(String::as_str)).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let unknown: Vec<&str> = wanted.iter().copied().filter(|w| !declared.contains(*w)).collect();
    if !unknown.is_empty() {
        let mut have: Vec<&str> = declared.iter().map(String::as_str).collect();
        have.sort_unstable();
        return Err(format!(
            "signal(s) not declared: {}; declared: {}",
            unknown.join(", "),
            if have.is_empty() { "none".to_string() } else { have.join(", ") }
        ));
    }

    let mut listen: Vec<String> = Vec::new();
    for l in &req.listen {
        if !listen.contains(l) {
            listen.push(l.clone());
        }
    }
    let budget_ms: u64 = req.steps.iter().map(|s| s.delay_ms.unwrap_or(0) + s.timeout_ms.unwrap_or(CORE_DEFAULT_TIMEOUT_MS)).sum::<u64>()
        + req.tail_ms.unwrap_or(0);
    let max_s = (budget_ms / 1000 + 10).min(CORE_MAX_CAPTURE_S);

    // Start the captures; on any failure stop the ones already running.
    let mut listeners: Vec<Listener> = Vec::new();
    for sig in &listen {
        match core.signal_capture_start(sig, Some(max_s), None).await {
            Ok(r) => listeners.push(Listener {
                signal: sig.clone(),
                id: r.id,
                next: 0,
                partial: String::new(),
                done: false,
                state: r.state,
                total_bytes: 0,
                gaps: Vec::new(),
                note: None,
                rows: Vec::new(),
                started_utc_ms: r.started_utc_ms,
                rx_queue: Default::default(),
            }),
            Err(e) => {
                for l in &listeners {
                    let _ = core.signal_capture_stop(&l.signal, None, Some(1), Some(&l.id)).await;
                }
                return Err(format!("capture_start on {sig} failed, nothing was written: {e:#}"));
            }
        }
    }

    let t0 = Instant::now();
    let now_ms = move || t0.elapsed().as_millis() as u64;
    // Core's UTC clock lands on this one at the earliest capture start, taken
    // when its start reply reached us (late by the request's latency, never
    // early, so a reply cannot precede its write). Wall clocks are never
    // compared across hosts.
    let anchor = listeners
        .iter()
        .filter_map(|l| l.started_utc_ms)
        .min()
        .map(|utc_origin| Anchor { utc_origin, anchor_ms: now_ms() });
    let finished = AtomicBool::new(false);
    let listened: HashSet<&str> = listen.iter().map(String::as_str).collect();

    let steps_fut = async {
        let mut rows: Vec<Row> = Vec::new();
        let mut summaries: Vec<Value> = Vec::new();
        let mut seq = 0usize;
        let mut failure: Option<String> = None;
        for (i, s) in req.steps.iter().enumerate() {
            if let Some(d) = s.delay_ms.filter(|d| *d > 0) {
                tokio::time::sleep(Duration::from_millis(d)).await;
            }
            let line = crate::tools::exchange_line(&s.write, true);
            let write_ms = now_ms();
            rows.push(Row { ms: write_ms, seq, signal: s.signal.clone(), kind: Kind::Write, text: s.write.trim_end_matches('\n').to_string(), core: false });
            seq += 1;
            match core.signal_exchange_with(&s.signal, &line, s.until.as_deref(), s.timeout_ms, None, s.match_after_echo).await {
                Ok(r) => {
                    let is_listened = listened.contains(s.signal.as_str());
                    // A reply on a captured signal also lands in its capture
                    // (embarch-core decision 92): the capture's rows carry it.
                    if !is_listened {
                        let (ms, by_core) = match (anchor, r.started_utc_ms) {
                            (Some(a), Some(u)) => (a.ms(u).max(write_ms), true),
                            _ => (now_ms(), false),
                        };
                        for t in split_lines(&r.text) {
                            rows.push(Row { ms, seq, signal: s.signal.clone(), kind: Kind::Recv, text: t, core: by_core });
                            seq += 1;
                        }
                    }
                    let mut sj = step_json(i + 1, s, &r, is_listened);
                    let notes = r.wire_notes(&line);
                    if !notes.is_empty() {
                        sj["notes"] = json!(notes);
                    }
                    summaries.push(sj);
                }
                Err(e) => {
                    failure = Some(format!("step {} on {} failed: {e:#}; later steps were not run", i + 1, s.signal));
                    break;
                }
            }
        }
        if failure.is_none() && !tail.is_zero() {
            tokio::time::sleep(tail).await;
        }
        finished.store(true, Ordering::SeqCst);
        (rows, summaries, failure)
    };

    let poll_fut = async {
        let mut seq = 1_000_000usize; // after any step row of the same ms
        loop {
            tokio::time::sleep(poll).await;
            let done = finished.load(Ordering::SeqCst);
            let ms = now_ms();
            for l in listeners.iter_mut() {
                drain(core, l, ms, &mut seq, false, anchor).await;
            }
            if done {
                break;
            }
        }
        // Final read and stop, stamped at the moment it happened.
        let ms = now_ms();
        for l in listeners.iter_mut() {
            drain(core, l, ms, &mut seq, false, anchor).await;
            drain(core, l, ms, &mut seq, true, anchor).await;
        }
        listeners
    };

    let ((mut rows, summaries, failure), listeners) = if listen.is_empty() {
        (steps_fut.await, Vec::new())
    } else {
        tokio::join!(steps_fut, poll_fut)
    };

    let mut captures = serde_json::Map::new();
    for l in &listeners {
        rows.extend(l.rows.iter().cloned());
        let mut c = json!({ "id": l.id, "state": l.state, "total_bytes": l.total_bytes });
        if !l.gaps.is_empty() {
            c["gaps"] = json!(l.gaps);
        }
        if let Some(n) = &l.note {
            c["note"] = json!(n);
        }
        captures.insert(l.signal.clone(), c);
    }
    let (timeline, dropped) = build_timeline(rows, &req.filters);
    let (mut by_core, mut by_poll): (BTreeMap<String, usize>, BTreeMap<String, usize>) = Default::default();
    for r in timeline.iter().filter(|r| r.kind == Kind::Recv) {
        *(if r.core { &mut by_core } else { &mut by_poll }).entry(r.signal.clone()).or_default() += 1;
    }
    let dropped_json: serde_json::Map<String, Value> =
        dropped.iter().map(|(k, d)| (k.clone(), json!({ "filtered": d.filtered, "capped": d.capped }))).collect();

    let mut out = json!({
        "success": failure.is_none(),
        "timeline": render(&timeline),
        "rows": timeline.len(),
        "steps": summaries,
        "dropped": dropped_json,
        "captures": captures,
        "time_basis": time_basis(&by_core, &by_poll),
    });
    if let Some(f) = failure {
        out["error"] = json!(f);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ms: u64, seq: usize, sig: &str, kind: Kind, text: &str) -> Row {
        Row { ms, seq, signal: sig.into(), kind, text: text.into(), core: false }
    }

    #[test]
    fn level_floor_drops_below_and_keeps_untagged_and_escaped_lines() {
        let f = Filter { level: Some("wrn".into()), ..Default::default() };
        assert!(f.keeps("\x1b[1;33m<wrn> pd: hard reset\x1b[0m"));
        assert!(f.keeps("<err> boom"));
        assert!(!f.keeps("\x1b[0m<inf> pd: attached"));
        assert!(!f.keeps("<dbg> x"));
        assert!(f.keeps("uart:~$ "));
        assert!(Filter { level: Some("bogus".into()), ..Default::default() }.validate("s").is_err());
    }

    #[test]
    fn include_exclude_and_cap_count_what_they_drop() {
        let mut filters = HashMap::new();
        filters.insert(
            "dut".to_string(),
            Filter { include: vec!["pd".into()], exclude: vec!["noise".into()], max_lines: Some(2), ..Default::default() },
        );
        let rows = vec![
            row(1, 0, "dut", Kind::Recv, "pd a"),
            row(2, 1, "dut", Kind::Recv, "other"),
            row(3, 2, "dut", Kind::Recv, "pd noise"),
            row(4, 3, "dut", Kind::Recv, "pd b"),
            row(5, 4, "dut", Kind::Recv, "pd c"),
            row(6, 5, "bench", Kind::Recv, "unfiltered"),
        ];
        let (out, dropped) = build_timeline(rows, &filters);
        assert_eq!(render(&out), "+1 dut| pd a\n+4 dut| pd b\n+6 bench| unfiltered");
        assert_eq!(dropped["dut"], Dropped { filtered: 2, capped: 1 });
    }

    #[test]
    fn star_filter_covers_unlisted_signals_and_writes_are_never_filtered() {
        let mut filters = HashMap::new();
        filters.insert("*".to_string(), Filter { exclude: vec!["pd".into()], ..Default::default() });
        let rows = vec![row(1, 0, "bench", Kind::Write, "pd status"), row(2, 1, "bench", Kind::Recv, "pd ok")];
        let (out, dropped) = build_timeline(rows, &filters);
        assert_eq!(render(&out), "+1 bench> pd status");
        assert_eq!(dropped["bench"].filtered, 1);
    }

    #[test]
    fn rows_from_two_consoles_interleave_by_time_then_arrival() {
        let rows = vec![
            row(300, 5, "dut", Kind::Recv, "<inf> pe: swap done"),
            row(10, 0, "bench", Kind::Write, "pd swap"),
            row(300, 4, "bench", Kind::Recv, "ok"),
            row(120, 1, "dut", Kind::Recv, "<inf> pe: swap start"),
        ];
        let (out, _) = build_timeline(rows, &HashMap::new());
        assert_eq!(
            render(&out),
            "+10 bench> pd swap\n+120 dut| <inf> pe: swap start\n+300 bench| ok\n+300 dut| <inf> pe: swap done"
        );
    }

    #[test]
    fn split_lines_drops_cr_and_blank_lines_but_keeps_escapes() {
        assert_eq!(split_lines("a\r\n\r\n\x1b[0mb\r\nuart:~$ "), vec!["a", "\x1b[0mb", "uart:~$ "]);
    }
}
