//! `export-audio`: a framed analog tap as 16-bit PCM mono WAV (`embarch-api`
//! decision 130, EMB-469 decision 12). One implementation behind the MCP tool
//! and the subcommand.
//!
//! **Input is the tap's raw block file** as Core serves it (`raw=1`), read with
//! `embarch_study_designer::analog` (`decode_start_header`, `decode_block`,
//! `IndexTracker`). Gaps and markers are re-derived from the blocks, so no Core
//! route for `events.csv` is needed. What the study declares (rate, block
//! layout, export folder) is read from the Study JSON the run was submitted
//! from, because Core does not serve it back.
//!
//! **One WAV per gap-free segment.** A gap (a skipped block) or an index that
//! goes backwards ends a segment; the gaps are listed in the outcome and in
//! `<stem>-gaps.csv`. Beside each WAV is an Audacity label file (tab-separated
//! start, end, label, in seconds) and the WAV carries the same markers as
//! RIFF `cue ` points with `labl` text. The chunks are written by hand: they
//! are a few dozen lines and no small crate fits.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use embarch_core_client::CoreClient;
use embarch_study_designer::analog::{
    decode_block, decode_start_header, IndexStep, IndexTracker, StartHeader, BLOCK_MAGIC, START_HEADER_BYTES,
    START_MAGIC,
};
use embarch_study_designer::{RecordFraming, Study, StreamSource};

/// Why an export was refused. `Display` says what the study or the capture
/// has to change.
#[derive(Debug)]
pub enum ExportError {
    /// No tap by that name in the study.
    NoSuchTap { tap: String },
    /// The tap declares no audio rate: only an `AnalogInput` source does.
    NoDeclaredRate { tap: String },
    /// The tap has a rate but the study declares no `AnalogBlocks` layout for it.
    NoBlockLayout { tap: String },
    /// A WAV here is mono; the layout has more than one channel.
    NotMono { tap: String, channels: u8 },
    /// No export folder: the study declares no session, and none was given.
    NoExportDir,
    /// The capture does not begin with a start header that decodes.
    NoStartHeader,
    /// The start header declares another channel count than the layout.
    HeaderChannels { header: u8, layout: u8 },
    /// A resolution the int16 scaling cannot express.
    Resolution { bits: u8 },
    /// No block verified.
    NoBlocks,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::NoSuchTap { tap } => write!(f, "the study declares no tap named '{tap}'"),
            ExportError::NoDeclaredRate { tap } => write!(
                f,
                "tap '{tap}' declares no audio rate (its source is not an AnalogInput with a sample_hz), so there is nothing to export as audio"
            ),
            ExportError::NoBlockLayout { tap } => write!(
                f,
                "tap '{tap}' has a rate but no record_checks entry declares its AnalogBlocks layout, so its bytes cannot be cut into blocks"
            ),
            ExportError::NotMono { tap, channels } => write!(
                f,
                "tap '{tap}' has {channels} channels per block; this export writes mono, and which channel to take is not defined"
            ),
            ExportError::NoExportDir => write!(
                f,
                "the study declares no session, so it names no export folder: pass an output folder (out_dir / --out-dir)"
            ),
            ExportError::NoStartHeader => write!(
                f,
                "the capture does not begin with a start header that decodes, so its resolution is unknown and the samples cannot be scaled"
            ),
            ExportError::HeaderChannels { header, layout } => {
                write!(f, "the start header declares {header} channels but the study's layout says {layout}")
            }
            ExportError::Resolution { bits } => {
                write!(f, "the start header declares a resolution of {bits} bits; this export scales 1 to {MAX_RESOLUTION_BITS} (codes are int16 counts)")
            }
            ExportError::NoBlocks => write!(f, "no block in the capture verifies, so there is no audio to export"),
        }
    }
}

impl std::error::Error for ExportError {}

/// A marker edge placed on a segment's sample axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// 1-based ordinal across the whole recording.
    pub number: u32,
    /// Sample offset from the segment's first sample.
    pub sample: u32,
    /// The marker level after the edge.
    pub high: bool,
}

/// One gap-free run of samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Per-channel sample index of the first sample, on the recording's axis.
    pub first_index: u32,
    /// Scaled samples, one per frame.
    pub samples: Vec<i16>,
    pub markers: Vec<Marker>,
}

