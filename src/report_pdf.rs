//! Renders a report's `report.typ` to PDF, offline and reproducibly
//! (`embarch-api` decision 110; HIL milestone M1.9).
//!
//! **No `typst` binary, no network.** Typst is linked as a library; its packages
//! (`typst-packages/`, lilaq and what lilaq imports, plus EmbArch's own plot
//! helper) are embedded in the binary and served from there by [`Resolver`],
//! never from the registry. There is no registry client in this build at all:
//! `typst-as-lib`'s `packages`, `reqwest` and `ureq` features are off.
//!
//! **The compile's root is the staged files and nothing else.** [`Staging`]
//! copies each file a report may read into a private directory by content,
//! refusing a symlink and a hard link; [`Resolver`] serves a project path only
//! from there, refusing a link again at read time. A `report.typ` that reads a
//! path outside the staging directory (`../`, an absolute path) fails to
//! compile.
//!
//! **The same inputs give the same PDF.** The PDF's `ident` and `timestamp`
//! come from the caller, Typst's `datetime.today()` is pinned to the same
//! instant, and fonts are the ones Typst embeds, never the machine's.
//!
//! [`plot_ticks`] is the tick algorithm of `hil-pdf.md`; the Typst helper
//! package (`typst-packages/embarch/report`) carries the same algorithm, and a
//! test holds the two to each other.

use std::borrow::Cow;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use include_dir::{include_dir, Dir};
use sha2::{Digest, Sha256};
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Smart};
use typst::layout::{Frame, FrameItem};
use typst::syntax::{FileId, Source, VirtualRoot};
use typst_as_lib::file_resolver::FileResolver;
use typst_as_lib::time::FixedTimeProvider;
use typst_as_lib::typst_kit_options::TypstKitFontOptions;
use typst_as_lib::TypstEngine;
use typst_layout::PagedDocument;
use typst_pdf::{PdfOptions, Timestamp};

/// The Typst release this build links, recorded in a report's appendix since a
/// PDF's bytes depend on it. A test holds it to `Cargo.lock`.
pub const TYPST_VERSION: &str = "0.15.1";

/// The file `report.typ` is staged as. A report's own file name is fixed so the
/// compile's entry point never depends on what the agent called anything.
pub const REPORT_FILE: &str = "report.typ";

/// What the compile wraps `report.typ` in: the one footer `hil-pdf.md` admits,
/// the page number alone, centred, in ASCII digits. `include` takes the report's
/// content under this page setup.
const WRAPPER: &str = "#set page(footer: context align(center, counter(page).display(\"1\")))\n#include \"/report.typ\"\n";

/// Every package a report may import, embedded at build time. The directory
/// layout is `<namespace>/<name>/<version>/…`, as Typst's own local package
/// root has it.
static PACKAGES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/typst-packages");

#[derive(Debug)]
pub enum ReportPdfError {
    /// A file could not be staged: a link, an escaping name, a read failure.
    Stage(String),
    /// Typst refused the document; the text is Typst's own diagnostics.
    Compile(String),
    /// The compiled document could not be written as PDF.
    Pdf(String),
}

impl fmt::Display for ReportPdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportPdfError::Stage(m) => write!(f, "staging refused: {m}"),
            ReportPdfError::Compile(m) => write!(f, "report did not compile: {m}"),
            ReportPdfError::Pdf(m) => write!(f, "PDF export failed: {m}"),
        }
    }
}

impl std::error::Error for ReportPdfError {}

/// A private directory holding exactly the files a report may read.
pub struct Staging {
    dir: tempfile::TempDir,
}

