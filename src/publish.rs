//! `publish <run id>`: a finished Study run as one zip (`embarch-api` decisions
//! 112 and 113; HIL milestone M1.10; layout in `interfaces/publish.md`).
//!
//! The sequence, each step through Core's routes and never Core's disk:
//!
//! 1. the notes, if any, adopted (`POST /study/{id}/adopt`), so `report-data`
//!    sees them;
//! 2. `POST /study/{id}/report-data`, whose revision this publish is;
//! 3. what the report reads, fetched: `measurements.json`, each capture's
//!    rendering, the step outcomes;
//! 4. the PDF, the appendix, the two exports, rendered here from those;
//! 5. `POST /study/{id}/publish` with the SHA-256 of every file this crate
//!    made, which Core lists as embarch-api's and does not vouch for;
//! 6. every file Core produced fetched from the revision's manifest, each
//!    checked against the manifest's digest, and the zip written.
//!
//! The zip is written to the configured folder as `<date>-<slug>-<id8>.zip`
//! (`-r<N>` from the second revision) and is never overwritten. A revision is
//! built from the earlier zip in that folder (decision 117): its captures and
//! `measurements.json` are read from it, not from Core, and the appendix names
//! the revision it supersedes and that zip's manifest hash.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
const PROSE_DIR_LABEL: &str = crate::publish_report::PROSE_DIR;
use base64::Engine as _;
use embarch_core_client::{CoreClient, PublishFile};
use embarch_study_designer::StreamEncoding;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::publish_export::{civil_from_days, junit_xml, ocp_jsonl, ExportInputs, StepRow};
use crate::publish_oracle::{self, Source};
use crate::publish_report::{
    appendix_source, data_section_source, CaptureRow, ProseBlock, ReportInputs, ReportSource, Supersedes,
};
use crate::report_pdf::{render_report, vendored_files, RenderOptions, Staging, APPENDIX_FILE, REPORT_FILE, TYPST_VERSION};
use crate::report_whitelist;

/// The note's name, to Core and in the zip.
pub const NOTES_NAME: &str = "notes/operator-notes.txt";
pub const REPORT_PDF: &str = "report.pdf";
pub const JUNIT_NAME: &str = "exports/junit.xml";
pub const OCP_NAME: &str = "exports/ocp-tv.jsonl";
/// Where the embedded Typst packages sit in the zip.
pub const PACKAGES_DIR: &str = "typst-packages/";
pub const MEASUREMENTS_NAME: &str = "measurements.json";
pub const REPORT_DATA_NAME: &str = "report-data.json";
pub const MANIFEST_NAME: &str = "manifest.dsse.json";
/// The files Core writes into a revision's directory (`embarch-core`
/// decision 116), fetched beside whatever the manifest names.
const CORE_PUBLISH_FILES: [&str; 4] = [REPORT_DATA_NAME, "report-data.dsse.json", "run-log-segment.jsonl", MANIFEST_NAME];

/// What `publish` refuses with no folder configured, and the way out.
pub const NO_FOLDER_MESSAGE: &str = "publish refuses: no publish folder is configured, and there is no default. \
Add to the embarch-api config file:\n\n[publish]\nfolder = \"/path/to/a/folder\"\n\n\
The zip is written there; sending it anywhere else stays a human step.";

pub struct PublishRequest<'a> {
    pub run_id: &'a str,
    /// The operator's words, verbatim; adopted through Core, which records the
    /// token's scope and the text's hash.
    pub notes: Option<&'a str>,
    /// The study's `report.typ`. None: the zip carries the data section this
    /// crate generates.
    pub report_typ: Option<&'a Path>,
    /// The prose files the agent wrote for the run, adopted through Core as
    /// `prose` and carried in the zip.
    pub prose: &'a [ProseBlock],
}

#[derive(Debug)]
pub struct PublishOutcome {
    pub zip: PathBuf,
    pub zip_sha256: String,
    pub revision: u64,
    pub manifest_sha256: String,
    pub report_source: ReportSource,
    /// Every name in the zip, sorted.
    pub files: Vec<String>,
    /// The files embarch-api made, with the hashes it sent Core.
    pub embarch_api_files: Vec<PublishFile>,
    /// The earlier revision this one supersedes.
    pub supersedes: Option<Supersedes>,
    /// Anything that makes the zip less than a complete report. Not empty means
    /// the zip is written and the caller must say so.
    pub warnings: Vec<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `YYYY-MM-DD` (UTC) of a Unix time.
pub fn utc_date(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Lowercase ASCII letters and digits, runs of anything else one `-`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "study".to_string() } else { out }
}