/// Where a segment ended because samples were missing or the index repeated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapRecord {
    /// 1-based number of the segment that ends at this gap.
    pub after_segment: usize,
    pub expected_index: u32,
    pub got_index: u32,
    /// Frames missing; 0 for an index that went backwards or repeated.
    pub missing_samples: u32,
    pub backwards: bool,
}

/// The capture cut into segments, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub header: StartHeader,
    pub segments: Vec<Segment>,
    pub gaps: Vec<GapRecord>,
    /// Edges the event word counts but does not place, or places outside the block.
    pub unplaced_marker_edges: u32,
    /// Byte spans that were not a verified block.
    pub refused_spans: u32,
}

/// What the tap's declaration says: the rate and layout, from the study.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declared {
    pub sample_hz: u32,
    pub channels: u8,
    pub samples_per_block: u16,
}

/// Reads a `Study` from a JSON file (the one `run_study` was given).
pub fn read_study(path: &Path) -> Result<Study> {
    let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not a Study (JSON)", path.display()))
}

/// The rate and block layout the study declares for `tap`, or the reason it
/// cannot be exported as audio.
pub fn declared(study: &Study, tap: &str) -> Result<Declared, ExportError> {
    let t = study
        .streams
        .iter()
        .find(|t| t.name.as_str() == tap)
        .ok_or_else(|| ExportError::NoSuchTap { tap: tap.to_string() })?;
    let sample_hz = match t.source {
        StreamSource::AnalogInput { sample_hz, .. } if sample_hz > 0 => sample_hz,
        _ => return Err(ExportError::NoDeclaredRate { tap: tap.to_string() }),
    };
    let (channels, samples_per_block) = study
        .record_checks
        .iter()
        .find_map(|c| match c.framing {
            RecordFraming::AnalogBlocks { channels, samples_per_block } if c.stream_id == t.id => {
                Some((channels, samples_per_block))
            }
            _ => None,
        })
        .ok_or_else(|| ExportError::NoBlockLayout { tap: tap.to_string() })?;
    if channels != 1 {
        return Err(ExportError::NotMono { tap: tap.to_string(), channels });
    }
    Ok(Declared { sample_hz, channels, samples_per_block })
}

/// One raw ADC code scaled to fill int16: `(code - mid-scale) * 2^(16 - bits)`.
/// A 12-bit code is multiplied by 16. Codes are read as unsigned counts of
/// `bits` bits; an out-of-range one is clamped rather than wrapped.
pub fn scale_code(code: i16, bits: u8) -> i16 {
    let mid = 1i32 << (bits - 1);
    let scaled = (i32::from(code) - mid) << (16 - bits);
    scaled.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn find_block_magic(bytes: &[u8], from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..].windows(BLOCK_MAGIC.len()).position(|w| w == BLOCK_MAGIC).map(|p| from + p)
}

