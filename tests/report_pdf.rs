//! M1.9: a report renders to PDF offline, from a root confined to the staged
//! files, and the same inputs give the same bytes (`embarch-api` decision 109).
#![cfg(feature = "pdf")]

use std::fs;
use std::path::{Path, PathBuf};

use embarch_api::report_pdf::{
    plot_ticks, render_report, report_text, vendored_files, RenderOptions, ReportPdfError,
    Staging, TickError, TYPST_VERSION,
};

const OPTS: RenderOptions = RenderOptions {
    ident: String::new(),
    timestamp_unix: 1_763_700_000,
};

fn opts() -> RenderOptions {
    RenderOptions {
        ident: "run-0123456789abcdef-r1".into(),
        ..OPTS
    }
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/report_pdf")
}

fn staged_fixture() -> Staging {
    let mut s = Staging::new().unwrap();
    for f in ["report.typ", "capture.csv", "measurements.json"] {
        s.stage_file(&fixture().join(f), f).unwrap();
    }
    s
}

fn staged_report(typ: &str) -> Staging {
    let mut s = Staging::new().unwrap();
    s.stage_bytes("report.typ", typ.as_bytes()).unwrap();
    s
}

#[test]
fn a_fixture_run_renders_with_no_network() {
    // Nothing here can reach a registry: the build links no package client,
    // and a package that is not embedded is not found.
    let pdf = render_report(&staged_fixture(), &opts()).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 5_000, "{} bytes", pdf.len());

    let missing = render_report(
        &staged_report("#import \"@preview/cetz:0.3.0\": *\nhello"),
        &opts(),
    );
    match missing {
        Err(ReportPdfError::Compile(m)) => assert!(m.contains("cetz"), "{m}"),
        other => panic!("expected a compile error, got {other:?}"),
    }
}

#[test]
fn the_page_number_is_the_footer_and_the_plot_carries_the_helpers_ticks() {
    let pages = report_text(&staged_fixture(), &opts()).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].last().map(String::as_str), Some("1"), "{:?}", pages[0]);
    assert!(pages[0].iter().any(|t| t.contains("49.8")), "{:?}", pages[0]);
    // The y ticks are those of the per-bin min and max of the capture, binned
    // as the helper bins (50 bins of consecutive rows).
    let csv = fs::read_to_string(fixture().join("capture.csv")).unwrap();
    let ys: Vec<f64> = csv
        .lines()
        .skip(1)
        .map(|l| l.split(',').nth(1).unwrap().parse().unwrap())
        .collect();
    let (n, nb) = (ys.len(), 50);
    let bins: Vec<&[f64]> = (0..nb).map(|b| &ys[b * n / nb..(b + 1) * n / nb]).collect();
    let lo = bins.iter().flat_map(|b| b.iter().copied()).fold(f64::INFINITY, f64::min);
    let hi = bins.iter().flat_map(|b| b.iter().copied()).fold(f64::NEG_INFINITY, f64::max);
    let ticks = plot_ticks(lo, hi).unwrap();
    for t in &ticks.texts {
        assert!(pages[0].contains(t), "tick {t} not on the page: {:?}", pages[0]);
    }
}

#[test]
fn a_report_reading_outside_the_root_fails() {
    let s = staged_report("#read(\"../outside.txt\")");
    // The file exists, one level above the staging root.
    fs::write(s.root().parent().unwrap().join("outside.txt"), "secret").unwrap();
    let err = render_report(&s, &opts()).unwrap_err();
    assert!(matches!(err, ReportPdfError::Compile(_)), "{err}");
    fs::remove_file(s.root().parent().unwrap().join("outside.txt")).unwrap();

    // An absolute path is the project root's, not the machine's.
    let err = render_report(&staged_report("#read(\"/etc/hostname\")"), &opts()).unwrap_err();
    assert!(matches!(err, ReportPdfError::Compile(_)), "{err}");

    // A file that is next to the report on disk but was never staged.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("unstaged.txt"), "x").unwrap();
    let err = render_report(&staged_report("#read(\"unstaged.txt\")"), &opts()).unwrap_err();
    assert!(matches!(err, ReportPdfError::Compile(_)), "{err}");

    // The control: a staged file reads fine.
    let mut ok = staged_report("#read(\"data.txt\")");
    ok.stage_bytes("data.txt", b"staged").unwrap();
    render_report(&ok, &opts()).unwrap();
}

#[test]
fn staging_refuses_links_and_escaping_names() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.txt");
    fs::write(&real, "x").unwrap();

    let link = dir.path().join("sym.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut s = Staging::new().unwrap();
    let err = s.stage_file(&link, "sym.txt").unwrap_err();
    assert!(err.to_string().contains("symbolic link"), "{err}");

    // `real.txt` now has a second name.
    let hard = dir.path().join("hard.txt");
    fs::hard_link(&real, &hard).unwrap();
    let err = s.stage_file(&hard, "hard.txt").unwrap_err();
    assert!(err.to_string().contains("hard links"), "{err}");
    let err = s.stage_file(&real, "real.txt").unwrap_err();
    assert!(err.to_string().contains("hard links"), "{err}");

    for bad in ["../x", "/x", "a/../../x", "", "."] {
        assert!(s.stage_bytes(bad, b"x").is_err(), "{bad:?} was staged");
    }
    assert!(!s.root().join("sym.txt").exists());
    assert!(!s.root().join("hard.txt").exists());
}

