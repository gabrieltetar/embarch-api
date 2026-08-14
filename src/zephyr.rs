//! Live Zephyr/west target discovery for a `discovery = "zephyr-west"`
//! project (`design.md` §3 decision 12).
//!
//! Nothing in here is cached, at any granularity, within or across sessions
//! — it's pure filesystem + YAML reads, already cheap enough to redo on
//! every call, and caching it would reintroduce the exact staleness bug
//! decision 12 exists to eliminate.
//!
//! This module owns two things a static, hand-maintained `[[projects]]`
//! entry used to leave to a human: enumerating what's actually buildable
//! (`scan`), and refusing to propose a (variant, revision) combination that
//! Zephyr's own board-revision mechanism would silently build with the
//! wrong shape (`revision_is_backed`) — see the module-level rationale in
//! `embarch-api/design.md` §3 decision 12 for why that check exists at all.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ---- board.yml schema (Zephyr's hardware model v2) -----------------------
//
// Only the fields this module actually needs. A real board.yml carries more
// (vendor, full_name, documentation links, ...) — deliberately not modeled,
// so a schema addition upstream doesn't break parsing here.

#[derive(Debug, Deserialize)]
struct BoardYml {
    board: BoardSection,
    #[serde(default)]
    revision: Option<RevisionSection>,
}

#[derive(Debug, Deserialize)]
struct BoardSection {
    name: String,
    #[serde(default)]
    socs: Vec<SocSection>,
}

#[derive(Debug, Deserialize)]
struct SocSection {
    name: String,
    #[serde(default)]
    cpuclusters: Vec<CpuClusterSection>,
    #[serde(default)]
    variants: Vec<VariantSection>,
}

#[derive(Debug, Deserialize)]
struct CpuClusterSection {
    name: String,
    #[serde(default)]
    variants: Vec<VariantSection>,
}

#[derive(Debug, Deserialize)]
struct VariantSection {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RevisionSection {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    revisions: Vec<RevisionEntry>,
}

#[derive(Debug, Deserialize)]
struct RevisionEntry {
    name: String,
}

/// A parsed `board.yml`, plus the directory it lives in — needed later to
/// check whether a revision-suffixed overlay/defconfig file actually exists
/// there.
struct BoardDef {
    dir: PathBuf,
    yml: BoardYml,
}

/// One live-scanned, file-backing-validated buildable target. Every field
/// but `board`/`soc`/`app` is optional because not every board declares a
/// cpucluster, a variant, or a revision scheme at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub board: String,
    pub soc: String,
    pub cpucluster: Option<String>,
    pub variant: Option<String>,
    pub revision: Option<String>,
    pub app: String,
}

impl Target {
    /// The board qualifier string `west build -b` expects:
    /// `<board>[@<revision>]/<soc>[/<cpucluster>][/<variant>]`.
    pub fn board_qualifier(&self) -> String {
        let mut s = self.board.clone();
        if let Some(rev) = &self.revision {
            s.push('@');
            s.push_str(rev);
        }
        s.push('/');
        s.push_str(&self.soc);
        if let Some(c) = &self.cpucluster {
            s.push('/');
            s.push_str(c);
        }
        if let Some(v) = &self.variant {
            s.push('/');
            s.push_str(v);
        }
        s
    }

    /// `board_qualifier()` plus the app name, for a human-readable listing
    /// (error messages, `list_targets`' CLI text form) — not itself a valid
    /// `west build -b` argument.
    pub fn board_qualifier_with_app(&self) -> String {
        format!("{} app={}", self.board_qualifier(), self.app)
    }

    /// Per-target build directory name, satisfying
    /// `embarch-umbrella/design.md` §3 decision 10's no-shared-build-dir
    /// rule without a human naming each one:
    /// `<board>-<variant-or-'default'>-<revision-or-'none'>-<app>`.
    pub fn build_dir_name(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.board,
            self.variant.as_deref().unwrap_or("default"),
            self.revision.as_deref().unwrap_or("none"),
            self.app
        )
    }
}

/// A project directory doesn't look Zephyr/west-shaped: no `boards/*/*.yml`
/// found at all (unrelated to whether any of them turned out file-backing
/// valid). Same detection shape `embarch-umbrella`'s `init` uses.
#[derive(Debug)]
pub struct NotZephyrWest;

impl std::fmt::Display for NotZephyrWest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no board.yml files found under boards/ — this doesn't look like a Zephyr/west project"
        )
    }
}

impl std::error::Error for NotZephyrWest {}