/// Cuts a raw block file into gap-free segments with their markers.
pub fn decode_capture(raw: &[u8], decl: &Declared) -> Result<Decoded, ExportError> {
    if raw.len() < START_HEADER_BYTES || raw[..START_MAGIC.len()] != START_MAGIC {
        return Err(ExportError::NoStartHeader);
    }
    let header = decode_start_header(&raw[..START_HEADER_BYTES]).map_err(|_| ExportError::NoStartHeader)?;
    if header.channel_count != decl.channels {
        return Err(ExportError::HeaderChannels { header: header.channel_count, layout: decl.channels });
    }
    if header.resolution_bits == 0 || header.resolution_bits > MAX_RESOLUTION_BITS {
        return Err(ExportError::Resolution { bits: header.resolution_bits });
    }
    let block_bytes = embarch_study_designer::analog::block_len(decl.channels, decl.samples_per_block)
        .ok_or(ExportError::NoBlocks)?;

    let mut segments: Vec<Segment> = Vec::new();
    let mut gaps = Vec::new();
    let mut tracker = IndexTracker::new();
    let (mut unplaced, mut refused) = (0u32, 0u32);
    let mut marker_number = 0u32;
    let mut pos = START_HEADER_BYTES;
    while pos < raw.len() {
        let end = pos + block_bytes;
        let block = if raw[pos..].starts_with(&BLOCK_MAGIC) && end <= raw.len() {
            decode_block(&raw[pos..end], decl.channels, decl.samples_per_block).ok()
        } else {
            None
        };
        let Some(block) = block else {
            refused += 1;
            pos = find_block_magic(raw, pos + 1).unwrap_or(raw.len());
            continue;
        };
        pos = end;
        let step = tracker.observe(block.index, decl.samples_per_block);
        let breaks = !matches!(step, IndexStep::InOrder);
        if let IndexStep::Gap { expected, got, missing_samples } = step {
            gaps.push(GapRecord {
                after_segment: segments.len(),
                expected_index: expected,
                got_index: got,
                missing_samples,
                backwards: false,
            });
        } else if let IndexStep::Backwards { expected, got } = step {
            gaps.push(GapRecord {
                after_segment: segments.len(),
                expected_index: expected,
                got_index: got,
                missing_samples: 0,
                backwards: true,
            });
        }
        if breaks || segments.is_empty() {
            segments.push(Segment { first_index: block.index, samples: Vec::new(), markers: Vec::new() });
        }
        let segment = segments.last_mut().expect("a segment was pushed above");
        let base = u32::try_from(segment.samples.len()).unwrap_or(u32::MAX);

        let edges = block.events.edge_count();
        if edges > 0 {
            match block.events.edge_offset() {
                Some(off) if u16::from(off) < block.frames => {
                    marker_number += 1;
                    segment.markers.push(Marker {
                        number: marker_number,
                        sample: base + u32::from(off),
                        high: block.events.edge_level().unwrap_or(false),
                    });
                    unplaced += u32::from(edges) - 1;
                }
                _ => unplaced += u32::from(edges),
            }
        }
        segment.samples.extend(block.codes().map(|c| scale_code(c, header.resolution_bits)));
    }
    if segments.is_empty() {
        return Err(ExportError::NoBlocks);
    }
    Ok(Decoded { header, segments, gaps, unplaced_marker_edges: unplaced, refused_spans: refused })
}

fn marker_text(m: &Marker) -> String {
    format!("marker {} ({})", m.number, if m.high { "high" } else { "low" })
}

/// Raw codes are `i16` counts, so 15 bits is the widest unsigned resolution they hold.
const MAX_RESOLUTION_BITS: u8 = 15;

const RIFF_ALIGN: usize = 2;

fn pad_even(out: &mut Vec<u8>) {
    if !out.len().is_multiple_of(RIFF_ALIGN) {
        out.push(0);
    }
}

fn chunk_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| anyhow!("a WAV chunk of {len} bytes does not fit RIFF's 32-bit size"))
}

/// A 16-bit PCM mono WAV: `fmt `, `data`, then a `cue ` chunk and a
/// `LIST`/`adtl` of `labl` texts when there are markers.
pub fn wav_bytes(samples: &[i16], markers: &[Marker], rate_hz: u32) -> Result<Vec<u8>> {
    const BYTES_PER_SAMPLE: u32 = 2;
    const FMT_BODY: u32 = 16;
    const PCM: u16 = 1;
    const CUE_POINT_BYTES: usize = 24;

    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&FMT_BODY.to_le_bytes());
    body.extend_from_slice(&PCM.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&rate_hz.to_le_bytes());
    body.extend_from_slice(&(rate_hz * BYTES_PER_SAMPLE).to_le_bytes());
    body.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());

    body.extend_from_slice(b"data");
    body.extend_from_slice(&chunk_len(samples.len() * BYTES_PER_SAMPLE as usize)?.to_le_bytes());
    for s in samples {
        body.extend_from_slice(&s.to_le_bytes());
    }
    pad_even(&mut body);

    if !markers.is_empty() {
        body.extend_from_slice(b"cue ");
        body.extend_from_slice(&chunk_len(4 + CUE_POINT_BYTES * markers.len())?.to_le_bytes());
        body.extend_from_slice(&chunk_len(markers.len())?.to_le_bytes());
        for (i, m) in markers.iter().enumerate() {
            body.extend_from_slice(&chunk_len(i + 1)?.to_le_bytes()); // cue point id
            body.extend_from_slice(&m.sample.to_le_bytes()); // play order position
            body.extend_from_slice(b"data");
            body.extend_from_slice(&0u32.to_le_bytes()); // chunk start: the one data chunk
            body.extend_from_slice(&0u32.to_le_bytes()); // block start
            body.extend_from_slice(&m.sample.to_le_bytes()); // sample offset
        }
        let mut adtl = Vec::new();
        adtl.extend_from_slice(b"adtl");
        for (i, m) in markers.iter().enumerate() {
            let text = marker_text(m);
            adtl.extend_from_slice(b"labl");
            adtl.extend_from_slice(&chunk_len(4 + text.len() + 1)?.to_le_bytes());
            adtl.extend_from_slice(&chunk_len(i + 1)?.to_le_bytes());
            adtl.extend_from_slice(text.as_bytes());
            adtl.push(0);
            pad_even(&mut adtl);
        }
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&chunk_len(adtl.len())?.to_le_bytes());
        body.extend_from_slice(&adtl);
    }

    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_len(body.len())?.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// An Audacity label file: `start<TAB>end<TAB>label`, seconds, one line per
