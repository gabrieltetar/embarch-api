//! `export-audio` (decision 130): a synthetic recording with one gap and three
//! markers exports two WAV files whose samples, cue points and label files
//! match fixtures written out here by hand, and a tap with no declared rate is
//! refused. Drives `audio_export::export`, which is what the MCP tool and the
//! subcommand both reach through `audio_export::run`.

mod support;

use embarch_api::audio_export::{self, ExportError};
use embarch_study_designer::analog::{
    encode_block, encode_start_header, AnalogReference, EventWord, StartHeader, BLOCK_PREFIX_BYTES, CRC_BYTES,
    START_HEADER_BYTES,
};
use embarch_study_designer::Study;

const RATE_HZ: u32 = 8000;
const FRAMES: u16 = 4;

fn study(source: serde_json::Value) -> Study {
    serde_json::from_value(serde_json::json!({
        "name": "audio-export",
        "requires": { "dev_bench_version": "any", "firmware_version": "any" },
        "steps": [],
        "steps_crc": 0,
        "streams": [{ "id": 0, "name": "mic", "source": source, "encoding": "Raw", "scope": "WholeStudy" }],
        "record_checks": [{ "stream_id": 0, "framing": { "AnalogBlocks": { "channels": 1, "samples_per_block": FRAMES } } }],
        "session": {
            "start_text": "go", "run_fields": [], "done_text": "done",
            "max_duration_ms": 1000, "export_dir": "exports/mic"
        }
    }))
    .unwrap()
}

fn analog_study() -> Study {
    study(serde_json::json!({ "AnalogInput": { "sample_hz": RATE_HZ, "channel": 0 } }))
}

fn header_bytes() -> Vec<u8> {
    let header = StartHeader {
        sample_rate_hz: RATE_HZ,
        resolution_bits: 12,
        gain_numerator: 1,
        gain_denominator: 1,
        reference: AnalogReference::Internal,
        channel_count: 1,
        vdd_mv: 3300,
        input_name: heapless::String::try_from("A0").unwrap(),
        board: heapless::String::try_from("dev kit").unwrap(),
        image_version: heapless::String::try_from("v0").unwrap(),
    };
    let mut out = vec![0u8; START_HEADER_BYTES];
    encode_start_header(&mut out, &header).unwrap();
    out
}

fn block(index: u32, events: EventWord, codes: [i16; FRAMES as usize]) -> Vec<u8> {
    let mut out = vec![0u8; BLOCK_PREFIX_BYTES + 2 * codes.len() + CRC_BYTES];
    encode_block(&mut out, index, events, &codes, 1, FRAMES).unwrap();
    out
}

/// Header, then blocks at 0, 4, 8, a gap of 8 samples, then 20 and 24. Markers:
/// high at sample 6 and low at sample 8 (first segment), high at 21 (second).
fn recording() -> Vec<u8> {
    let none = EventWord::new(0, 0, false);
    let mut raw = header_bytes();
    raw.extend(block(0, none, [2048, 2049, 2050, 2051]));
    raw.extend(block(4, EventWord::new(1, 2, true), [2052, 2053, 2054, 2055]));
    raw.extend(block(8, EventWord::new(1, 0, false), [0, 4095, 2048, 2047]));
    raw.extend(block(20, EventWord::new(1, 1, true), [3000, 3001, 3002, 3003]));
    raw.extend(block(24, none, [1000, 1001, 1002, 1003]));
    raw
}

/// A hand-rolled RIFF reader: chunk id and body, in order.
fn chunks(wav: &[u8]) -> Vec<(String, Vec<u8>)> {
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, wav.len() - 8);
    assert_eq!(&wav[8..12], b"WAVE");
    let mut pos = 12;
    let mut out = Vec::new();
    while pos < wav.len() {
        let id = String::from_utf8(wav[pos..pos + 4].to_vec()).unwrap();
        let len = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().unwrap()) as usize;
        out.push((id, wav[pos + 8..pos + 8 + len].to_vec()));
        pos += 8 + len + (len % 2);
    }
    assert_eq!(pos, wav.len());
    out
}

fn pcm(body: &[u8]) -> Vec<i16> {
    body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn cue_points(body: &[u8]) -> Vec<(u32, u32)> {
    let n = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    (0..n)
        .map(|i| {
            let p = &body[4 + 24 * i..4 + 24 * (i + 1)];
            assert_eq!(&p[8..12], b"data");
            let id = u32::from_le_bytes(p[0..4].try_into().unwrap());
            let sample_offset = u32::from_le_bytes(p[20..24].try_into().unwrap());
            assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), sample_offset);
            (id, sample_offset)
        })
        .collect()
}

