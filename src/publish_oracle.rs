//! The numeric-oracle listing of the audit appendix (`embarch-api` decision
//! 115; `notes/hil/02-study.md`, "The run's files are the numeric oracle").
//!
//! Every number in the prose files and in `report.typ`'s text is listed as
//! **cited** (it names a measurement and matches it to the digits written),
//! **tagged** (`(derived)`, `(spec)`, `(inference)`, `(analysis)`), **unbound**
//! (it cites nothing but equals some measurement) or **untagged** (it cites
//! nothing and equals none), and every numeric literal of `report.typ`'s code
//! that is not a length is listed **hardcoded**. The tokens are the grammar of
//! `embarch-study-designer/interfaces/hil-pdf.md`, applied to source text.
//!
//! The scan is **lexical**, not a Typst parse: it tracks markup and code mode
//! and string, comment and bracket nesting, and nothing more. It errs toward
//! listing: a code literal that never reaches the page is still listed.
//! `verify` rescans the sources in the zip rather than trusting this list.

use std::collections::BTreeSet;

use serde_json::Value;

/// How a number stands against the run's measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Cites a measurement by name and equals it at the digits written.
    Cited,
    /// Cites a measurement it does not equal, or one the study does not declare.
    Mismatch,
    /// Carries a tag.
    Tagged,
    /// Cites nothing, equals some measurement.
    Unbound,
    /// Cites nothing, equals none.
    Untagged,
    /// A numeric literal of `report.typ`'s code.
    Hardcoded,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Cited => "cited",
            Kind::Mismatch => "cited, does not match",
            Kind::Tagged => "tagged",
            Kind::Unbound => "unbound",
            Kind::Untagged => "untagged",
            Kind::Hardcoded => "hardcoded",
        }
    }
}

/// One listed number.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub file: String,
    pub line: usize,
    pub text: String,
    pub kind: Kind,
    pub detail: String,
}

/// A file to scan: a prose file's text, or `report.typ`.
pub struct Source<'a> {
    pub name: &'a str,
    pub text: &'a str,
    pub typst: bool,
}

const TAGS: [&str; 4] = ["derived", "spec", "inference", "analysis"];
/// Typst length, angle, ratio and fraction units: a literal with one is layout.
const LAYOUT_UNITS: [&str; 10] = ["pt", "mm", "cm", "in", "em", "fr", "deg", "rad", "%", "ex"];
/// (larger, smaller) units a written number may be converted between.
const UNIT_PAIRS: [(&str, &str); 6] =
    [("V", "mV"), ("s", "ms"), ("A", "mA"), ("W", "mW"), ("kHz", "Hz"), ("MHz", "kHz")];
const CITE_OPEN: char = '\u{1}';
const CITE_CLOSE: char = '\u{2}';
/// A number may sit this many words before its citation or tag (`49.8 Hz [rate]`).
const CITE_REACH: usize = 2;

// ---- the Typst scan ------------------------------------------------------