/// marker. A marker is a point, so start and end are equal.
pub fn label_file(markers: &[Marker], rate_hz: u32) -> String {
    markers
        .iter()
        .map(|m| {
            let t = f64::from(m.sample) / f64::from(rate_hz);
            format!("{t:.6}\t{t:.6}\t{}\n", marker_text(m))
        })
        .collect()
}

/// One file written for a segment.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentFile {
    pub wav: PathBuf,
    pub labels: PathBuf,
    pub first_index: u32,
    pub samples: usize,
    pub markers: usize,
}

/// What an export wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub export_dir: PathBuf,
    pub sample_hz: u32,
    pub resolution_bits: u8,
    pub files: Vec<SegmentFile>,
    pub gaps: Vec<GapRecord>,
    pub gaps_file: Option<PathBuf>,
    pub warnings: Vec<String>,
}

fn file_stem(study_id: &str, tap: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect()
    };
    format!("{}-{}", clean(study_id), clean(tap))
}

/// Where the study says to write: its session's `export_dir`, a relative one
/// resolved against the folder of the study file (Core resolves it on its own
/// side; this crate has only the file). `out_dir` replaces it.
pub fn export_dir(study: &Study, study_file: Option<&Path>, out_dir: Option<&Path>) -> Result<PathBuf, ExportError> {
    if let Some(d) = out_dir {
        return Ok(d.to_path_buf());
    }
    let session = study.session.as_ref().ok_or(ExportError::NoExportDir)?;
    let dir = Path::new(session.export_dir.as_str());
    if dir.is_absolute() {
        return Ok(dir.to_path_buf());
    }
    Ok(study_file.and_then(Path::parent).map(|p| p.join(dir)).unwrap_or_else(|| dir.to_path_buf()))
}

/// Writes the export of one tap's raw capture. Pure over its inputs apart
/// from the files it writes: no Core, so a test drives it with fixtures.
pub fn export(study: &Study, tap: &str, study_id: &str, raw: &[u8], dir: &Path) -> Result<Outcome> {
    let decl = declared(study, tap)?;
    let decoded = decode_capture(raw, &decl)?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    let rate = decl.sample_hz;
    let mut warnings = Vec::new();
    if decoded.header.sample_rate_hz != rate {
        warnings.push(format!(
            "the start header says {} Hz, the study declares {rate} Hz; the declared rate is used",
            decoded.header.sample_rate_hz
        ));
    }
    if decoded.unplaced_marker_edges > 0 {
        warnings.push(format!(
            "{} marker edge(s) have no place on the sample axis (a block holds only its first edge's offset) and are not in the files",
            decoded.unplaced_marker_edges
        ));
    }
    if decoded.refused_spans > 0 {
        warnings.push(format!("{} span(s) of the capture were not verified blocks and were skipped", decoded.refused_spans));
    }

    let stem = file_stem(study_id, tap);
    let mut files = Vec::new();
    for (i, seg) in decoded.segments.iter().enumerate() {
        let name = format!("{stem}-seg{:02}", i + 1);
        let wav = dir.join(format!("{name}.wav"));
        let labels = dir.join(format!("{name}.txt"));
        std::fs::write(&wav, wav_bytes(&seg.samples, &seg.markers, rate)?)
            .with_context(|| format!("could not write {}", wav.display()))?;
        std::fs::write(&labels, label_file(&seg.markers, rate))
            .with_context(|| format!("could not write {}", labels.display()))?;
        files.push(SegmentFile {
            wav,
            labels,
            first_index: seg.first_index,
            samples: seg.samples.len(),
            markers: seg.markers.len(),
        });
    }
    let gaps_file = if decoded.gaps.is_empty() {
        None
    } else {
        let path = dir.join(format!("{stem}-gaps.csv"));
        let mut csv = String::from("after_segment,expected_index,got_index,missing_samples,kind\n");
        for g in &decoded.gaps {
            csv.push_str(&format!(
                "{},{},{},{},{}\n",
                g.after_segment,
                g.expected_index,
                g.got_index,
                g.missing_samples,
                if g.backwards { "backwards" } else { "gap" }
            ));
        }
        std::fs::write(&path, csv).with_context(|| format!("could not write {}", path.display()))?;
        Some(path)
    };
    Ok(Outcome {
        export_dir: dir.to_path_buf(),
        sample_hz: rate,
        resolution_bits: decoded.header.resolution_bits,
        files,
        gaps: decoded.gaps,
        gaps_file,
        warnings,
    })
}