#[test]
fn one_gap_and_three_markers_export_two_wavs_that_match_the_fixtures() {
    let dir = support::unique_dir("audio-export");
    let out = audio_export::export(&analog_study(), "mic", "run-1", &recording(), &dir).unwrap();

    assert_eq!(out.files.len(), 2);
    assert_eq!(out.sample_hz, RATE_HZ);
    assert_eq!(out.resolution_bits, 12);
    assert_eq!(out.gaps.len(), 1);
    assert_eq!((out.gaps[0].after_segment, out.gaps[0].expected_index, out.gaps[0].got_index), (1, 12, 20));
    assert_eq!(out.gaps[0].missing_samples, 8);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.gaps_file.as_ref().unwrap().is_file());

    // Segment 1: 12 samples, (code - 2048) * 16.
    let wav1 = std::fs::read(&out.files[0].wav).unwrap();
    let c1 = chunks(&wav1);
    let ids: Vec<_> = c1.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["fmt ", "data", "cue ", "LIST"]);
    let fmt = &c1[0].1;
    assert_eq!(u16::from_le_bytes([fmt[0], fmt[1]]), 1, "PCM");
    assert_eq!(u16::from_le_bytes([fmt[2], fmt[3]]), 1, "mono");
    assert_eq!(u32::from_le_bytes(fmt[4..8].try_into().unwrap()), RATE_HZ);
    assert_eq!(u16::from_le_bytes([fmt[14], fmt[15]]), 16, "bits");
    assert_eq!(
        pcm(&c1[1].1),
        [0, 16, 32, 48, 64, 80, 96, 112, -32768, 32752, 0, -16]
    );
    assert_eq!(cue_points(&c1[2].1), [(1, 6), (2, 8)]);
    let adtl = &c1[3].1;
    assert_eq!(&adtl[..4], b"adtl");
    assert!(adtl.windows(16).any(|w| w == b"marker 1 (high)\0"));
    assert!(adtl.windows(15).any(|w| w == b"marker 2 (low)\0"));
    assert_eq!(
        std::fs::read_to_string(&out.files[0].labels).unwrap(),
        "0.000750\t0.000750\tmarker 1 (high)\n0.001000\t0.001000\tmarker 2 (low)\n"
    );

    // Segment 2: 8 samples starting at sample index 20, one marker one sample in.
    let wav2 = std::fs::read(&out.files[1].wav).unwrap();
    let c2 = chunks(&wav2);
    assert_eq!(out.files[1].first_index, 20);
    assert_eq!(pcm(&c2[1].1), [952 * 16, 953 * 16, 954 * 16, 955 * 16, -1048 * 16, -1047 * 16, -1046 * 16, -1045 * 16]);
    assert_eq!(cue_points(&c2[2].1), [(1, 1)]);
    assert_eq!(
        std::fs::read_to_string(&out.files[1].labels).unwrap(),
        "0.000125\t0.000125\tmarker 3 (high)\n"
    );
    assert!(out.files[0].wav.starts_with(&dir) && out.files[1].labels.starts_with(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_tap_with_no_declared_rate_is_refused_with_a_reason() {
    // A power tap declares a sample_hz but is not an analog input, and a text
    // tap declares none: neither is audio.
    for source in [
        serde_json::json!({ "PowerFrontEnd": { "sample_hz": 1000 } }),
        serde_json::json!("GattTranscript"),
    ] {
        let err = audio_export::export(&study(source), "mic", "run-1", &recording(), &support::unique_dir("none"))
            .unwrap_err();
        let refused = err.downcast_ref::<ExportError>().expect("an ExportError");
        assert!(matches!(refused, ExportError::NoDeclaredRate { .. }), "{refused}");
        assert!(refused.to_string().contains("no audio rate"), "{refused}");
    }
}

#[test]
fn a_capture_without_a_start_header_is_refused() {
    let err = audio_export::export(&analog_study(), "mic", "run-1", &recording()[START_HEADER_BYTES..], &support::unique_dir("nohdr"))
        .unwrap_err();
    assert!(matches!(err.downcast_ref::<ExportError>(), Some(ExportError::NoStartHeader)));
}

#[test]
fn the_export_folder_is_the_sessions_unless_one_is_given() {
    let study = analog_study();
    let file = std::path::Path::new("/studies/mic.json");
    assert_eq!(
        audio_export::export_dir(&study, Some(file), None).unwrap(),
        std::path::Path::new("/studies/exports/mic")
    );
    assert_eq!(
        audio_export::export_dir(&study, Some(file), Some(std::path::Path::new("/tmp/x"))).unwrap(),
        std::path::Path::new("/tmp/x")
    );
}