/// Live-scan `source_path` for every file-backing-validated (board, soc,
/// cpucluster, variant, revision, app) tuple. Pure filesystem + YAML reads —
/// no `west` invocation, so cheap enough to call on every request.
pub fn scan(source_path: &Path) -> Result<Vec<Target>, NotZephyrWest> {
    let boards = scan_boards(source_path);
    if boards.is_empty() {
        return Err(NotZephyrWest);
    }
    let apps = scan_apps(source_path);

    let mut targets = Vec::new();
    for board in &boards {
        for soc in &board.yml.board.socs {
            if soc.cpuclusters.is_empty() {
                push_targets_for_variants(&mut targets, board, soc, None, &soc.variants, &apps);
            } else {
                for cluster in &soc.cpuclusters {
                    push_targets_for_variants(
                        &mut targets,
                        board,
                        soc,
                        Some(cluster.name.as_str()),
                        &cluster.variants,
                        &apps,
                    );
                }
            }
        }
    }
    Ok(targets)
}

#[allow(clippy::too_many_arguments)]
fn push_targets_for_variants(
    out: &mut Vec<Target>,
    board: &BoardDef,
    soc: &SocSection,
    cpucluster: Option<&str>,
    variants: &[VariantSection],
    apps: &[String],
) {
    let variant_names: Vec<Option<&str>> = if variants.is_empty() {
        vec![None]
    } else {
        variants.iter().map(|v| Some(v.name.as_str())).collect()
    };

    let revisions = candidate_revisions(&board.yml.revision);

    for variant in &variant_names {
        for revision in &revisions {
            let backed = revision_is_backed(
                &board.dir,
                &board.yml.board.name,
                &soc.name,
                cpucluster,
                *variant,
                revision.as_deref(),
                board.yml.revision.as_ref().and_then(|r| r.default.as_deref()),
            );
            if !backed {
                continue;
            }
            for app in apps {
                out.push(Target {
                    board: board.yml.board.name.clone(),
                    soc: soc.name.clone(),
                    cpucluster: cpucluster.map(str::to_string),
                    variant: variant.map(str::to_string),
                    revision: revision.clone(),
                    app: app.clone(),
                });
            }
        }
    }
}

/// Every revision worth considering: the declared default (even if
/// `revisions` doesn't separately list it) plus every declared revision.
/// A board with no `revision:` section at all has exactly one "revision" —
/// `None`, meaning the axis doesn't apply.
fn candidate_revisions(revision: &Option<RevisionSection>) -> Vec<Option<String>> {
    match revision {
        None => vec![None],
        Some(r) => {
            let mut names: Vec<String> = r.revisions.iter().map(|e| e.name.clone()).collect();
            if let Some(default) = &r.default {
                if !names.contains(default) {
                    names.push(default.clone());
                }
            }
            if names.is_empty() {
                vec![None]
            } else {
                names.into_iter().map(Some).collect()
            }
        }
    }
}

/// The mechanical safety check this decision exists for: Zephyr's
/// board-revision mechanism auto-applies a revision-suffixed
/// overlay/defconfig file *if one exists*, and silently falls back to the
/// board's un-revisioned base files if it doesn't. So a (variant, revision)
/// combination is only real if the default revision applies (implicitly
/// backed by the base files) or a matching revision-suffixed file exists —
/// never just because `board.yml` lists the revision's name.
fn revision_is_backed(
    board_dir: &Path,
    board: &str,
    soc: &str,
    cpucluster: Option<&str>,
    variant: Option<&str>,
    revision: Option<&str>,
    default_revision: Option<&str>,
) -> bool {
    let Some(revision) = revision else {
        // No revision axis for this board at all — always backed by the
        // plain base files.
        return true;
    };
    if Some(revision) == default_revision {
        return true;
    }

    let mut stem = board.to_string();
    stem.push('_');
    stem.push_str(soc);
    if let Some(c) = cpucluster {
        stem.push('_');
        stem.push_str(c);
    }
    if let Some(v) = variant {
        stem.push('_');
        stem.push_str(v);
    }
    stem.push('_');
    stem.push_str(&revision.replace('.', "_"));

    [".overlay", ".defconfig"]
        .iter()
        .any(|ext| board_dir.join(format!("{stem}{ext}")).exists())
}

/// Every `board.yml`/`.yaml` under `source_path/boards`, recursively — not
/// just one level deep, since real repos nest boards under a vendor
/// directory (`boards/<vendor>/<board>/<board>.yml`). Files that don't
/// parse as a board definition (wrong shape, or genuinely not one) are
/// skipped rather than treated as an error — a `boards/` directory
/// legitimately holds other YAML too.
fn scan_boards(source_path: &Path) -> Vec<BoardDef> {
    let boards_dir = source_path.join("boards");
    let mut out = Vec::new();
    let mut stack = vec![boards_dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "yml" || e == "yaml");
            if !is_yaml {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(yml) = serde_yaml::from_str::<BoardYml>(&raw) {
                out.push(BoardDef {
                    dir: path.parent().map(Path::to_path_buf).unwrap_or(dir.clone()),
                    yml,
                });
            }
        }
    }
    out
}