impl Staging {
    pub fn new() -> Result<Self, ReportPdfError> {
        let dir = tempfile::Builder::new()
            .prefix("embarch-report-")
            .tempdir()
            .map_err(|e| ReportPdfError::Stage(format!("no staging directory: {e}")))?;
        Ok(Self { dir })
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Copies `src`'s content in as `name` (a relative path, no `..`). A
    /// symlink is refused, and so is a file with another hard link: its bytes
    /// are another path's, and a staged copy of a link's target is the way a
    /// report would read what staging never meant to carry.
    pub fn stage_file(&mut self, src: &Path, name: &str) -> Result<(), ReportPdfError> {
        let dest = self.dest(name)?;
        let before = fs::symlink_metadata(src)
            .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", src.display())))?;
        if before.file_type().is_symlink() {
            return Err(ReportPdfError::Stage(format!(
                "{} is a symbolic link",
                src.display()
            )));
        }
        let mut file = fs::File::open(src)
            .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", src.display())))?;
        let after = file
            .metadata()
            .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", src.display())))?;
        if !after.is_file() {
            return Err(ReportPdfError::Stage(format!(
                "{} is not a regular file",
                src.display()
            )));
        }
        refuse_if_linked(src, &before, &after)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", src.display())))?;
        self.write(&dest, &bytes)
    }

    /// Stages generated content as `name`.
    pub fn stage_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), ReportPdfError> {
        let dest = self.dest(name)?;
        self.write(&dest, bytes)
    }

    fn dest(&self, name: &str) -> Result<PathBuf, ReportPdfError> {
        let rel = Path::new(name);
        let ok = !name.is_empty()
            && rel
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
        if !ok {
            return Err(ReportPdfError::Stage(format!(
                "{name:?} is not a plain relative path"
            )));
        }
        Ok(self.dir.path().join(rel))
    }

    fn write(&self, dest: &Path, bytes: &[u8]) -> Result<(), ReportPdfError> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", parent.display())))?;
        }
        if fs::symlink_metadata(dest).is_ok() {
            return Err(ReportPdfError::Stage(format!(
                "{} is staged twice",
                dest.display()
            )));
        }
        fs::write(dest, bytes)
            .map_err(|e| ReportPdfError::Stage(format!("{}: {e}", dest.display())))
    }
}

