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
//! (`scan`), and refusing to propose a (soc, cpucluster, variant, revision)
//! combination that isn't real — two distinct failure modes, both found by
//! checking against a real target repo rather than a synthetic fixture
//! alone:
//! - **No devicetree source at all** (`dts_exists`): `board.yml` can declare
//!   a SoC/variant that was never given real `.dts` files (confirmed real
//!   example: `ref_nrf54dk` declares `nrf54l05`/`nrf54l10`/an `nrf54l15`
//!   `xip`/`ns` split that board.yml lists but only one of those five
//!   combinations — bare `nrf54l15`/`cpuapp` — has a `.dts` file backing
//!   it). Proposing the other four fails loud, at CMake configure time —
//!   not silent, but still wrong information for `list_targets` to hand out.
//! - **Wrong revision-overlay shape applied** (`revision_is_backed`):
//!   Zephyr's board-revision mechanism auto-applies a revision-suffixed
//!   overlay/defconfig file *if one exists*, and silently falls back to the
//!   board's un-revisioned base files if it doesn't — so a (variant,
//!   revision) combination is only real if a matching file exists, never
//!   just because `board.yml` lists the revision's name. This one *is*
//!   silent, which is why decision 12 exists at all.

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
}

#[derive(Debug, Deserialize)]
struct BoardSection {
    name: String,
    #[serde(default)]
    socs: Vec<SocSection>,
    /// Nested under `board:`, not a sibling top-level key — confirmed
    /// against the real reference-dut repo's `board.yml` files (`roadrunner`,
    /// `dut_dev`, `ref_nrf54dk`, `dut_demo`), all four of which nest it here.
    /// An earlier version of this module assumed a top-level `revision:`
    /// key instead, which silently discarded every real board's revision
    /// data instead of erroring — the exact kind of "succeeds while quietly
    /// wrong" failure this decision otherwise exists to prevent.
    #[serde(default)]
    revision: Option<RevisionSection>,
}

#[derive(Debug, Deserialize)]
struct SocSection {
    name: String,
    #[serde(default)]
    variants: Vec<VariantSection>,
}

/// A board variant. `cpucluster` lives *on the variant*, not on a separate
/// nesting level above it — confirmed against the real repo: `ref_nrf54dk`'s
/// `nrf54l15` SoC declares two variants (`xip`/`cpuflpr`, `ns`/`cpuapp`) each
/// with its own `cpucluster`, and a SoC can have zero variants at all
/// (`ref_nrf54dk`'s `nrf54l05`) — handled the same way an empty `variants`
/// list always was, as "one implicit unnamed variant, no cpucluster."
#[derive(Debug, Deserialize)]
struct VariantSection {
    name: String,
    #[serde(default)]
    cpucluster: Option<String>,
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

/// A `snippets/<dir>/snippet.yml` file. Only `name` matters here — real
/// snippets add `EXTRA_CONF_FILE`/`EXTRA_DTC_OVERLAY_FILE` and more, which
/// this module doesn't need: it only has to know a snippet exists and what
/// it's called, to validate a call-time `-S` selection and pass it through
/// to `west build` unchanged.
#[derive(Debug, Deserialize)]
struct SnippetYml {
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
    /// `<board>-<variant-or-'default'>-<revision-or-'none'>-<app>[-<snippets>]`.
    /// `snippets` (already sorted+deduped by the caller — `resolve.rs`) is
    /// folded in here too: two builds of the same (board, variant, revision,
    /// app) with a different `-S` selection are different CMake
    /// configurations and must not share a build directory, same reasoning
    /// as every other axis in this name.
    pub fn build_dir_name(&self, snippets: &[String]) -> String {
        let mut name = format!(
            "{}-{}-{}-{}",
            self.board,
            self.variant.as_deref().unwrap_or("default"),
            self.revision.as_deref().unwrap_or("none"),
            self.app
        );
        if !snippets.is_empty() {
            name.push('-');
            name.push_str(&snippets.join("_"));
        }
        name
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
            push_targets_for_soc(&mut targets, board, soc, &apps);
        }
    }
    Ok(targets)
}