/// Every `app/<name>/CMakeLists.txt` under `source_path` — app name is the
/// directory name. Same shape `embarch-umbrella`'s `init` already
/// recognizes as "this repo has a west app."
fn scan_apps(source_path: &Path) -> Vec<String> {
    let app_dir = source_path.join("app");
    let Ok(entries) = std::fs::read_dir(&app_dir) else {
        return Vec::new();
    };
    let mut apps: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir() && e.path().join("CMakeLists.txt").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    apps.sort();
    apps
}

/// Narrows `targets` by whatever subset of `board`/`variant`/`revision`/`app`
/// is given. Never guesses: exactly one match proceeds, zero or more than
/// one is an error (`design.md` §3 decision 12) — the caller decides how to
/// report each case (`Selection::NoMatch` lists nothing to fall back to
/// beyond the full scan; `Selection::Ambiguous` carries exactly the
/// narrowed remainder, not the full unfiltered list).
#[derive(Debug)]
pub enum Selection {
    Ambiguous(Vec<Target>),
    NoMatch,
}

pub fn select(
    targets: &[Target],
    board: Option<&str>,
    variant: Option<&str>,
    revision: Option<&str>,
    app: Option<&str>,
) -> Result<Target, Selection> {
    let filtered: Vec<Target> = targets
        .iter()
        .filter(|t| board.is_none_or(|b| t.board == b))
        .filter(|t| variant.is_none_or(|v| t.variant.as_deref() == Some(v)))
        .filter(|t| revision.is_none_or(|r| t.revision.as_deref() == Some(r)))
        .filter(|t| app.is_none_or(|a| t.app == a))
        .cloned()
        .collect();

    match filtered.len() {
        1 => Ok(filtered.into_iter().next().expect("len == 1")),
        0 => Err(Selection::NoMatch),
        _ => Err(Selection::Ambiguous(filtered)),
    }
}

/// `<build dir>/zephyr/zephyr.<ext>`, per `flash_format` — the standard
/// `west build` output layout, same convention a `discovery = "static"`
/// project's `artifact_path` already assumes by hand.
pub fn artifact_path(build_dir: &Path, flash_format: &str) -> PathBuf {
    build_dir.join("zephyr").join(format!("zephyr.{flash_format}"))
}

/// The `west build` argv for a resolved target: `west build -b
/// <qualifier> -d <build_dir> <app_path>`.
pub fn build_command(
    west_binary: &Path,
    target: &Target,
    build_dir: &Path,
    app_path: &Path,
) -> Vec<String> {
    vec![
        west_binary.display().to_string(),
        "build".to_string(),
        "-b".to_string(),
        target.board_qualifier(),
        "-d".to_string(),
        build_dir.display().to_string(),
        app_path.display().to_string(),
    ]
}

fn context_result<T>(r: Result<T, NotZephyrWest>, source_path: &Path) -> Result<T> {
    r.with_context(|| format!("{} does not look like a Zephyr/west project", source_path.display()))
}