#[cfg(unix)]
fn refuse_if_linked(
    src: &Path,
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> Result<(), ReportPdfError> {
    use std::os::unix::fs::MetadataExt;
    if after.nlink() > 1 {
        return Err(ReportPdfError::Stage(format!(
            "{} has {} hard links",
            src.display(),
            after.nlink()
        )));
    }
    // The path was swapped between the check and the open.
    if before.ino() != after.ino() || before.dev() != after.dev() {
        return Err(ReportPdfError::Stage(format!(
            "{} changed while staging",
            src.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_if_linked(
    _src: &Path,
    _before: &fs::Metadata,
    _after: &fs::Metadata,
) -> Result<(), ReportPdfError> {
    Ok(())
}

/// What the PDF's identity and dates come from. Both are the caller's, so the
/// same inputs give the same bytes.
pub struct RenderOptions {
    /// A string that names this document stably (the run id and revision).
    pub ident: String,
    /// The instant stamped on the PDF and returned by `datetime.today()`, in
    /// seconds since the Unix epoch (UTC). A publish passes its own time.
    pub timestamp_unix: i64,
}

fn compile(staging: &Staging, opts: &RenderOptions) -> Result<(PagedDocument, Datetime), ReportPdfError> {
    let timestamp = unix_to_datetime(opts.timestamp_unix)
        .ok_or_else(|| ReportPdfError::Compile("timestamp out of range".into()))?;
    let secs = u64::try_from(opts.timestamp_unix)
        .map_err(|_| ReportPdfError::Compile("timestamp before 1970".into()))?;
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);

    let engine = TypstEngine::builder()
        .main_file(("/embarch-main.typ", WRAPPER))
        .search_fonts_with(
            TypstKitFontOptions::new()
                .include_system_fonts(false)
                .include_embedded_fonts(true),
        )
        .time_provider(FixedTimeProvider::new(now))
        .add_file_resolver(Resolver {
            root: staging.root().to_path_buf(),
        })
        .build();

    let doc = engine
        .compile::<PagedDocument>()
        .output
        .map_err(|e| ReportPdfError::Compile(e.to_string()))?;
    Ok((doc, timestamp))
}

/// Renders `report.typ` from `staging` to PDF bytes.
pub fn render_report(staging: &Staging, opts: &RenderOptions) -> Result<Vec<u8>, ReportPdfError> {
    let (doc, timestamp) = compile(staging, opts)?;
    let options = PdfOptions {
        ident: Smart::Custom(opts.ident.clone()),
        timestamp: Some(Timestamp::new_utc(timestamp)),
        ..PdfOptions::default()
    };
    typst_pdf::pdf(&doc, &options).map_err(|e| {
        ReportPdfError::Pdf(
            e.iter()
                .map(|d| d.message.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}

/// The text runs the report draws, page by page, in the order Typst lays them
/// out. For tests of what a report puts on a page; `verify`'s extractor reads
/// the PDF itself (`hil-pdf.md`).
pub fn report_text(staging: &Staging, opts: &RenderOptions) -> Result<Vec<Vec<String>>, ReportPdfError> {
    fn walk(frame: &Frame, out: &mut Vec<String>) {
        for (_, item) in frame.items() {
            match item {
                FrameItem::Text(t) => out.push(t.text.to_string()),
                FrameItem::Group(g) => walk(&g.frame, out),
                _ => {}
            }
        }
    }
    let (doc, _) = compile(staging, opts)?;
    Ok(doc
        .pages()
        .iter()
        .map(|p| {
            let mut runs = Vec::new();
            walk(&p.frame, &mut runs);
            runs
        })
        .collect())
}

/// Serves Typst two kinds of path and no third: a package, from the embedded
/// `typst-packages`; a project file, from the staging directory.
struct Resolver {
    root: PathBuf,
}

impl Resolver {
    fn bytes(&self, id: FileId) -> FileResult<Cow<'static, [u8]>> {
        match id.root() {
            VirtualRoot::Package(spec) => {
                let path = format!(
                    "{}/{}/{}/{}",
                    spec.namespace,
                    spec.name,
                    spec.version,
                    id.vpath().get_without_slash()
                );
                match PACKAGES.get_file(&path) {
                    Some(f) => Ok(Cow::Borrowed(f.contents())),
                    None => Err(FileError::NotFound(PathBuf::from(path))),
                }
            }
            VirtualRoot::Project => {
                let path = id
                    .vpath()
                    .realize(&self.root)
                    .map_err(|_| FileError::AccessDenied)?;
                let meta = fs::symlink_metadata(&path)
                    .map_err(|e| FileError::from_io(e, &path))?;
                if !meta.file_type().is_file() {
                    return Err(FileError::AccessDenied);
                }
                fs::read(&path)
                    .map(Cow::Owned)
                    .map_err(|e| FileError::from_io(e, &path))
            }
        }
    }
}

impl FileResolver for Resolver {
    fn resolve_binary(&self, id: FileId) -> FileResult<Cow<'_, Bytes>> {
        let b = self.bytes(id)?;
        Ok(Cow::Owned(Bytes::new(b.into_owned())))
    }

    fn resolve_source(&self, id: FileId) -> FileResult<Cow<'_, Source>> {
        let b = self.bytes(id)?;
        let text = std::str::from_utf8(&b).map_err(|_| FileError::InvalidUtf8)?;
        Ok(Cow::Owned(Source::new(id, text.to_owned())))
    }
}

/// Seconds since the Unix epoch to a calendar date (Hinnant's civil-from-days).
fn unix_to_datetime(secs: i64) -> Option<Datetime> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Datetime::from_ymd_hms(
        i32::try_from(year).ok()?,
        u8::try_from(month).ok()?,
        u8::try_from(day).ok()?,
        u8::try_from(rem / 3_600).ok()?,
        u8::try_from(rem % 3_600 / 60).ok()?,
        u8::try_from(rem % 60).ok()?,
    )
}

/// One embedded package file, for the zip's list of the packages it carries.
pub struct VendoredFile {
    /// `<namespace>/<name>/<version>/<path>`.
    pub path: String,
    pub sha256: String,
    pub bytes: &'static [u8],
}

/// Every embedded package file (not the provenance note), in path order. A zip carries these so its PDF
/// can be rebuilt with no repo and no network; `verify` hashes them against the
/// release's list.
pub fn vendored_files() -> Vec<VendoredFile> {
    fn walk(dir: &'static Dir<'static>, out: &mut Vec<VendoredFile>) {
        for f in dir.files() {
            out.push(VendoredFile {
                path: f.path().to_string_lossy().replace('\\', "/"),
                sha256: hex(&Sha256::digest(f.contents())),
                bytes: f.contents(),
            });
        }
        for d in dir.dirs() {
            walk(d, out);
        }
    }
    let mut out = Vec::new();
    // The root's own files (the provenance note) are not packages.
    for d in PACKAGES.dirs() {
        walk(d, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// The plot helper's tick algorithm (`hil-pdf.md`, "The plot helper's tick
// algorithm"). Linear axes only.
// ---------------------------------------------------------------------------

/// How many ticks an axis aims for. Assumed (`hil-pdf.md`), not measured.
pub const TICK_TARGET: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Ticks {
    /// 1, 2 or 5 (0 for a one-tick axis).
    pub step_mantissa: u8,
    pub step_exp10: i32,
    pub first_k: i64,
    pub last_k: i64,
    /// Digits after the point in every tick text.
    pub decimals: usize,
    pub texts: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TickError {
    /// `lo` or `hi` is not finite, or `lo > hi`.
    Refused,
    /// The range is too wide for the tick index to be exact.
    OutOfRange,
}

/// 10^e for the tick step: exact for `e >= 0`, and for `e < 0` the correctly
/// rounded `1 / 10^-e`, the same operation the Typst helper performs.
fn pow10(e: i32) -> f64 {
    if e >= 0 {
        10f64.powi(e)
    } else {
        1.0 / 10f64.powi(-e)
    }
}

fn step_of(m: u8, e: i32) -> f64 {
    if e >= 0 {
        f64::from(m) * pow10(e)
    } else {
        f64::from(m) / 10f64.powi(-e)
    }
}

/// The ticks of an axis whose series span `lo..=hi`.
pub fn plot_ticks(lo: f64, hi: f64) -> Result<Ticks, TickError> {
    if !lo.is_finite() || !hi.is_finite() || lo > hi {
        return Err(TickError::Refused);
    }
    if lo == hi {
        let text = if lo == 0.0 {
            "0".to_string()
        } else {
            format!("{lo}")
        };
        let decimals = text.split('.').nth(1).map_or(0, str::len);
        return Ok(Ticks {
            step_mantissa: 0,
            step_exp10: 0,
            first_k: 0,
            last_k: 0,
            decimals,
            texts: vec![text],
        });
    }
    let raw = (hi - lo) / f64::from(TICK_TARGET - 1);
    let mut e = raw.log10().floor() as i32;
    while pow10(e) > raw {
        e -= 1;
    }
    while pow10(e + 1) <= raw {
        e += 1;
    }
    let f = raw / pow10(e);
    let mut m: u8 = if f < 1.5 {
        1
    } else if f < 3.0 {
        2
    } else if f < 7.0 {
        5
    } else {
        10
    };
    if m == 10 {
        m = 1;
        e += 1;
    }
    let step = step_of(m, e);
    if (lo / step).abs() > 1e15 || (hi / step).abs() > 1e15 {
        return Err(TickError::OutOfRange);
    }
    let mut first_k = (lo / step).floor() as i64;
    while first_k as f64 * step > lo {
        first_k -= 1;
    }
    while (first_k + 1) as f64 * step <= lo {
        first_k += 1;
    }
    let mut last_k = (hi / step).ceil() as i64;
    while (last_k as f64) * step < hi {
        last_k += 1;
    }
    while ((last_k - 1) as f64) * step >= hi {
        last_k -= 1;
    }
    let decimals = usize::try_from((-e).max(0)).unwrap_or(0);
    let texts = (first_k..=last_k)
        .map(|k| tick_text(k * i64::from(m), e, decimals))
        .collect();
    Ok(Ticks {
        step_mantissa: m,
        step_exp10: e,
        first_k,
        last_k,
        decimals,
        texts,
    })
}

/// `n × 10^e` written with `decimals` digits after the point, by integer
/// arithmetic; an ASCII `-`, and `-0` written as `0`.
fn tick_text(n: i64, e: i32, decimals: usize) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let a = n.unsigned_abs();
    if e >= 0 {
        let v = a * 10u64.pow(e as u32);
        return format!("{}{v}", if v == 0 { "" } else { sign });
    }
    let scale = 10u64.pow(decimals as u32);
    let (ip, fp) = (a / scale, a % scale);
    let sign = if a == 0 { "" } else { sign };
    format!("{sign}{ip}.{fp:0>decimals$}")
}