/// `<date>-<slug>-<id8>.zip`, with `-r<N>` for revision 2 and later.
pub fn zip_name(date: &str, slug: &str, run_id: &str, revision: u64) -> String {
    let id8: String = run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    if revision > 1 {
        format!("{date}-{slug}-{id8}-r{revision}.zip")
    } else {
        format!("{date}-{slug}-{id8}.zip")
    }
}

/// A `report.typ` that sets its own footer is refused: the page number is the
/// compile's alone (`hil-pdf.md`).
fn refuse_footer(src: &str) -> Result<()> {
    let mut rest = src;
    while let Some(at) = rest.find("footer") {
        let after = rest[at + "footer".len()..].trim_start();
        if after.starts_with(':') {
            bail!(
                "publish refuses this report.typ: it sets a page footer, and the footer is the compile's \
                 alone (the page number, centred). Remove the `footer:` argument"
            );
        }
        rest = &rest[at + "footer".len()..];
    }
    Ok(())
}

/// The names the manifest Statement lists as Core's subjects, with their digests.
fn manifest_subjects(envelope: &[u8]) -> Result<Vec<(String, String)>> {
    let env: Value = serde_json::from_slice(envelope).context("manifest.dsse.json is not JSON")?;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(env["payload"].as_str().unwrap_or(""))
        .context("manifest payload is not base64")?;
    let statement: Value = serde_json::from_slice(&payload).context("manifest payload is not JSON")?;
    statement["subject"]
        .as_array()
        .ok_or_else(|| anyhow!("manifest has no subject list"))?
        .iter()
        .map(|s| {
            let name = s["name"].as_str().ok_or_else(|| anyhow!("a manifest subject has no name"))?;
            let digest = s["digest"]["sha256"].as_str().ok_or_else(|| anyhow!("subject {name} has no sha256"))?;
            Ok((name.to_string(), digest.to_string()))
        })
        .collect()
}

/// A zip name of this run, and its revision: `<date>-<slug>-<id8>[-r<N>].zip`.
fn revision_of_name(file: &str, id8: &str) -> Option<u64> {
    let stem = file.strip_suffix(".zip")?;
    if stem.ends_with(&format!("-{id8}")) {
        return Some(1);
    }
    let (head, n) = stem.rsplit_once("-r")?;
    if head.ends_with(&format!("-{id8}")) { n.parse().ok().filter(|n| *n >= 2) } else { None }
}

/// An earlier publish of this run found in the folder: its revision, and its
/// files by name.
struct EarlierZip {
    revision: u64,
    files: BTreeMap<String, Vec<u8>>,
}

fn read_zip(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b)?;
        out.insert(f.name()?.to_string(), b);
    }
    Ok(out)
}

/// The run's earlier zips in `folder`, by the revision `report-data.json`
/// inside them says; a name that merely looks like this run's is checked
/// against the run id it holds.
fn earlier_zips(folder: &Path, run_id: &str) -> Vec<EarlierZip> {
    let id8: String = run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(folder) else { return out };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if revision_of_name(&name, &id8).is_none() {
            continue;
        }
        let Ok(files) = read_zip(&e.path()) else { continue };
        let Some(rd) = files.get(REPORT_DATA_NAME).and_then(|b| serde_json::from_slice::<Value>(b).ok()) else { continue };
        if rd["study_id"].as_str() != Some(run_id) {
            continue;
        }
        if let Some(revision) = rd["revision"].as_u64() {
            out.push(EarlierZip { revision, files });
        }
    }
    out.sort_by_key(|z| z.revision);
    out
}

fn count_rows(name: &str, bytes: &[u8]) -> u64 {
    let lines = bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count() as u64;
    if name.ends_with(".csv") { lines.saturating_sub(1) } else { lines }
}

