//! The whitelist of `report.typ`'s syntax tree (`embarch-api` decision 122;
//! `notes/hil/06-report-and-publish.md`, "Verify" check 9).
//!
//! `publish` refuses a source this module refuses, and `verify` runs it again on
//! the `report.typ` the zip carries. It is a rule over the **parsed tree**, not
//! a search for names: it admits a picture only through the plot helper over a
//! signed file (named as `path("...")`) or `image()` of a signed capture, and it
//! refuses what could reach a file, a number or a picture some other way:
//!
//! - `eval`, `read`, `json`, `csv`, `yaml`, `toml`, `cbor`, `xml`, `plugin`,
//!   `bibliography` and `svg`, as an identifier anywhere, and `include`;
//! - every `import` but `"@embarch/report:<version>"`, and from it only the
//!   helper's own names, never renamed;
//! - a helper call or `image` used as a value, which is what an alias is, or
//!   called inside a closure, a show rule or a set rule, which is what a wrapper
//!   is;
//! - a helper call whose file is not `path("<literal>")` of a signed file, or
//!   whose `x`, `y` or `bins` is not a literal (verify derives the plot's axis
//!   ticks from them);
//! - a `footer` argument of `page` (the page number is the compile's alone).
//!
//! A numeral computed from other code is not refused here: check 4 reads the
//! numbers the PDF actually prints.

use typst::syntax::{ast, parse, SyntaxKind, SyntaxNode};

/// The only import a report may make.
pub const HELPER_IMPORT_PREFIX: &str = "@embarch/report:";
/// What the helper package exports, which a report may import.
const HELPER_EXPORTS: [&str; 3] = ["minmax-plot", "m", "plot-ticks"];
/// Admitted calls: they may be called, never used as a value or wrapped.
const ADMITTED: [&str; 4] = ["minmax-plot", "m", "plot-ticks", "image"];
/// Identifiers a report may not use at all.
const FORBIDDEN: [&str; 11] = ["eval", "read", "json", "csv", "yaml", "toml", "cbor", "xml", "plugin", "bibliography", "svg"];
const DEFAULT_BINS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub line: usize,
    pub what: String,
}

/// A `minmax-plot` call, its arguments read as literals.
#[derive(Debug, Clone, PartialEq)]
pub struct Plot {
    pub line: usize,
    pub file: String,
    pub x: String,
    pub y: String,
    pub bins: usize,
}

#[derive(Debug, Default)]
pub struct Verdict {
    pub violations: Vec<Violation>,
    pub plots: Vec<Plot>,
}

/// Whether a path a report names is a file Core signs: what a plot or `image()`
/// may be drawn from.
pub fn is_core_file(name: &str) -> bool {
    matches!(name, "measurements.json" | "report-data.json" | "events.json" | "study.json") || name.starts_with("streams/")
}

struct Walk<'a> {
    src: &'a str,
    is_signed: &'a dyn Fn(&str) -> bool,
    out: Verdict,
    /// Names the helper module is bound to (`import "@embarch/report:..." as r`).
    module_names: Vec<String>,
    wrapped: usize,
}

fn lit_str(n: &SyntaxNode) -> Option<String> {
    n.cast::<ast::Str>().map(|s| s.get().to_string())
}

fn significant(n: &SyntaxNode) -> impl Iterator<Item = &SyntaxNode> {
    n.children().filter(|c| {
        !matches!(c.kind(), SyntaxKind::Space | SyntaxKind::LineComment | SyntaxKind::BlockComment | SyntaxKind::Comma | SyntaxKind::LeftParen | SyntaxKind::RightParen)
    })
}

impl<'a> Walk<'a> {
    fn line(&self, off: usize) -> usize {
        1 + self.src[..off.min(self.src.len())].bytes().filter(|b| *b == b'\n').count()
    }
    fn refuse(&mut self, off: usize, what: String) {
        let line = self.line(off);
        self.out.violations.push(Violation { line, what });
    }