struct Scan {
    c: Vec<char>,
    i: usize,
    line: usize,
    /// The page's text as far as source shows it: markup text, with `#m("x")`
    /// calls as a marker.
    out: Vec<(char, usize)>,
    hard: Vec<(usize, String)>,
    pics: Vec<(usize, String)>,
    suppress: usize,
}

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl Scan {
    fn new(src: &str) -> Self {
        Scan { c: src.chars().collect(), i: 0, line: 1, out: Vec::new(), hard: Vec::new(), pics: Vec::new(), suppress: 0 }
    }
    fn peek(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }
    fn emit(&mut self, c: char) {
        let l = self.line;
        self.out.push((c, l));
    }
    fn skip_comment(&mut self) -> bool {
        match (self.peek(0), self.peek(1)) {
            (Some('/'), Some('/')) => {
                while let Some(c) = self.peek(0) {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
                true
            }
            (Some('/'), Some('*')) => {
                self.bump();
                self.bump();
                while self.peek(0).is_some() && !(self.peek(0) == Some('*') && self.peek(1) == Some('/')) {
                    self.bump();
                }
                self.bump();
                self.bump();
                true
            }
            _ => false,
        }
    }
    fn skip_ws(&mut self) {
        while self.peek(0).is_some_and(char::is_whitespace) {
            self.bump();
        }
    }
    fn read_ident(&mut self) -> String {
        let mut s = String::new();
        if self.peek(0).is_some_and(ident_start) {
            while let Some(c) = self.peek(0) {
                // A `-` ends an identifier before a digit: `a-1` is a subtraction.
                if ident_char(c) && !(c == '-' && !self.peek(1).is_some_and(ident_start)) {
                    s.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
        }
        s
    }
    /// A string literal, the opening quote under the cursor.
    fn string(&mut self) -> String {
        self.bump();
        let mut s = String::new();
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    if let Some(n) = self.bump() {
                        s.push(n);
                    }
                }
                '"' => break,
                c => s.push(c),
            }
        }
        s
    }

    fn markup(&mut self, close: bool) {
        let mut depth = 0usize;
        while let Some(c) = self.peek(0) {
            if self.skip_comment() {
                continue;
            }
            match c {
                '\\' => {
                    self.bump();
                    if let Some(n) = self.bump() {
                        let l = self.line;
                        self.out.push((n, l));
                    }
                }
                '#' => {
                    self.bump();
                    self.hash_expr();
                }
                '[' => {
                    depth += 1;
                    self.bump();
                    self.emit('[');
                }
                ']' => {
                    self.bump();
                    if depth == 0 && close {
                        return;
                    }
                    depth = depth.saturating_sub(1);
                    self.emit(']');
                }
                _ => {
                    self.bump();
                    self.emit(c);
                }
            }
        }
    }

    fn content_block(&mut self) {
        self.emit(' ');
        self.markup(true);
        self.emit(' ');
    }

    fn hash_expr(&mut self) {
        let ident = self.read_ident();
        if ident.is_empty() {
            match self.peek(0) {
                Some('(') => {
                    self.bump();
                    self.code(Some(')'), false);
                }
                Some('{') => {
                    self.bump();
                    self.code(Some('}'), false);
                }
                _ => self.emit('#'),
            }
            return;
        }
        match ident.as_str() {
            "let" => {
                self.code(None, true);
                return;
            }
            "set" | "show" | "import" | "include" => {
                self.suppress += 1;
                self.code(None, true);
                self.suppress -= 1;
                return;
            }
            _ => {}
        }
        let mut first = Some(ident);
        loop {
            match self.peek(0) {
                Some('.') if self.peek(1).is_some_and(ident_start) => {
                    self.bump();
                    let field = self.read_ident();
                    if first.as_deref().is_some_and(|f| f == "image" || f == "svg") {
                        let l = self.line;
                        self.pics.push((l, format!("{}.{field}", first.as_deref().unwrap_or(""))));
                    }
                    first = Some(field);
                }
                Some('(') => {
                    self.bump();
                    let name = first.take().unwrap_or_default();
                    self.call(&name);
                }
                Some('[') => {
                    self.bump();
                    self.content_block();
                }
                _ => break,
            }
        }
    }

    /// The argument list of a call, the `(` already taken.
    fn call(&mut self, name: &str) {
        self.skip_ws();
        match name {
            "m" => {
                if self.peek(0) == Some('"') {
                    let n = self.string();
                    self.emit(CITE_OPEN);
                    for ch in n.chars() {
                        self.emit(ch);
                    }
                    self.emit(CITE_CLOSE);
                }
                self.suppress += 1;
                self.code(Some(')'), false);
                self.suppress -= 1;
            }
            "image" | "svg" => {
                let l = self.line;
                let path = if self.peek(0) == Some('"') { self.string() } else { String::new() };
                // `image()` of a capture Core signed is the one admitted picture.
                if name == "svg" || !path.starts_with("streams/") {
                    self.pics.push((l, format!("{name}({path:?})")));
                }
                self.suppress += 1;
                self.code(Some(')'), false);
                self.suppress -= 1;
            }
            _ => self.code(Some(')'), false),
        }
    }

    fn code(&mut self, close: Option<char>, stop_at_newline: bool) {
        while let Some(c) = self.peek(0) {
            if self.skip_comment() {
                continue;
            }
            if stop_at_newline && c == '\n' {
                return;
            }
            match c {
                '"' => {
                    self.string();
                }
                '(' => {
                    self.bump();
                    self.code(Some(')'), false);
                }
                '{' => {
                    self.bump();
                    self.code(Some('}'), false);
                }
                '[' => {
                    self.bump();
                    self.content_block();
                }
                ')' | '}' | ']' => {
                    self.bump();
                    if close == Some(c) {
                        return;
                    }
                }
                c if ident_start(c) => {
                    let id = self.read_ident();
                    if (id == "image" || id == "svg") && self.peek(0) == Some('.') {
                        let l = self.line;
                        self.pics.push((l, format!("{id}.…")));
                    }
                    if self.peek(0) == Some('(') {
                        self.bump();
                        self.call(&id);
                    }
                }
                c if c.is_ascii_digit() || (c == '.' && self.peek(1).is_some_and(|d| d.is_ascii_digit())) => {
                    self.number();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn number(&mut self) {
        let line = self.line;
        let mut s = String::new();
        while let Some(c) = self.peek(0) {
            if c.is_ascii_digit() || (c == '.' && self.peek(1).is_some_and(|d| d.is_ascii_digit())) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if matches!(self.peek(0), Some('e' | 'E'))
            && (self.peek(1).is_some_and(|d| d.is_ascii_digit())
                || (matches!(self.peek(1), Some('+' | '-')) && self.peek(2).is_some_and(|d| d.is_ascii_digit())))
        {
            s.push(self.bump().unwrap_or('e'));
            if matches!(self.peek(0), Some('+' | '-')) {
                s.push(self.bump().unwrap_or('+'));
            }
            while self.peek(0).is_some_and(|d| d.is_ascii_digit()) {
                s.push(self.bump().unwrap_or('0'));
            }
        }
        let mut unit = String::new();
        while let Some(c) = self.peek(0) {
            if c.is_ascii_alphabetic() || c == '%' {
                unit.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if self.suppress == 0 && !LAYOUT_UNITS.contains(&unit.as_str()) {
            self.hard.push((line, format!("{s}{unit}")));
        }
    }
}

/// Pictures in `report.typ`'s code that publish refuses: anything but
/// `image()` of a capture, a `streams/` path.
pub fn pictures(report_typ: &str) -> Vec<(usize, String)> {
    let mut s = Scan::new(report_typ);
    s.markup(false);
    s.pics
}

// ---- the number grammar --------------------------------------------------

#[derive(Debug, Clone)]
struct Token {
    text: String,
    value: f64,
    sig: usize,
}

fn tokens(word: &str) -> Vec<Token> {
    let c: Vec<char> = word.chars().collect();
    let n = c.len();
    let is_sign = |ch: char| matches!(ch, '-' | '+' | '\u{2212}');
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let start = i;
        let mut j = i;
        if is_sign(c[i])
            && (i == 0 || !c[i - 1].is_alphanumeric())
            && i + 1 < n
            && (c[i + 1].is_ascii_digit() || (c[i + 1] == '.' && i + 2 < n && c[i + 2].is_ascii_digit()))
        {
            j = i + 1;
        }
        let mant_start = j;
        if j < n && c[j].is_ascii_digit() {
            while j < n && c[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < n && c[j] == '.' && c[j + 1].is_ascii_digit() {
                j += 1;
                while j < n && c[j].is_ascii_digit() {
                    j += 1;
                }
            }
        } else if j + 1 < n && c[j] == '.' && c[j + 1].is_ascii_digit() {
            j += 1;
            while j < n && c[j].is_ascii_digit() {
                j += 1;
            }
        } else {
            i += 1;
            continue;
        }
        let mant_end = j;
        if j < n && matches!(c[j], 'e' | 'E') {
            let mut k = j + 1;
            if k < n && is_sign(c[k]) {
                k += 1;
            }
            if k < n && c[k].is_ascii_digit() {
                while k < n && c[k].is_ascii_digit() {
                    k += 1;
                }
                j = k;
            }
        }
        let text: String = c[start..j].iter().collect();
        let mant: String = c[mant_start..mant_end].iter().filter(|ch| **ch != '.').collect();
        let sig = mant.trim_start_matches('0').len().max(1);
        let norm = text.replace('\u{2212}', "-");
        if let Ok(value) = norm.parse::<f64>() {
            out.push(Token { text, value, sig });
        }
        i = j;
    }
    out
}

fn same_at(sig: usize, x: f64, written: f64) -> bool {
    if x == 0.0 || !x.is_finite() {
        return written == 0.0;
    }
    let rounded: f64 = format!("{:.*e}", sig.saturating_sub(1), x).parse().unwrap_or(f64::NAN);
    (rounded - written).abs() <= 1e-9 * written.abs().max(1e-300)
}

struct Measure {
    name: String,
    value: Option<f64>,
    unit: String,
}

impl Measure {
    fn shown(&self) -> String {
        match self.value {
            Some(v) if self.unit.is_empty() || self.unit == "1" => format!("{} = {v}", self.name),
            Some(v) => format!("{} = {v} {}", self.name, self.unit),
            None => format!("{} (not a number)", self.name),
        }
    }
    fn equals(&self, t: &Token) -> bool {
        let Some(v) = self.value else { return false };
        let mut factors = vec![1.0];
        for (big, small) in UNIT_PAIRS {
            if self.unit == big {
                factors.push(1e3);
            }
            if self.unit == small {
                factors.push(1e-3);
            }
        }
        factors.iter().any(|f| same_at(t.sig, v * f, t.value))
    }
}

fn measures(ms: Option<&Value>) -> Vec<Measure> {
    ms.and_then(|m| m["measurements"].as_array())
        .into_iter()
        .flatten()
        .map(|m| Measure {
            name: m["name"].as_str().unwrap_or("").to_string(),
            value: m["value"].as_f64(),
            unit: m["unit"].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

fn strings_of(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Array(a) => a.iter().for_each(|x| strings_of(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings_of(x, out)),
        _ => {}
    }
}

// ---- the listing ---------------------------------------------------------

enum Item {
    Word(String, usize),
    Cite(String, usize),
}

fn items(chars: &[(char, usize)]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut wline = 0;
    let mut cite: Option<(String, usize)> = None;
    let flush = |word: &mut String, wline: usize, out: &mut Vec<Item>| {
        if !word.is_empty() {
            out.push(Item::Word(std::mem::take(word), wline));
        }
    };
    for &(ch, line) in chars {
        if let Some((name, l)) = cite.as_mut() {
            if ch == CITE_CLOSE {
                let (n, l) = (std::mem::take(name), *l);
                cite = None;
                out.push(Item::Cite(n, l));
            } else {
                name.push(ch);
            }
            continue;
        }
        if ch == CITE_OPEN {
            flush(&mut word, wline, &mut out);
            cite = Some((String::new(), line));
        } else if ch.is_whitespace() {
            flush(&mut word, wline, &mut out);
        } else {
            if word.is_empty() {
                wline = line;
            }
            word.push(ch);
        }
    }
    flush(&mut word, wline, &mut out);
    out
}

#[derive(Clone)]
enum Cite {
    Measurement(String),
    Tag(String),
}

/// A word's `[name]` and `(tag)` groups, and the word with them cut out.
fn groups(word: &str) -> (String, Vec<Cite>) {
    let mut rest = String::new();
    let mut cites = Vec::new();
    let c: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let (open, close) = match c[i] {
            '[' => ('[', ']'),
            '(' => ('(', ')'),
            _ => {
                rest.push(c[i]);
                i += 1;
                continue;
            }
        };
        let end = c[i + 1..].iter().position(|x| *x == close).map(|p| i + 1 + p);
        let inner: String = end.map(|e| c[i + 1..e].iter().collect()).unwrap_or_default();
        let cite = match open {
            '[' if inner.chars().next().is_some_and(ident_start) && inner.chars().all(|x| ident_char(x) || x == '.') => {
                Some(Cite::Measurement(inner.clone()))
            }
            '(' if TAGS.contains(&inner.as_str()) => Some(Cite::Tag(inner.clone())),
            _ => None,
        };
        match (cite, end) {
            (Some(ct), Some(e)) => {
                cites.push(ct);
                i = e + 1;
            }
            _ => {
                rest.push(c[i]);
                i += 1;
            }
        }
    }
    (rest, cites)
}

fn opaque(word: &str, strings: &BTreeSet<String>) -> bool {
    let w = word.trim_start_matches(['(', '[', '"', '\'']);
    let w = w.trim_end_matches([')', ']', '"', '\'', ',', ';', ':']);
    let w = w.strip_suffix('.').unwrap_or(w);
    strings.contains(w)
}

fn list_text(file: &str, chars: &[(char, usize)], ms: &[Measure], strings: &BTreeSet<String>, rows: &mut Vec<Row>) {
    struct Tok {
        t: Token,
        word: usize,
        line: usize,
    }
    struct At {
        cite: Cite,
        word: usize,
        marker: bool,
        line: usize,
    }
    let its = items(chars);
    let mut toks: Vec<Tok> = Vec::new();
    let mut cites: Vec<At> = Vec::new();
    for (k, it) in its.iter().enumerate() {
        match it {
            Item::Cite(name, line) => cites.push(At { cite: Cite::Measurement(name.clone()), word: k, marker: true, line: *line }),
            Item::Word(w, line) => {
                let (rest, found) = groups(w);
                for ct in found {
                    cites.push(At { cite: ct, word: k, marker: false, line: *line });
                }
                if !rest.is_empty() && !opaque(&rest, strings) {
                    for t in tokens(&rest) {
                        toks.push(Tok { t, word: k, line: *line });
                    }
                }
            }
        }
    }
    let mut used = vec![false; cites.len()];
    for tk in &toks {
        let hit = cites.iter().position(|c| c.word >= tk.word && c.word <= tk.word + CITE_REACH);
        let (kind, detail) = match hit.map(|h| {
            used[h] = true;
            &cites[h].cite
        }) {
            Some(Cite::Tag(tag)) => (Kind::Tagged, format!("({tag})")),
            Some(Cite::Measurement(name)) => match ms.iter().find(|m| &m.name == name) {
                Some(m) if m.equals(&tk.t) => (Kind::Cited, m.shown()),
                Some(m) => (Kind::Mismatch, format!("cites {}; the digits written do not match", m.shown())),
                None => (Kind::Mismatch, format!("cites {name:?}, which the study does not declare")),
            },
            None => match ms.iter().find(|m| m.equals(&tk.t)) {
                Some(m) => (Kind::Unbound, format!("equals {} but cites no measurement", m.shown())),
                None => (Kind::Untagged, "cites nothing and equals no measurement".to_string()),
            },
        };
        rows.push(Row { file: file.to_string(), line: tk.line, text: tk.t.text.clone(), kind, detail });
    }
    // A `#m("name")` call renders the measurement itself; no number precedes it.
    for (_, c) in cites.iter().enumerate().filter(|(n, c)| c.marker && !used[*n]) {
        if let Cite::Measurement(name) = &c.cite {
            let (kind, detail) = match ms.iter().find(|m| &m.name == name) {
                Some(m) => (Kind::Cited, format!("{}, rendered from the signed file", m.shown())),
                None => (Kind::Mismatch, format!("cites {name:?}, which the study does not declare")),
            };
            rows.push(Row { file: file.to_string(), line: c.line, text: format!("#m({name:?})"), kind, detail });
        }
    }
}

/// Every number of `sources` listed, in source order, prose files first as given.
pub fn rows(sources: &[Source], measurements: Option<&Value>, report_data: &Value) -> Vec<Row> {
    let ms = measures(measurements);
    let mut strings = BTreeSet::new();
    strings_of(report_data, &mut strings);
    let mut out = Vec::new();
    for s in sources {
        let mut part = Vec::new();
        if s.typst {
            let mut sc = Scan::new(s.text);
            sc.markup(false);
            list_text(s.name, &sc.out, &ms, &strings, &mut part);
            for (line, text) in &sc.hard {
                part.push(Row {
                    file: s.name.to_string(),
                    line: *line,
                    text: text.clone(),
                    kind: Kind::Hardcoded,
                    detail: "a numeric literal in report.typ's code".to_string(),
                });
            }
        } else {
            let mut line = 1;
            let chars: Vec<(char, usize)> = s
                .text
                .chars()
                .map(|c| {
                    let l = line;
                    if c == '\n' {
                        line += 1;
                    }
                    (c, l)
                })
                .collect();
            list_text(s.name, &chars, &ms, &strings, &mut part);
        }
        part.sort_by_key(|r| r.line);
        out.extend(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ms() -> Value {
        json!({"measurements": [
            {"name": "rate", "value": 49.8, "unit": "Hz"},
            {"name": "vdd", "value": 3.3, "unit": "V"},
        ]})
    }

    fn prose(text: &str) -> Vec<Row> {
        rows(&[Source { name: "prose/a.md", text, typst: false }], Some(&ms()), &json!({"study_id": "run42abc"}))
    }

    fn kinds(r: &[Row]) -> Vec<(&str, Kind)> {
        r.iter().map(|r| (r.text.as_str(), r.kind)).collect()
    }

    #[test]
    fn a_number_citing_its_measurement_at_the_digits_written_is_cited() {
        assert_eq!(kinds(&prose("The rate was 49.8 Hz [rate] and 50 Hz [rate].")), [("49.8", Kind::Cited), ("50", Kind::Cited)]);
        // mV and V are one quantity.
        assert_eq!(kinds(&prose("The rail sat at 3300 mV [vdd].")), [("3300", Kind::Cited)]);
    }

    #[test]
    fn a_cited_number_that_does_not_match_is_a_mismatch() {
        let r = prose("The rate was 48.1 Hz [rate] and 2 V [nope].");
        assert_eq!(kinds(&r), [("48.1", Kind::Mismatch), ("2", Kind::Mismatch)]);
        assert!(r[1].detail.contains("does not declare"));
    }

    #[test]
    fn a_tagged_number_is_listed_with_its_tag() {
        let r = prose("A margin of 12 (derived) and 7 ms (spec) and 3 (analysis).");
        assert_eq!(kinds(&r), [("12", Kind::Tagged), ("7", Kind::Tagged), ("3", Kind::Tagged)]);
        assert_eq!(r[1].detail, "(spec)");
    }

    #[test]
    fn a_number_citing_nothing_is_unbound_when_it_equals_a_measurement_and_untagged_when_not() {
        let r = prose("It ran at 49.8 Hz but 17 steps.");
        assert_eq!(kinds(&r), [("49.8", Kind::Unbound), ("17", Kind::Untagged)]);
        assert!(r[0].detail.contains("rate = 49.8 Hz"));
    }

    #[test]
    fn a_word_the_signed_data_holds_is_opaque_and_the_grammar_splits_the_rest() {
        let r = prose("Run run42abc, revision 8, 4.75-5.25 and A-8.");
        assert_eq!(kinds(&r), [("8", Kind::Untagged), ("4.75", Kind::Untagged), ("5.25", Kind::Untagged), ("8", Kind::Untagged)]);
    }

    #[test]
    fn report_typ_lists_m_calls_text_numbers_and_hardcoded_literals() {
        let src = "#import \"@embarch/report:0.1.0\": m\n#set text(size: 10pt)\nRate #m(\"rate\", path(\"measurements.json\")) and 7 widgets (derived), 49.8 Hz.\n#let data = (1, 2.5, 3pt)\n// 99 in a comment\n";
        let r = rows(&[Source { name: "report.typ", text: src, typst: true }], Some(&ms()), &json!({}));
        let k: Vec<_> = r.iter().map(|r| (r.text.as_str(), r.kind, r.line)).collect();
        assert_eq!(
            k,
            [
                ("7", Kind::Tagged, 3),
                ("49.8", Kind::Unbound, 3),
                ("#m(\"rate\")", Kind::Cited, 3),
                ("1", Kind::Hardcoded, 4),
                ("2.5", Kind::Hardcoded, 4),
            ]
        );
    }

    #[test]
    fn an_m_call_after_a_number_is_that_numbers_citation() {
        let src = "About 49.8 Hz #m(\"rate\", path(\"measurements.json\")) sampled.";
        let r = rows(&[Source { name: "report.typ", text: src, typst: true }], Some(&ms()), &json!({}));
        assert_eq!(kinds(&r), [("49.8", Kind::Cited)]);
    }

    #[test]
    fn only_an_image_of_a_capture_is_not_a_picture() {
        assert!(pictures("#image(\"streams/rate.png\")").is_empty());
        assert_eq!(pictures("#image(\"logo.png\", width: 5cm)").len(), 1);
        assert_eq!(pictures("#svg(\"a.svg\")").len(), 1);
        assert_eq!(pictures("#let f = image(\"x.png\")").len(), 1);
        assert_eq!(pictures("#image.decode(\"<svg/>\")").len(), 1);
        assert!(pictures("// #image(\"x.png\")\nan image of 3").is_empty());
    }
}