/// Convenience wrapper returning `anyhow::Result` for callers that just want
/// a single error type (MCP tool / CLI handlers), rather than matching on
/// `NotZephyrWest` themselves.
pub fn scan_or_err(source_path: &Path) -> Result<Vec<Target>> {
    context_result(scan(source_path), source_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Builds a synthetic repo tree modeling the real healthband repo's
    /// structure described in `embarch-umbrella/milestone-6.md`'s 2026-08-13
    /// entry: one board (`roadrunner`) with a single SoC/cpucluster and four
    /// LED variants, real hardware-revision overlays only at `evt1` — not at
    /// every revision `board.yml` declares (`1`, `2`, `evt1`). This is the
    /// exact shape the file-backing check exists to get right: naively
    /// trusting `board.yml`'s declared revision list would let `os_5led`
    /// resolve at revision `1`, which Zephyr would silently build with no
    /// revision overlay applied at all.
    fn write_synthetic_repo(root: &Path) {
        let board_dir = root.join("boards/acme/roadrunner");
        fs::create_dir_all(&board_dir).unwrap();
        fs::write(
            board_dir.join("roadrunner.yml"),
            r#"
board:
  name: roadrunner
  socs:
    - name: nrf54l15
      cpuclusters:
        - name: cpuapp
          variants:
            - name: os_5led
            - name: os_3led
            - name: max_5led
            - name: max_3led
revision:
  format: custom
  default: "2"
  revisions:
    - name: "1"
    - name: "2"
    - name: "evt1"
"#,
        )
        .unwrap();
        // Only os_5led/evt1 actually has a revision-suffixed overlay —
        // matches the real finding: variants only have real overlays at evt1.
        fs::write(
            board_dir.join("roadrunner_nrf54l15_cpuapp_os_5led_evt1.overlay"),
            "",
        )
        .unwrap();

        let app_dir = root.join("app/healthband");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();
    }

    #[test]
    fn scans_default_revision_for_every_variant() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        // Every one of the 4 variants is valid at the default revision "2".
        let at_default: Vec<_> = targets
            .iter()
            .filter(|t| t.revision.as_deref() == Some("2"))
            .collect();
        assert_eq!(at_default.len(), 4, "{targets:#?}");
    }

    #[test]
    fn only_backed_variant_revision_combo_resolves_at_non_default_revision() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        let at_evt1: Vec<_> = targets
            .iter()
            .filter(|t| t.revision.as_deref() == Some("evt1"))
            .collect();
        assert_eq!(at_evt1.len(), 1, "{targets:#?}");
        assert_eq!(at_evt1[0].variant.as_deref(), Some("os_5led"));
    }

    #[test]
    fn unbacked_variant_revision_combo_never_appears() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        // os_5led at revision "1" has no overlay — must not appear, unlike
        // naively trusting board.yml's declared revision list.
        let bogus = targets.iter().any(|t| {
            t.variant.as_deref() == Some("os_5led") && t.revision.as_deref() == Some("1")
        });
        assert!(!bogus, "{targets:#?}");
    }

    #[test]
    fn no_board_yml_is_not_zephyr_west() {
        let dir = tempfile_dir();
        fs::create_dir_all(dir.path().join("app/foo")).unwrap();
        assert!(scan(dir.path()).is_err());
    }

    #[test]
    fn select_narrows_to_singleton() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        let picked = select(&targets, None, Some("os_5led"), Some("evt1"), None).unwrap();
        assert_eq!(picked.variant.as_deref(), Some("os_5led"));
        assert_eq!(picked.revision.as_deref(), Some("evt1"));
    }

    #[test]
    fn select_with_no_filters_is_ambiguous_when_more_than_one_target_exists() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        match select(&targets, None, None, None, None) {
            Err(Selection::Ambiguous(remaining)) => assert_eq!(remaining.len(), targets.len()),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn select_narrows_a_singleton_variant_without_it_being_typed() {
        // A board with exactly one real variant (no cpucluster-level
        // variants declared at all) never requires `variant` to disambiguate
        // — mirrors nff_dev's single "default" variant in the real repo.
        let dir = tempfile_dir();
        let board_dir = dir.path().join("boards/acme/single");
        fs::create_dir_all(&board_dir).unwrap();
        fs::write(
            board_dir.join("single.yml"),
            r#"
board:
  name: single
  socs:
    - name: nrf54l15
      cpuclusters:
        - name: cpuapp
"#,
        )
        .unwrap();
        let app_dir = dir.path().join("app/dev");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        let targets = scan(dir.path()).unwrap();
        assert_eq!(targets.len(), 1);
        let picked = select(&targets, None, None, None, None).unwrap();
        assert_eq!(picked.board, "single");
        assert_eq!(picked.variant, None);
        assert_eq!(picked.revision, None);
    }

    #[test]
    fn board_qualifier_formats_correctly() {
        let t = Target {
            board: "roadrunner".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: Some("os_5led".into()),
            revision: Some("evt1".into()),
            app: "healthband".into(),
        };
        assert_eq!(t.board_qualifier(), "roadrunner@evt1/nrf54l15/cpuapp/os_5led");
    }

    #[test]
    fn board_qualifier_omits_absent_axes() {
        let t = Target {
            board: "nff_dev".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: None,
            revision: None,
            app: "healthband".into(),
        };
        assert_eq!(t.board_qualifier(), "nff_dev/nrf54l15/cpuapp");
    }

    #[test]
    fn build_dir_name_is_stable_and_unique_per_target() {
        let a = Target {
            board: "roadrunner".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: Some("os_5led".into()),
            revision: Some("evt1".into()),
            app: "healthband".into(),
        };
        assert_eq!(a.build_dir_name(), "roadrunner-os_5led-evt1-healthband");
    }

    // Minimal tempdir helper — avoids pulling in the `tempfile` crate for a
    // handful of tests. Cleans up via Drop, same guarantee `tempfile` gives.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tempfile_dir() -> TempDir {
        let mut base = std::env::temp_dir();
        let unique = format!(
            "embarch-api-zephyr-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        base.push(unique);
        fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }
}