    fn node(&mut self, n: &SyntaxNode, off: usize) {
        match n.kind() {
            SyntaxKind::Error => self.refuse(off, "does not parse".into()),
            SyntaxKind::ModuleInclude => self.refuse(off, "`include` reads a file".into()),
            SyntaxKind::ModuleImport => return self.import(n, off),
            SyntaxKind::Named => {
                // The key is a name, not a use of one.
                let mut o = off;
                let mut key_done = false;
                for c in n.children() {
                    if !key_done && c.kind() == SyntaxKind::Ident {
                        key_done = true;
                    } else {
                        self.node(c, o);
                    }
                    o += c.len();
                }
                return;
            }
            SyntaxKind::FuncCall => return self.call(n, off),
            SyntaxKind::Ident => self.ident(n.leaf_text(), false, off),
            SyntaxKind::Closure | SyntaxKind::ShowRule | SyntaxKind::SetRule => {
                if n.kind() == SyntaxKind::SetRule {
                    self.set_rule(n, off);
                }
                self.wrapped += 1;
                self.children(n, off);
                self.wrapped -= 1;
                return;
            }
            _ => {}
        }
        self.children(n, off);
    }

    fn children(&mut self, n: &SyntaxNode, mut off: usize) {
        for c in n.children() {
            self.node(c, off);
            off += c.len();
        }
    }

    fn ident(&mut self, name: &str, as_callee: bool, off: usize) {
        if FORBIDDEN.contains(&name) {
            self.refuse(off, format!("`{name}` is not admitted (it reads a file, runs code or draws a picture)"));
        } else if ADMITTED.contains(&name) && !as_callee {
            self.refuse(off, format!("`{name}` is used as a value, which is an alias of an admitted call: call it where it is used"));
        }
    }

    fn import(&mut self, n: &SyntaxNode, off: usize) {
        let kids: Vec<&SyntaxNode> = significant(n).collect();
        let source = kids.iter().find(|k| k.kind() != SyntaxKind::Import).copied();
        let path = source.and_then(lit_str);
        match path {
            Some(p) if p.starts_with(HELPER_IMPORT_PREFIX) => {}
            _ => return self.refuse(off, "only `import \"@embarch/report:<version>\"` is admitted".into()),
        }
        let mut bare_alias = true;
        let mut o = off;
        for c in n.children() {
            match c.kind() {
                SyntaxKind::ImportItems => {
                    bare_alias = false;
                    let mut io = o;
                    for item in c.children() {
                        match item.kind() {
                            SyntaxKind::RenamedImportItem => self.refuse(io, "a renamed import is an alias of an admitted call".into()),
                            SyntaxKind::Ident => {
                                if !HELPER_EXPORTS.contains(&item.leaf_text().as_str()) {
                                    self.refuse(io, format!("`{}` is not something the helper exports", item.leaf_text()));
                                }
                            }
                            SyntaxKind::ImportItemPath => {
                                let names: Vec<&SyntaxNode> = item.children().filter(|k| k.kind() == SyntaxKind::Ident).collect();
                                match names.as_slice() {
                                    [one] if HELPER_EXPORTS.contains(&one.leaf_text().as_str()) => {}
                                    _ => self.refuse(io, "an import item that is not one of the helper's own names".into()),
                                }
                            }
                            _ => {}
                        }
                        io += item.len();
                    }
                }
                SyntaxKind::Ident if o > off && source.is_some_and(|s| s.kind() == SyntaxKind::Str) => {
                    // `as <name>`: the module's name.
                    self.module_names.push(c.leaf_text().to_string());
                }
                _ => {}
            }
            o += c.len();
        }
        if bare_alias && !self.module_names.iter().any(|m| m == "report") {
            self.module_names.push("report".into());
        }
    }

    fn set_rule(&mut self, n: &SyntaxNode, off: usize) {
        let name = significant(n).find(|k| k.kind() == SyntaxKind::Ident).map(|k| k.leaf_text().to_string());
        if name.as_deref() == Some("page") {
            self.page_args(n, off);
        }
    }

    fn page_args(&mut self, n: &SyntaxNode, off: usize) {
        let mut o = off;
        for c in n.children() {
            if c.kind() == SyntaxKind::Args {
                let mut ao = o;
                for a in c.children() {
                    if a.kind() == SyntaxKind::Named {
                        let key = significant(a).next().filter(|k| k.kind() == SyntaxKind::Ident).map(|k| k.leaf_text().to_string());
                        if matches!(key.as_deref(), Some("footer" | "footer-descent")) {
                            self.refuse(ao, "sets a page footer, and the footer is the compile's alone (the page number, centred)".into());
                        }
                    }
                    ao += a.len();
                }
            }
            o += c.len();
        }
    }