/// Hands Core the earlier zip's Core files for a swept run (`embarch-core`
/// decision 120): `manifest.dsse.json` first, then every Core file it names as a
/// subject. Core's own revision outputs (`report-data.*`, the segment, the
/// manifest) are not restorable and are skipped; embarch-api's files are not
/// subjects. The zip is only read.
async fn restore_from_zip(core: &CoreClient, run_id: &str, z: &EarlierZip) -> Result<()> {
    let manifest = z
        .files
        .get(MANIFEST_NAME)
        .ok_or_else(|| anyhow!("the earlier zip (revision {}) holds no {MANIFEST_NAME}, so Core's files cannot be restored from it", z.revision))?;
    core.restore_file(run_id, MANIFEST_NAME, manifest).await.context("Core refused the earlier zip's manifest")?;
    for (name, _) in manifest_subjects(manifest)?.iter().filter(|(n, _)| !CORE_PUBLISH_FILES.contains(&n.as_str())) {
        let bytes = z
            .files
            .get(name)
            .ok_or_else(|| anyhow!("the earlier zip's manifest names {name}, but the zip does not hold it"))?;
        core.restore_file(run_id, name, bytes).await.with_context(|| format!("Core refused the restored file {name}"))?;
    }
    Ok(())
}

/// Runs the whole publish. `folder` is the configured publish folder, `now_unix`
/// the time stamped on the PDF and the date in the zip's name.
pub async fn run(core: &CoreClient, folder: Option<&Path>, req: &PublishRequest<'_>, now_unix: i64) -> Result<PublishOutcome> {
    let folder = folder.ok_or_else(|| anyhow!(NO_FOLDER_MESSAGE))?;
    if req.run_id.is_empty() || !req.run_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("{:?} is not a run id", req.run_id);
    }
    let run_id = req.run_id;
    let mut warnings: Vec<String> = Vec::new();

    // The study's own report.typ, read before anything is sent to Core.
    let mut staging = Staging::new()?;
    let report_source = match req.report_typ {
        Some(path) => {
            let src = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
            refuse_footer(&src)?;
            staging.stage_file(path, REPORT_FILE)?;
            ReportSource::Study
        }
        None => ReportSource::Generated,
    };
    let study_report_bytes = match report_source {
        ReportSource::Study => Some(std::fs::read(req.report_typ.unwrap())?),
        ReportSource::Generated => None,
    };

    let study_report_text: Option<String> =
        study_report_bytes.as_ref().map(|b| String::from_utf8_lossy(b).into_owned());
    if let Some(src) = &study_report_text {
        let pics = publish_oracle::pictures(src);
        if let Some((line, what)) = pics.first() {
            bail!(
                "publish refuses this report.typ: line {line} draws a picture ({what}). A plot is drawn by the plot helper \
                 from a signed file; any other picture, an embedded image or SVG included, is refused, except `image()` of \
                 a capture Core signed (a `streams/` path)"
            );
        }
    }
    if let Some(src) = &study_report_text {
        // The syntax-tree whitelist `verify` runs again on the zip's report.typ
        // (decision 122): no file read, no other import, no alias or wrapper of an
        // admitted call, no plot over a file Core does not sign.
        let verdict = report_whitelist::check(src, &report_whitelist::is_core_file);
        if let Some(v) = verdict.violations.first() {
            bail!(
                "publish refuses this report.typ: line {}: {}. verify refuses the same source (the whitelist of report.typ's syntax tree)",
                v.line,
                v.what
            );
        }
    }
    let mut prose_names = std::collections::BTreeSet::new();
    for p in req.prose {
        if p.name.is_empty() || p.name.contains('/') || p.name.contains('\\') || p.name == ".." || p.name == "." {
            bail!("{:?} is not a prose file name: a name only, no directory (the zip carries it as {PROSE_DIR_LABEL}<name>)", p.name);
        }
        if !prose_names.insert(p.name.clone()) {
            bail!("the prose file {:?} is given twice", p.name);
        }
    }

    let earlier = earlier_zips(folder, run_id);

    let notes = req.notes.filter(|n| !n.trim().is_empty());
    if let Some(text) = notes {
        core.adopt(run_id, "note", NOTES_NAME, text).await.context("Core refused the notes")?;
    }
    for p in req.prose {
        core.adopt(run_id, "prose", &p.zip_name(), &p.text)
            .await
            .with_context(|| format!("Core refused the prose file {}", p.name))?;
    }

    let rd = match core.report_data(run_id).await {
        Ok(rd) => rd,
        // Core answers 409 for a run whose directory the retention sweep took
        // (`embarch-core` decision 120): hand it the earlier zip's own Core files.
        Err(e) if format!("{e:#}").contains("409") => {
            let Some(z) = earlier.iter().max_by_key(|z| z.revision) else {
                bail!(
                    "Core refused report-data with 409 ({e:#}). If the run's directory is swept, Core needs the earlier revision's own files \
                     back, and publish reads them from an earlier zip in {} (a file named like `<date>-<slug>-{}.zip` or `...-r<N>.zip`, \
                     whose report-data.json names this run); there is none there, so nothing was written (embarch-api decision 117)",
                    folder.display(),
                    run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>()
                );
            };
            restore_from_zip(core, run_id, z).await?;
            core.report_data(run_id).await.context("Core refused report-data after the earlier revision's files were handed back")?
        }
        Err(e) => return Err(e.context("Core refused report-data")),
    };
    let revision = rd.revision;
    // A revision is built from the earlier zip: the highest one below this revision.
    let prev: Option<&EarlierZip> = earlier.iter().filter(|z| z.revision < revision).max_by_key(|z| z.revision);
    let report_data_bytes = core
        .publish_file(run_id, revision, REPORT_DATA_NAME)
        .await?
        .ok_or_else(|| anyhow!("Core did not serve report-data.json for revision {revision}"))?;
    if sha256_hex(&report_data_bytes) != rd.report_data_sha256 {
        bail!("Core's report-data.json does not hash to the report_data_sha256 it answered");
    }
    staging.stage_bytes(REPORT_DATA_NAME, &report_data_bytes)?;

    // What the report reads. measurements.json comes through the revision's
    // files route, which Core is asked to serve before the manifest exists
    // (open.md); a 404 is "absent", and is warned about once the manifest says
    // whether the run has one.
    let measurements_bytes: Option<Vec<u8>> = match prev.and_then(|z| z.files.get(MEASUREMENTS_NAME)) {
        Some(b) => Some(b.clone()),
        None => core.publish_file(run_id, revision, MEASUREMENTS_NAME).await?.map(|b| b.to_vec()),
    };
    let measurements: Option<Value> = match &measurements_bytes {
        Some(b) => {
            staging.stage_bytes(MEASUREMENTS_NAME, b)?;
            Some(serde_json::from_slice(b).context("measurements.json is not JSON")?)
        }
        None => None,
    };
    let mut staged_streams: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if let Some(z) = prev {
        // The earlier zip's renderings, so a run whose captures retention has swept republishes.
        for (name, bytes) in z.files.iter().filter(|(n, _)| n.starts_with("streams/") && (n.ends_with(".csv") || n.ends_with(".txt"))) {
            staging.stage_bytes(name, bytes)?;
            staged_streams.insert(name.clone(), bytes.clone());
        }
    } else if let Some(index) = core.study_streams(run_id).await? {
        for tap in index.streams.iter().filter(|t| t.rendered) {
            let ext = if matches!(tap.encoding, StreamEncoding::Text) { "txt" } else { "csv" };
            let name = format!("streams/{}.{ext}", tap.name);
            let bytes = core.get_study_stream(run_id, &tap.name, false).await?;
            staging.stage_bytes(&name, &bytes)?;
            staged_streams.insert(name, bytes.to_vec());
        }
    }
    let captures: Vec<CaptureRow> =
        staged_streams.iter().map(|(n, b)| CaptureRow { file: n.clone(), rows: count_rows(n, b) }).collect();
    let steps: Vec<StepRow> = core
        .study_steps(run_id)
        .await?
        .map(|s| {
            s.steps
                .into_iter()
                .map(|e| StepRow {
                    name: e.step_name,
                    outcome: e.outcome,
                    reason: e.reason,
                    started_utc_ms: e.started_utc_ms,
                    ended_utc_ms: e.ended_utc_ms,
                })
                .collect()
        })
        .unwrap_or_default();

    // What this revision supersedes: the earlier zip's manifest hash, or Core's copy.
    let supersedes: Option<Supersedes> = match (prev, revision) {
        (_, r) if r <= 1 => None,
        (Some(z), _) => Some(Supersedes {
            revision: z.revision,
            manifest_sha256: z.files.get(MANIFEST_NAME).map(|b| sha256_hex(b)),
            from_zip: true,
        }),
        (None, r) => Some(Supersedes {
            revision: r - 1,
            manifest_sha256: core.publish_file(run_id, r - 1, MANIFEST_NAME).await.ok().flatten().map(|b| sha256_hex(&b)),
            from_zip: false,
        }),
    };

    // The numeric-oracle listing.
    let mut sources: Vec<Source> = req.prose.iter().map(|p| Source { name: &p.name, text: &p.text, typst: false }).collect();
    let report_label = REPORT_FILE;
    if let Some(src) = &study_report_text {
        sources.push(Source { name: report_label, text: src, typst: true });
    }
    let oracle = publish_oracle::rows(&sources, measurements.as_ref(), &rd.report_data);

    // The report's sources, then the PDF.
    let inputs = ReportInputs {
        prose: req.prose,
        oracle: &oracle,
        supersedes: supersedes.as_ref(),
        report_data: &rd.report_data,
        measurements: measurements.as_ref(),
        steps: &steps,
        captures: &captures,
        notes,
        revision,
        api_version: env!("CARGO_PKG_VERSION"),
        typst_version: TYPST_VERSION,
        source: report_source,
    };
    let report_typ_bytes: Vec<u8> = match &study_report_bytes {
        Some(b) => b.clone(),
        None => {
            let src = data_section_source(&inputs);
            staging.stage_bytes(REPORT_FILE, src.as_bytes())?;
            src.into_bytes()
        }
    };
    let appendix = appendix_source(&inputs);
    staging.stage_bytes(APPENDIX_FILE, appendix.as_bytes())?;
    let slug_name = slug(rd.report_data["subject"]["study_name"].as_str().unwrap_or(""));
    let pdf = render_report(&staging, &RenderOptions { ident: format!("{run_id}-r{revision}"), timestamp_unix: now_unix })?;
    let export = ExportInputs {
        slug: &slug_name,
        run_id,
        report_data: &rd.report_data,
        measurements: measurements.as_ref(),
        steps: &steps,
    };

    // embarch-api's files: everything it made.
    let mut api: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    api.insert(REPORT_PDF.into(), pdf);
    api.insert(REPORT_FILE.into(), report_typ_bytes);
    api.insert(APPENDIX_FILE.into(), appendix.into_bytes());
    api.insert(JUNIT_NAME.into(), junit_xml(&export).into_bytes());
    api.insert(OCP_NAME.into(), ocp_jsonl(&export).into_bytes());
    if let Some(text) = notes {
        api.insert(NOTES_NAME.into(), text.as_bytes().to_vec());
    }
    for p in req.prose {
        api.insert(p.zip_name(), p.text.as_bytes().to_vec());
    }
    if report_source == ReportSource::Study {
        for f in vendored_files() {
            api.insert(format!("{PACKAGES_DIR}{}", f.path), f.bytes.to_vec());
        }
    }
    let api_files: Vec<PublishFile> =
        api.iter().map(|(n, b)| PublishFile { name: n.clone(), sha256: sha256_hex(b) }).collect();

    let published = core.publish(run_id, &api_files).await.context("Core refused publish")?;
    if published.revision != revision {
        bail!(
            "Core published revision {} but the report was built for revision {revision}: another publish raced this one. Nothing was written; publish again",
            published.revision
        );
    }

    // Core's files, from the manifest.
    let manifest_bytes = core
        .publish_file(run_id, revision, MANIFEST_NAME)
        .await?
        .ok_or_else(|| anyhow!("Core did not serve manifest.dsse.json for revision {revision}"))?;
    let subjects = manifest_subjects(&manifest_bytes)?;
    let mut core_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (name, digest) in &subjects {
        let bytes = core
            .publish_file(run_id, revision, name)
            .await?
            .ok_or_else(|| anyhow!("Core's manifest names {name} but did not serve it"))?;
        if &sha256_hex(&bytes) != digest {
            bail!("Core served {name} with bytes that do not match the manifest's digest for it");
        }
        core_files.insert(name.clone(), bytes.to_vec());
    }
    for name in CORE_PUBLISH_FILES {
        if name == MANIFEST_NAME {
            continue;
        }
        if !core_files.contains_key(name) {
            let b = core
                .publish_file(run_id, revision, name)
                .await?
                .ok_or_else(|| anyhow!("Core did not serve {name} for revision {revision}"))?;
            core_files.insert(name.to_string(), b.to_vec());
        }
    }
    core_files.insert(MANIFEST_NAME.to_string(), manifest_bytes.to_vec());

    // What the PDF read must be what Core signed.
    if core_files.get(REPORT_DATA_NAME).map(Vec::as_slice) != Some(&report_data_bytes[..]) {
        bail!("report-data.json changed between the render and the manifest; the PDF is built from other bytes");
    }
    match (core_files.get(MEASUREMENTS_NAME), &measurements_bytes) {
        (Some(signed), Some(read)) if signed.as_slice() != &read[..] => {
            bail!("measurements.json changed between the render and the manifest; the PDF is built from other bytes")
        }
        (Some(_), None) => warnings.push(
            "Core's manifest names measurements.json, but Core did not serve it before publish, so the PDF and the exports were built without it. \
             Core must serve a pending revision's files (embarch-api open.md)"
                .into(),
        ),
        _ => {}
    }
    for (name, read) in &staged_streams {
        if let Some(signed) = core_files.get(name) {
            if signed != read {
                bail!("{name} changed between the render and the manifest; the PDF is built from other bytes");
            }
        }
    }
    for name in core_files.keys().filter(|n| n.starts_with("streams/") && (n.ends_with(".csv") || n.ends_with(".txt"))) {
        if !staged_streams.contains_key(name) {
            warnings.push(format!("{name} is in Core's manifest but was not available to the report when it was built"));
        }
    }

    // One namespace: a name both sides produced is a bug in one of them.
    let mut zip_entries: BTreeMap<String, Vec<u8>> = core_files;
    for (name, bytes) in api {
        if zip_entries.insert(name.clone(), bytes).is_some() {
            bail!("embarch-api and Core both produced {name}; nothing was written");
        }
    }

    std::fs::create_dir_all(folder).with_context(|| format!("cannot create the publish folder {}", folder.display()))?;
    let zip_path = folder.join(zip_name(&utc_date(now_unix), &slug_name, run_id, revision));
    let zip_sha256 = write_zip(&zip_path, &zip_entries)?;
    Ok(PublishOutcome {
        zip: zip_path,
        zip_sha256,
        revision,
        manifest_sha256: sha256_hex(&manifest_bytes),
        report_source,
        files: zip_entries.keys().cloned().collect(),
        embarch_api_files: api_files,
        supersedes,
        warnings,
    })
}