/// A SoC with no declared `variants` (e.g. `ref_nrf54dk`'s `nrf54l05`) gets
/// exactly one implicit variant — no name, no cpucluster — same posture an
/// empty list always had, just no longer requiring a separate nesting level
/// to express "this SoC has no cpucluster/variant split."
///
/// A variant literally named `default` (e.g. `dut_dev`, `dut_demo`) is
/// treated the same way — confirmed against the real repo, not assumed: its
/// name never appears in any board filename (`dut_dev_nrf54l15_cpuapp.dts`,
/// `_6.overlay`, `_7.overlay` — never `_default_...`), and the real board
/// qualifier `west` actually built with (`build/build_info.yml`'s
/// `qualifiers: 'nrf54l15/cpuapp'`) has no variant component at all. It's a
/// schema placeholder for "this SoC's cpucluster, no real variant split,"
/// not a selectable value — carrying it through as `Some("default")` would
/// make both file-backing lookups and the assembled `west build -b` qualifier
/// wrong (e.g. looking for a `_default_7.overlay` that doesn't exist, or
/// building `dut_dev/nrf54l15/cpuapp/default` instead of the real
/// `dut_dev/nrf54l15/cpuapp`).
///
/// A SoC that *does* declare named product variants can still have a real,
/// separately-buildable variant-less build for the same cpucluster —
/// confirmed against `roadrunner`: `board.yml` declares three named variants
/// (`os`, `max_3led`, `max_5led`) under `nrf54l15`/`cpuapp`, but
/// `roadrunner_nrf54l15_cpuapp.dts` (no variant suffix) also exists, and is
/// exactly what the real day-to-day static config builds
/// (`roadrunner@2/nrf54l15/cpuapp`, no variant at all) — Zephyr's
/// board-qualifier variant component is always optional, declaring named
/// variants doesn't make selecting one mandatory. Without offering this
/// candidate too, `list_targets` would never surface the one target this
/// project's real config actually uses. One candidate is added per distinct
/// cpucluster already present among the named variants (skipping any
/// cpucluster a `default`-named variant already maps to `None` for, to avoid
/// a duplicate) — then every candidate, named or synthesized, still has to
/// pass `dts_exists` below to survive at all.
fn push_targets_for_soc(out: &mut Vec<Target>, board: &BoardDef, soc: &SocSection, apps: &[String]) {
    struct Selected<'a> {
        name: Option<&'a str>,
        cpucluster: Option<&'a str>,
    }
    let mut candidates: Vec<Selected> = if soc.variants.is_empty() {
        vec![Selected { name: None, cpucluster: None }]
    } else {
        let named: Vec<Selected> = soc
            .variants
            .iter()
            .map(|v| Selected {
                name: (v.name != "default").then_some(v.name.as_str()),
                cpucluster: v.cpucluster.as_deref(),
            })
            .collect();

        let already_bare: std::collections::HashSet<Option<&str>> =
            named.iter().filter(|s| s.name.is_none()).map(|s| s.cpucluster).collect();
        let mut clusters: Vec<&str> = named.iter().filter_map(|s| s.cpucluster).collect();
        clusters.sort_unstable();
        clusters.dedup();

        let mut all = named;
        for cluster in clusters {
            if !already_bare.contains(&Some(cluster)) {
                all.push(Selected { name: None, cpucluster: Some(cluster) });
            }
        }
        all
    };

    // Every candidate — named, implicit, or synthesized bare — needs real
    // devicetree source behind it, independent of the revision axis. This
    // is the check that excludes `ref_nrf54dk`'s `nrf54l05`/`nrf54l10`/
    // `nrf54l15`-`xip`/`nrf54l15`-`ns`: board.yml declares all four, but none
    // has a `.dts` file, only bare `nrf54l15`/`cpuapp` does.
    candidates.retain(|c| dts_exists(&board.dir, &board.yml.board.name, &soc.name, c.cpucluster, c.name));

    let revisions = candidate_revisions(&board.yml.board.revision);
    let default_revision = board.yml.board.revision.as_ref().and_then(|r| r.default.as_deref());

    for variant in &candidates {
        for revision in &revisions {
            let backed = revision_is_backed(
                &board.dir,
                &board.yml.board.name,
                &soc.name,
                variant.cpucluster,
                variant.name,
                revision.as_deref(),
                default_revision,
            );
            if !backed {
                continue;
            }
            for app in apps {
                out.push(Target {
                    board: board.yml.board.name.clone(),
                    soc: soc.name.clone(),
                    cpucluster: variant.cpucluster.map(str::to_string),
                    variant: variant.name.map(str::to_string),
                    revision: revision.clone(),
                    app: app.clone(),
                });
            }
        }
    }
}