    /// The callee's name, and whether it is the helper's (a plain name, or a
    /// field of the helper module) or `image`.
    fn call(&mut self, n: &SyntaxNode, off: usize) {
        let mut kids = n.children();
        let Some(callee) = kids.next() else { return };
        let mut admitted: Option<String> = None;
        match callee.kind() {
            SyntaxKind::Ident => {
                let name = callee.leaf_text().to_string();
                self.ident(&name, true, off);
                if ADMITTED.contains(&name.as_str()) {
                    admitted = Some(name);
                } else if name == "page" {
                    self.page_args(n, off);
                }
            }
            SyntaxKind::FieldAccess => {
                let mut fo = off;
                let parts: Vec<&SyntaxNode> = callee.children().collect();
                let base = parts.first().filter(|b| b.kind() == SyntaxKind::Ident).map(|b| b.leaf_text().to_string());
                let field = parts.last().filter(|f| f.kind() == SyntaxKind::Ident).map(|f| f.leaf_text().to_string());
                match (&base, &field) {
                    (Some(b), Some(f)) if self.module_names.contains(b) && ADMITTED.contains(&f.as_str()) && f != "image" => {
                        admitted = Some(f.clone());
                    }
                    (_, Some(f)) if ADMITTED.contains(&f.as_str()) => {
                        self.refuse(off, format!("`{f}` is reached by a path that is not the helper's import: an alias of an admitted call"));
                    }
                    (_, Some(f)) => self.ident(f, true, off),
                    _ => {}
                }
                if let Some(b) = &base {
                    if !self.module_names.contains(b) {
                        self.ident(b, false, off);
                    }
                } else {
                    for p in parts.iter().take(parts.len().saturating_sub(1)) {
                        self.node(p, fo);
                        fo += p.len();
                    }
                }
            }
            _ => self.node(callee, off),
        }
        let args_off = off + callee.len();
        if let Some(name) = &admitted {
            if self.wrapped > 0 {
                self.refuse(off, format!("`{name}` is called inside a closure, show rule or set rule, which wraps an admitted call"));
            }
            self.admitted_call(name, n, args_off, off);
        }
        // Everything inside the arguments is walked like any code.
        let mut o = args_off;
        for c in kids {
            self.node(c, o);
            o += c.len();
        }
    }

    fn admitted_call(&mut self, name: &str, call: &SyntaxNode, args_off: usize, off: usize) {
        let Some(args) = call.children().find(|c| c.kind() == SyntaxKind::Args) else { return };
        let mut positional: Vec<&SyntaxNode> = Vec::new();
        let mut named: Vec<(String, &SyntaxNode)> = Vec::new();
        for a in significant(args) {
            if a.kind() == SyntaxKind::Named {
                let mut parts = significant(a).filter(|k| k.kind() != SyntaxKind::Colon);
                let (Some(k), Some(v)) = (parts.next(), parts.next()) else { continue };
                named.push((k.leaf_text().to_string(), v));
            } else if !matches!(a.kind(), SyntaxKind::ContentBlock) {
                positional.push(a);
            }
        }
        match name {
            "image" => {
                let path = positional.first().and_then(|p| lit_str(p));
                match path {
                    Some(p) if p.starts_with("streams/") && (self.is_signed)(&p) => {}
                    Some(p) => self.refuse(off, format!("`image({p:?})`: only a signed capture (a `streams/` path Core signs) is an admitted picture")),
                    None => self.refuse(off, "`image` of a computed path is not admitted: name a signed capture by a string literal".into()),
                }
            }
            "minmax-plot" | "m" => {
                let file = named.iter().find(|(k, _)| k == "file").map(|(_, v)| *v).or_else(|| {
                    // `m(name, file)` and `minmax-plot(file, ...)`: the file's position differs.
                    if name == "m" { positional.get(1).copied() } else { positional.first().copied() }
                });
                let path = file.and_then(|f| self.path_literal(f));
                let Some(path) = path else {
                    return self.refuse(args_off, format!("`{name}`'s file must be `path(\"<literal>\")` of a file Core signs"));
                };
                if !(self.is_signed)(&path) {
                    return self.refuse(args_off, format!("`{name}` reads {path:?}, which is not a file Core signs"));
                }
                if name == "minmax-plot" {
                    let lit = |key: &str, default: &str| -> Option<String> {
                        match named.iter().find(|(k, _)| k == key) {
                            Some((_, v)) => lit_str(v),
                            None => Some(default.to_string()),
                        }
                    };
                    let bins = match named.iter().find(|(k, _)| k == "bins") {
                        Some((_, v)) if v.kind() == SyntaxKind::Int => v.leaf_text().parse().ok(),
                        Some(_) => None,
                        None => Some(DEFAULT_BINS),
                    };
                    match (lit("x", "x"), lit("y", "y"), bins) {
                        (Some(x), Some(y), Some(bins)) => {
                            let line = self.line(off);
                            self.out.plots.push(Plot { line, file: path, x, y, bins });
                        }
                        _ => self.refuse(args_off, "`minmax-plot`'s `x`, `y` and `bins` must be literals: verify derives the axis ticks from them".into()),
                    }
                }
            }
            _ => {}
        }
    }