/// Writes entries in name order with a fixed time, so the same entries are the
/// same bytes. Refuses to overwrite (`create_new`).
fn write_zip(path: &Path, entries: &BTreeMap<String, Vec<u8>>) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create {} (a publish is never overwritten)", path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, bytes) in entries {
        zip.start_file(name, opts)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(sha256_hex(&std::fs::read(path)?))
}

/// The JSON a tool or `--json` prints.
pub fn outcome_json(o: &PublishOutcome) -> Value {
    serde_json::json!({
        "success": o.warnings.is_empty(),
        "zip": o.zip.display().to_string(),
        "zip_sha256": o.zip_sha256,
        "revision": o.revision,
        "manifest_sha256": o.manifest_sha256,
        "report_source": o.report_source.as_str(),
        "files": o.files,
        "embarch_api_files": o.embarch_api_files.iter().map(|f| serde_json::json!({"name": f.name, "sha256": f.sha256})).collect::<Vec<_>>(),
        "supersedes": o.supersedes.as_ref().map(|s| serde_json::json!({"revision": s.revision, "manifest_sha256": s.manifest_sha256, "from_zip": s.from_zip})),
        "warnings": o.warnings,
    })
}

/// Seconds since the Unix epoch, now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_date_slug_and_the_first_eight_characters() {
        assert_eq!(utc_date(1_763_700_000), "2025-11-21");
        assert_eq!(slug("GATT rate / loss (v2)"), "gatt-rate-loss-v2");
        assert_eq!(slug("!!!"), "study");
        assert_eq!(zip_name("2025-11-21", "gatt", "0123456789abcdef", 1), "2025-11-21-gatt-01234567.zip");
        assert_eq!(zip_name("2025-11-21", "gatt", "0123456789abcdef", 2), "2025-11-21-gatt-01234567-r2.zip");
    }

    #[test]
    fn a_report_that_sets_a_footer_is_refused() {
        assert!(refuse_footer("#set page(footer: [x])").is_err());
        assert!(refuse_footer("#set page(footer : [x])").is_err());
        assert!(refuse_footer("the footer of a page is prose").is_ok());
    }
}