#[test]
fn a_link_planted_in_the_staging_directory_is_not_read() {
    let s = staged_report("#read(\"planted.txt\")");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), s.root().join("planted.txt"))
        .unwrap();
    let err = render_report(&s, &opts()).unwrap_err();
    assert!(matches!(err, ReportPdfError::Compile(_)), "{err}");
}

#[test]
fn the_same_inputs_give_the_same_pdf() {
    let a = render_report(&staged_fixture(), &opts()).unwrap();
    let b = render_report(&staged_fixture(), &opts()).unwrap();
    assert_eq!(a, b, "two renders of the same staged files differ");

    let later = RenderOptions {
        timestamp_unix: OPTS.timestamp_unix + 1,
        ..opts()
    };
    assert_ne!(a, render_report(&staged_fixture(), &later).unwrap());
}

#[test]
fn the_helpers_ticks_are_the_documented_ones() {
    let cases: [(f64, f64, &[&str]); 6] = [
        (0.0, 100.0, &["0", "20", "40", "60", "80", "100"]),
        (0.0, 3.3, &["0", "1", "2", "3", "4"]),
        (4.75, 5.25, &["4.7", "4.8", "4.9", "5.0", "5.1", "5.2", "5.3"]),
        (-10.0, 10.0, &["-10", "-5", "0", "5", "10"]),
        (2.0, 2.0, &["2"]),
        (-0.4, 0.4, &["-0.4", "-0.2", "0.0", "0.2", "0.4"]),
    ];
    for (lo, hi, want) in cases {
        let t = plot_ticks(lo, hi).unwrap();
        assert_eq!(t.texts, want, "({lo}, {hi})");
    }
    assert_eq!(plot_ticks(1.0, 0.0), Err(TickError::Refused));
    assert_eq!(plot_ticks(f64::NAN, 1.0), Err(TickError::Refused));
    assert_eq!(plot_ticks(0.0, f64::INFINITY), Err(TickError::Refused));
}

#[test]
fn the_typst_helper_and_the_rust_ticks_agree() {
    let ranges = [
        (0.0, 100.0),
        (0.0, 3.3),
        (4.75, 5.25),
        (-10.0, 10.0),
        (2.0, 2.0),
        (-0.3, 0.3),
        (0.0, 0.0007),
        (-1234.5, 98765.4),
        (1.0, 1.0001),
        (0.0, 7.0),
        (0.0, 28.0),
        (3.0, 3.5),
    ];
    let mut typ = String::from("#import \"@embarch/report:0.1.0\": plot-ticks\n");
    for (lo, hi) in ranges {
        typ.push_str(&format!(
            "#for t in plot-ticks({lo:?}, {hi:?}).texts [#t|]\n#parbreak()\n"
        ));
    }
    let pages = report_text(&staged_report(&typ), &opts()).unwrap();
    let drawn: String = pages[0].concat();
    for (lo, hi) in ranges {
        let want: String = plot_ticks(lo, hi)
            .unwrap()
            .texts
            .iter()
            .map(|t| format!("{t}|"))
            .collect();
        assert!(drawn.contains(&want), "({lo}, {hi}) want {want} in {drawn}");
    }
}

#[test]
fn the_vendored_packages_are_the_pinned_ones() {
    let files = vendored_files();
    for p in [
        "preview/lilaq/0.6.0/src/lilaq.typ",
        "preview/elembic/1.1.1/src/lib.typ",
        "preview/tiptoe/0.4.0/src/tiptoe.typ",
        "preview/zero/0.6.1/src/zero.typ",
        "embarch/report/0.1.0/lib.typ",
    ] {
        assert!(files.iter().any(|f| f.path == p), "{p} is not embedded");
    }
    // Nothing from the registry beyond what lilaq imports.
    for f in &files {
        let ok = f.path.starts_with("embarch/")
            || ["lilaq/0.6.0", "elembic/1.1.1", "tiptoe/0.4.0", "zero/0.6.1"]
                .iter()
                .any(|p| f.path.starts_with(&format!("preview/{p}/")));
        assert!(ok, "{} is not a pinned package", f.path);
    }
}

#[test]
fn the_recorded_typst_version_is_the_linked_one() {
    let lock = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).unwrap();
    let want = format!("name = \"typst\"\nversion = \"{TYPST_VERSION}\"");
    assert!(lock.contains(&want), "Cargo.lock does not pin typst {TYPST_VERSION}");
}