/// Whether `<board>_<soc>[_<cpucluster>][_<variant>].dts` exists in
/// `board_dir` — the real gate on whether a (soc, cpucluster, variant)
/// combination has devicetree source to build at all, independent of the
/// revision axis `revision_is_backed` covers. See the module doc for the
/// real `ref_nrf54dk` finding this exists to catch.
fn dts_exists(board_dir: &Path, board: &str, soc: &str, cpucluster: Option<&str>, variant: Option<&str>) -> bool {
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
    board_dir.join(format!("{stem}.dts")).exists()
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
///
/// **The "default revision is automatically backed" shortcut only applies
/// when `variant` is `None`.** Confirmed necessary against `roadrunner`'s
/// real `revision.cmake`: it declares default revision `"1"`, but its own
/// custom logic hard-errors if a named product variant (`os`, `max_3led`,
/// `max_5led`) is built at any revision *other than* `evt1` — variants there
/// aren't just cosmetically tied to hardware revision, they're revision-
/// gated by name, deliberately, in code this module can't (and per decision
/// 12's own "pure filesystem + YAML reads" scope, shouldn't try to)
/// interpret. A board using `format: custom` opts out of Zephyr's default
/// per-revision-file convention entirely, so treating "this is the declared
/// default revision" as sufficient for *every* variant is an assumption
/// that only holds for the variant-less axis: the default revision's whole
/// point (per that repo's own comment) is being the one "no-override"
/// revision — safe because it carries no per-variant coupling — while
/// naming a product variant re-introduces exactly the coupling that default
/// was safe from. Requiring an explicit revision-suffixed file for any
/// named variant, even at the default revision, is the conservative
/// posture consistent with decision 12's original purpose: erring toward
/// fewer, correct proposals rather than more, unverifiable ones.
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
    if variant.is_none() && Some(revision) == default_revision {
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

/// Every snippet name declared under `source_path/app/<app>/snippets`,
/// recursively — matching Zephyr's own `snippets.py` (`os.walk` under a
/// snippet root, not a fixed one-level nesting), since a `snippet.yml`'s
/// directory nesting depth isn't itself meaningful. Confirmed against a
/// real target repo: ten snippets (`ble-shell`, `release`,
/// `factory-test`, `datalogging_cli`, `charging-state`, `wdt31`,
/// `sensor01-evk`, `sensor01-evt-3led`, `sensor01-evt-5led`, `max_signal`)
/// all one level deep, but not assumed to always be. The name comes from `snippet.yml`'s
/// own `name:` field, not the directory name — they match by convention in
/// every real example here, but Zephyr doesn't require it.
///
/// Only `app/<app>/snippets` is scanned, not `boards/**/snippets` or a
/// workspace-wide `snippets/` root — both real Zephyr snippet locations this
/// module doesn't yet cover, since the real repo only uses the former.
fn scan_snippets(source_path: &Path, app: &str) -> Vec<String> {
    let snippets_dir = source_path.join("app").join(app).join("snippets");
    let mut out = Vec::new();
    let mut stack = vec![snippets_dir];
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
            if path.file_name().and_then(|n| n.to_str()) != Some("snippet.yml") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(yml) = serde_yaml::from_str::<SnippetYml>(&raw) {
                out.push(yml.name);
            }
        }
    }
    out.sort();
    out.dedup();
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

/// Every real snippet name declared for `target`'s app — see `scan_snippets`
/// for what's scanned and why. Exposed separately from `Target` itself
/// (rather than as one of its fields) because a snippet selection isn't part
/// of the (board, soc, cpucluster, variant, revision, app) tuple `select`
/// narrows: it's an independent, purely additive build flag, validated
/// against this list at `build_command` call time instead.
pub fn available_snippets(source_path: &Path, app: &str) -> Vec<String> {
    scan_snippets(source_path, app)
}

/// The `west build` argv for a resolved target: `west build -b
/// <qualifier> -S <snippet> [-S <snippet> ...] -d <build_dir> <app_path>`.
/// One repeated `-S` per snippet — confirmed against the real `west build`
/// argument parser (`-S`/`--snippet`, `action='append'`), not a single
/// comma-joined value.
pub fn build_command(
    west_binary: &Path,
    target: &Target,
    snippets: &[String],
    build_dir: &Path,
    app_path: &Path,
) -> Vec<String> {
    let mut cmd = vec![
        west_binary.display().to_string(),
        "build".to_string(),
        "-b".to_string(),
        target.board_qualifier(),
    ];
    for snippet in snippets {
        cmd.push("-S".to_string());
        cmd.push(snippet.clone());
    }
    cmd.push("-d".to_string());
    cmd.push(build_dir.display().to_string());
    cmd.push(app_path.display().to_string());
    cmd
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

    /// Builds a synthetic repo tree modeling `roadrunner`'s real current
    /// shape (checked directly against `boards/nordic/roadrunner/board.yml`
    /// and its sibling files, not reconstructed from memory — an earlier
    /// version of this fixture modeled a since-stale shape with four
    /// variants and default revision `"2"`; both this fixture and the
    /// behavior it tests were corrected together after re-checking the real
    /// files): one SoC/cpucluster, three named product variants (`os`,
    /// `max_3led`, `max_5led`), default revision `"1"`, and a real,
    /// separately-buildable variant-less ("bare") build for the same
    /// cpucluster — `roadrunner_nrf54l15_cpuapp.dts` exists alongside each
    /// variant's own `.dts`, and the real day-to-day static config builds
    /// exactly this bare target (`roadrunner@2/nrf54l15/cpuapp`, no
    /// variant).
    ///
    /// The revision-backing shape mirrors a real, verified finding
    /// (`roadrunner`'s own `revision.cmake`): the bare target is backed at
    /// its default revision `"1"` (no override needed) and explicitly at
    /// `"2"` (`roadrunner_nrf54l15_cpuapp_2.overlay`), but *not* at `"evt1"`
    /// — that revision has no board behind it without a product variant
    /// selected. Named variants are the opposite: `revision.cmake` hard-
    /// errors unless a named variant is built at exactly `"evt1"`, so **the
    /// default-revision shortcut must not apply to a named variant at
    /// all** — modeled here by giving only `os` a real `_evt1.overlay` and
    /// no `_1`/`_2` file for any variant, and giving `max_3led`/`max_5led`
    /// no revision-suffixed file whatsoever (declared in `board.yml`, never
    /// actually backed at any revision — the same "don't trust the
    /// declared list" lesson the original fixture's `os_5led` taught).
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
      variants:
        - name: os
          cpucluster: cpuapp
        - name: max_3led
          cpucluster: cpuapp
        - name: max_5led
          cpucluster: cpuapp
  revision:
    format: custom
    default: "1"
    revisions:
      - name: "1"
      - name: "2"
      - name: "evt1"
"#,
        )
        .unwrap();

        // Devicetree source: the bare (variant-less) build and all three
        // named variants each have real .dts files — every one of these
        // four combinations is buildable at all, just not at every revision.
        for stem in [
            "roadrunner_nrf54l15_cpuapp",
            "roadrunner_nrf54l15_cpuapp_os",
            "roadrunner_nrf54l15_cpuapp_max_3led",
            "roadrunner_nrf54l15_cpuapp_max_5led",
        ] {
            fs::write(board_dir.join(format!("{stem}.dts")), "").unwrap();
        }

        // Bare + revision "2" is explicitly backed (matches the real static
        // config's actual day-to-day build). Bare has no "evt1" overlay —
        // that revision only exists as a product-tier variant.
        fs::write(board_dir.join("roadrunner_nrf54l15_cpuapp_2.overlay"), "").unwrap();
        // Only "os" actually has a real evt1 overlay — matches the real
        // finding: not every variant board.yml declares is really backed at
        // any revision. "max_3led"/"max_5led" get none at all, deliberately.
        fs::write(board_dir.join("roadrunner_nrf54l15_cpuapp_os_evt1.overlay"), "").unwrap();

        let app_dir = root.join("app/reference-dut");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();
    }

    #[test]
    fn bare_target_is_backed_at_default_and_explicit_revision_but_not_evt1() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        let bare_revisions: std::collections::BTreeSet<_> = targets
            .iter()
            .filter(|t| t.variant.is_none())
            .map(|t| t.revision.clone().unwrap())
            .collect();
        assert_eq!(
            bare_revisions,
            ["1", "2"].into_iter().map(String::from).collect(),
            "{targets:#?}"
        );
    }

    #[test]
    fn named_variant_is_never_backed_by_the_default_revision_shortcut() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        // "os" is declared at "1"/"2"/"evt1" but only really backed at
        // "evt1" — the default-revision shortcut ("1" is the default) must
        // not apply just because board.yml lists it, since a named variant
        // has no base-file fallback the way a bare build does.
        let os_revisions: std::collections::BTreeSet<_> = targets
            .iter()
            .filter(|t| t.variant.as_deref() == Some("os"))
            .map(|t| t.revision.clone().unwrap())
            .collect();
        assert_eq!(os_revisions, ["evt1"].into_iter().map(String::from).collect(), "{targets:#?}");
    }

    #[test]
    fn unbacked_variant_revision_combo_never_appears() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        // "max_3led"/"max_5led" are declared in board.yml with real .dts
        // files (so they're buildable in principle) but have no
        // revision-suffixed overlay/defconfig at any revision — must never
        // appear, unlike naively trusting board.yml's declared revision list.
        let bogus = targets
            .iter()
            .any(|t| matches!(t.variant.as_deref(), Some("max_3led") | Some("max_5led")));
        assert!(!bogus, "{targets:#?}");
    }

    #[test]
    fn no_board_yml_is_not_zephyr_west() {
        let dir = tempfile_dir();
        fs::create_dir_all(dir.path().join("app/foo")).unwrap();
        assert!(scan(dir.path()).is_err());
    }

    /// Regression test using the real reference-dut repo's actual
    /// `boards/nordic/dut_dev/board.yml` content verbatim (checked directly
    /// against the repo, not reconstructed from memory) plus its real
    /// per-revision overlay files, no synthetic simplification. Guards two
    /// real bugs an earlier version of this module had, both found only by
    /// checking against the real repo: a variant literally named `default`
    /// must not appear in the assembled filename stem or board qualifier
    /// (it's a schema placeholder here, not a real product variant — its
    /// name never appears in any real board filename), and `revision` lives
    /// under `board:`, not as a top-level sibling key.
    #[test]
    fn real_dut_dev_board_yml_resolves_every_real_revision() {
        let dir = tempfile_dir();
        let board_dir = dir.path().join("boards/nordic/dut_dev");
        fs::create_dir_all(&board_dir).unwrap();
        fs::write(
            board_dir.join("board.yml"),
            r#"
board:
  name: dut_dev
  full_name: Reference DUT Development Board
  vendor: nordic
  socs:
    - name: nrf54l15
      variants:
        - name: default
          cpucluster: cpuapp
  revision:
    format: number
    default: "3"
    exact: true
    revisions:
      - name: "3"
      - name: "6"
      - name: "7"
"#,
        )
        .unwrap();
        fs::write(board_dir.join("dut_dev_nrf54l15_cpuapp.dts"), "").unwrap();
        for rev in ["6", "7"] {
            fs::write(board_dir.join(format!("dut_dev_nrf54l15_cpuapp_{rev}.overlay")), "").unwrap();
        }
        let app_dir = dir.path().join("app/reference-dut");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        let targets = scan(dir.path()).unwrap();
        let revisions: std::collections::BTreeSet<_> =
            targets.iter().map(|t| t.revision.clone().unwrap()).collect();
        assert_eq!(
            revisions,
            ["3", "6", "7"].into_iter().map(String::from).collect(),
            "{targets:#?}"
        );
        assert!(targets.iter().all(|t| t.variant.is_none()), "{targets:#?}");

        let picked = select(&targets, None, None, Some("7"), None).unwrap();
        assert_eq!(picked.board_qualifier(), "dut_dev@7/nrf54l15/cpuapp");
    }

    #[test]
    fn select_narrows_to_singleton() {
        let dir = tempfile_dir();
        write_synthetic_repo(dir.path());
        let targets = scan(dir.path()).unwrap();

        let picked = select(&targets, None, Some("os"), Some("evt1"), None).unwrap();
        assert_eq!(picked.variant.as_deref(), Some("os"));
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
        // A SoC with no variants declared at all (real example: ref_nrf54dk's
        // nrf54l05) gets exactly one implicit variant — no name, no
        // cpucluster — so board/variant/revision/app never has to be typed
        // to disambiguate it.
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
"#,
        )
        .unwrap();
        fs::write(board_dir.join("single_nrf54l15.dts"), "").unwrap();
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
    fn soc_with_no_dts_backing_at_all_yields_no_targets() {
        // Real example this guards: ref_nrf54dk's board.yml declares
        // nrf54l05 with no variants at all, but the real repo has no
        // ref_nrf54dk_nrf54l05.dts file — must yield zero targets for that
        // SoC, not one bogus implicit-variant target that would fail at
        // CMake configure time.
        let dir = tempfile_dir();
        let board_dir = dir.path().join("boards/acme/nodts");
        fs::create_dir_all(&board_dir).unwrap();
        fs::write(
            board_dir.join("nodts.yml"),
            r#"
board:
  name: nodts
  socs:
    - name: nrf54l05
"#,
        )
        .unwrap();
        let app_dir = dir.path().join("app/dev");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        let targets = scan(dir.path()).unwrap();
        assert_eq!(targets, vec![], "{targets:#?}");
    }

    #[test]
    fn named_variant_with_no_dts_is_excluded_but_sibling_with_dts_survives() {
        // Real example this guards: ref_nrf54dk's nrf54l15 SoC declares two
        // variants (xip/cpuflpr, ns/cpuapp) that board.yml lists but that
        // have no .dts file at all — only a *bare* (no-variant) cpuapp .dts
        // exists. list_targets must offer exactly that bare target, not the
        // two declared-but-unbacked variants.
        let dir = tempfile_dir();
        let board_dir = dir.path().join("boards/acme/mixed");
        fs::create_dir_all(&board_dir).unwrap();
        fs::write(
            board_dir.join("mixed.yml"),
            r#"
board:
  name: mixed
  socs:
    - name: nrf54l15
      variants:
        - name: xip
          cpucluster: cpuflpr
        - name: ns
          cpucluster: cpuapp
"#,
        )
        .unwrap();
        // Only the bare cpuapp .dts is real — neither "xip"/cpuflpr nor
        // "ns"/cpuapp variant has its own .dts.
        fs::write(board_dir.join("mixed_nrf54l15_cpuapp.dts"), "").unwrap();
        let app_dir = dir.path().join("app/dev");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        let targets = scan(dir.path()).unwrap();
        assert_eq!(targets.len(), 1, "{targets:#?}");
        assert_eq!(targets[0].cpucluster.as_deref(), Some("cpuapp"));
        assert_eq!(targets[0].variant, None);
    }

    #[test]
    fn board_qualifier_formats_correctly() {
        let t = Target {
            board: "roadrunner".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: Some("os_5led".into()),
            revision: Some("evt1".into()),
            app: "reference-dut".into(),
        };
        assert_eq!(t.board_qualifier(), "roadrunner@evt1/nrf54l15/cpuapp/os_5led");
    }

    #[test]
    fn board_qualifier_omits_absent_axes() {
        let t = Target {
            board: "dut_dev".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: None,
            revision: None,
            app: "reference-dut".into(),
        };
        assert_eq!(t.board_qualifier(), "dut_dev/nrf54l15/cpuapp");
    }

    #[test]
    fn build_dir_name_is_stable_and_unique_per_target() {
        let a = Target {
            board: "roadrunner".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: Some("os_5led".into()),
            revision: Some("evt1".into()),
            app: "widget".into(),
        };
        assert_eq!(a.build_dir_name(&[]), "roadrunner-os_5led-evt1-widget");
    }

    #[test]
    fn build_dir_name_folds_in_snippets_so_they_dont_share_a_build_dir() {
        let a = Target {
            board: "roadrunner".into(),
            soc: "nrf54l15".into(),
            cpucluster: Some("cpuapp".into()),
            variant: Some("os_5led".into()),
            revision: Some("evt1".into()),
            app: "widget".into(),
        };
        let snippets = vec!["ble-shell".to_string(), "wdt31".to_string()];
        assert_eq!(
            a.build_dir_name(&snippets),
            "roadrunner-os_5led-evt1-widget-ble-shell_wdt31"
        );
        assert_ne!(a.build_dir_name(&snippets), a.build_dir_name(&[]));
    }

    #[test]
    fn scan_snippets_finds_every_real_snippet_regardless_of_nesting() {
        let dir = tempfile_dir();
        let app_dir = dir.path().join("app/widget");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        // One level deep, matching the real repo's layout.
        let ble_dir = dir.path().join("app/widget/snippets/ble-shell");
        fs::create_dir_all(&ble_dir).unwrap();
        fs::write(ble_dir.join("snippet.yml"), "name: ble-shell\n").unwrap();

        // Nested two levels deep — must still be found, since Zephyr's own
        // scanner (`os.walk`) doesn't assume a fixed depth either.
        let nested_dir = dir.path().join("app/widget/snippets/datalogging_cli/boards");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(
            dir.path()
                .join("app/widget/snippets/datalogging_cli/snippet.yml"),
            "name: datalogging_cli\n",
        )
        .unwrap();

        let boards_dir = dir.path().join("boards/acme/single");
        fs::create_dir_all(&boards_dir).unwrap();
        fs::write(
            boards_dir.join("single.yml"),
            "board:\n  name: single\n  socs:\n    - name: nrf54l15\n",
        )
        .unwrap();

        let found = available_snippets(dir.path(), "widget");
        assert_eq!(found, vec!["ble-shell".to_string(), "datalogging_cli".to_string()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>());
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