/// What the tool and the subcommand take.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub study_id: &'a str,
    pub tap: &'a str,
    /// The Study JSON the run was submitted from.
    pub study_file: &'a Path,
    /// Replaces the study's own export folder.
    pub out_dir: Option<&'a Path>,
}

/// The one entry point: reads the study, fetches the tap's raw capture from
/// Core and writes the export. Refuses before fetching when the study alone
/// settles it (no such tap, no rate, no layout, no folder).
pub async fn run(core: &CoreClient, req: &Request<'_>) -> Result<Outcome> {
    let study = read_study(req.study_file)?;
    declared(&study, req.tap)?;
    let dir = export_dir(&study, Some(req.study_file), req.out_dir)?;
    let raw = core
        .get_study_stream(req.study_id, req.tap, true)
        .await
        .with_context(|| format!("could not fetch tap '{}' of study '{}'", req.tap, req.study_id))?;
    if raw.is_empty() {
        bail!("tap '{}' of study '{}' captured nothing", req.tap, req.study_id);
    }
    export(&study, req.tap, req.study_id, &raw, &dir)
}

/// The outcome as JSON, for both front ends.
pub fn outcome_json(o: &Outcome) -> serde_json::Value {
    serde_json::json!({
        "export_dir": o.export_dir.display().to_string(),
        "sample_hz": o.sample_hz,
        "resolution_bits": o.resolution_bits,
        "files": o.files.iter().map(|f| serde_json::json!({
            "wav": f.wav.display().to_string(),
            "labels": f.labels.display().to_string(),
            "first_sample_index": f.first_index,
            "samples": f.samples,
            "markers": f.markers,
        })).collect::<Vec<_>>(),
        "gaps": o.gaps.iter().map(|g| serde_json::json!({
            "after_segment": g.after_segment,
            "expected_index": g.expected_index,
            "got_index": g.got_index,
            "missing_samples": g.missing_samples,
            "backwards": g.backwards,
        })).collect::<Vec<_>>(),
        "gaps_file": o.gaps_file.as_ref().map(|p| p.display().to_string()),
        "warnings": o.warnings,
    })
}

/// A short human rendering of the outcome.
pub fn outcome_text(o: &Outcome) -> String {
    let mut s = format!(
        "exported {} segment(s) at {} Hz ({}-bit source) to {}",
        o.files.len(),
        o.sample_hz,
        o.resolution_bits,
        o.export_dir.display()
    );
    for f in &o.files {
        s.push_str(&format!("\n  {} ({} samples, {} markers)", f.wav.display(), f.samples, f.markers));
    }
    for g in &o.gaps {
        s.push_str(&format!(
            "\n  gap after segment {}: expected index {}, got {} ({} samples missing{})",
            g.after_segment,
            g.expected_index,
            g.got_index,
            g.missing_samples,
            if g.backwards { ", index went backwards" } else { "" }
        ));
    }
    for w in &o.warnings {
        s.push_str(&format!("\n  warning: {w}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_twelve_bit_code_is_centred_and_scaled_by_sixteen() {
        assert_eq!(scale_code(2048, 12), 0);
        assert_eq!(scale_code(2049, 12), 16);
        assert_eq!(scale_code(4095, 12), 2047 * 16);
        assert_eq!(scale_code(0, 12), i16::MIN);
        assert_eq!(scale_code(16384, 15), 0);
        assert_eq!(scale_code(16385, 15), 2);
    }

    #[test]
    fn a_wav_with_no_markers_has_no_cue_chunk() {
        let wav = wav_bytes(&[1, -1], &[], 8000).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert!(!wav.windows(4).any(|w| w == b"cue "));
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, wav.len() - 8);
    }
}