    fn path_literal(&self, n: &SyntaxNode) -> Option<String> {
        if n.kind() != SyntaxKind::FuncCall {
            return None;
        }
        let mut kids = n.children();
        let callee = kids.next()?;
        if callee.kind() != SyntaxKind::Ident || callee.leaf_text() != "path" {
            return None;
        }
        let args = kids.find(|c| c.kind() == SyntaxKind::Args)?;
        let mut a = significant(args);
        let first = a.next()?;
        if a.next().is_some() {
            return None;
        }
        lit_str(first)
    }
}

/// Reads `src` and says what the whitelist refuses in it, and which plots it
/// draws. `is_signed` says whether a path a report names is a file Core signs.
pub fn check(src: &str, is_signed: &dyn Fn(&str) -> bool) -> Verdict {
    let root = parse(src);
    let mut w = Walk { src, is_signed, out: Verdict::default(), module_names: Vec::new(), wrapped: 0 };
    w.node(&root, 0);
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(src: &str) -> Vec<String> {
        check(src, &is_core_file).violations.into_iter().map(|v| format!("{}: {}", v.line, v.what)).collect()
    }

    const OK: &str = "#import \"@embarch/report:0.1.0\": minmax-plot, m\n= Title\nRate #m(\"rate\", path(\"measurements.json\")).\n#minmax-plot(path(\"streams/rate.csv\"), x: \"t_s\", y: \"value\", bins: 50, xlabel: [time])\n#image(\"streams/rate.png\")\n";

    #[test]
    fn the_admitted_forms_pass_and_a_plot_is_read() {
        let v = check(OK, &is_core_file);
        assert!(v.violations.is_empty(), "{:?}", v.violations);
        assert_eq!(v.plots, [Plot { line: 4, file: "streams/rate.csv".into(), x: "t_s".into(), y: "value".into(), bins: 50 }]);
    }

    #[test]
    fn file_reads_eval_and_other_imports_are_refused() {
        for src in [
            "#eval(\"1+1\")",
            "#read(\"/etc/passwd\")",
            "#let d = json(\"x.json\")",
            "#include \"other.typ\"",
            "#import \"other.typ\": foo",
            "#import \"@preview/lilaq:0.6.0\" as lq",
            "#import \"@embarch/report:0.1.0\": minmax-plot as mp",
            "#import \"@embarch/report:0.1.0\": _need-path",
            "#bibliography(\"refs.bib\")",
        ] {
            assert!(!refused(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn an_alias_or_a_wrapper_of_an_admitted_call_is_refused() {
        for src in [
            "#import \"@embarch/report:0.1.0\": minmax-plot\n#let p = minmax-plot\n#p(path(\"streams/a.csv\"))",
            "#import \"@embarch/report:0.1.0\": m\n#let f(x) = m(x, path(\"measurements.json\"))\n#f(\"rate\")",
            "#import \"@embarch/report:0.1.0\": m\n#show heading: it => m(\"rate\", path(\"measurements.json\"))",
            "#let i = image\n#i(\"streams/a.png\")",
            "#std.image(\"streams/a.png\")",
            "#image.decode(\"<svg/>\")",
            "#image(\"logo.png\")",
            "#image(read(\"x\"))",
        ] {
            assert!(!refused(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn a_plot_over_a_file_core_does_not_sign_or_a_computed_path_is_refused() {
        let h = "#import \"@embarch/report:0.1.0\": minmax-plot\n";
        assert!(!refused(&format!("{h}#minmax-plot(path(\"mine.csv\"), x: \"a\", y: \"b\")")).is_empty());
        assert!(!refused(&format!("{h}#minmax-plot(\"streams/a.csv\", x: \"a\", y: \"b\")")).is_empty());
        assert!(!refused(&format!("{h}#minmax-plot(path(\"streams/a.csv\"), x: \"a\", y: str(1))")).is_empty());
    }

    #[test]
    fn a_footer_is_refused_by_the_tree_and_a_word_footer_is_not() {
        assert!(!refused("#set page(footer: [mine])").is_empty());
        assert!(!refused("#page(footer: none)[x]").is_empty());
        assert!(refused("The footer: is a word. #set page(margin: 1cm)").is_empty());
    }
}
