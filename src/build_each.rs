//! `build --each <base>..<tip>` / `build_each`: every commit of a range, in
//! every build variant, each built from **its own** module sources (decision
//! 104).
//!
//! The module's sources for a commit come from one detached checkout kept
//! inside the west workspace (`<topdir>/.embarch-build-each/<project>/`), a
//! `git clone --shared` of the project's repository that only this tool ever
//! moves. West still lists the workspace's own checkout of the module, so the
//! build is also given `-DEXTRA_ZEPHYR_MODULES=<checkout>`: Zephyr keys
//! modules by name and the extra list is read last, so the checkout replaces
//! the workspace's module — its boards, drivers and Kconfig — instead of
//! sitting beside it. Without that, west builds the main checkout's boards
//! whatever the source directory says, which is how two field checks went
//! wrong.
//!
//! The engineer's own repository and working tree are never touched: the
//! clone is a separate repository sharing its objects read-only.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::build::{BuildLocks, BuildOutcome, BuildPlan};
use crate::config::{require_plain_west_build, ProjectConfig, DEFAULT_BUILD_VARIANT};
use crate::resolve::{self, BuildChoice, Selection};
use crate::verdict::Region;

/// Where the checkout and its build directories live, under the workspace.
pub const SCRATCH_DIR: &str = ".embarch-build-each";
/// The shortest hex run counted as a commit id when two images are compared.
const MIN_COMMIT_ID_HEX: usize = 7;

/// What a caller asked for.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub project: &'a ProjectConfig,
    pub selection: Selection<'a>,
    /// `<base>..<tip>`: the commits after `base` up to and including `tip`.
    pub range: &'a str,
    /// Build only this variant (or `default`); `None` builds the default and
    /// every configured variant.
    pub build_variant: Option<&'a str>,
    /// Assert the tip's tree equals this ref's, and that their images differ
    /// only in embedded commit ids.
    pub same_tree_as: Option<&'a str>,
    pub stop_at_first_failure: bool,
}

/// Splits `<base>..<tip>`, refusing `...`, which means something else to git.
pub fn parse_range(range: &str) -> Result<(&str, &str)> {
    if range.contains("...") {
        bail!("range '{range}' uses '...'; give <base>..<tip>, the commits after base up to tip");
    }
    match range.split_once("..") {
        Some((base, tip)) if !base.is_empty() && !tip.is_empty() => Ok((base, tip)),
        _ => bail!("range '{range}' is not <base>..<tip>"),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} in {} failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The variants a run builds, `default` first.
fn variants(project: &ProjectConfig, only: Option<&str>) -> Vec<String> {
    match only {
        Some(v) => vec![v.to_string()],
        None => std::iter::once(DEFAULT_BUILD_VARIANT.to_string())
            .chain(project.build_variants.keys().cloned())
            .collect(),
    }
}

/// The `west` binary a project builds with.
fn west_binary(project: &ProjectConfig) -> Result<PathBuf> {
    if project.is_zephyr_west() {
        return Ok(project.west_binary.clone().expect("validate() enforces west_binary for a zephyr-west project"));
    }
    require_plain_west_build(project, "is asked to build --each")?;
    Ok(PathBuf::from(&project.build_command.as_ref().expect("static has build_command")[0]))
}

/// The detached checkout this run moves from commit to commit, created on
/// first use as a shared clone of the project's repository.
fn checkout(project: &ProjectConfig, west: &Path) -> Result<(PathBuf, PathBuf)> {
    let out = Command::new(west)
        .arg("topdir")
        .current_dir(&project.source_path)
        .envs(&project.env)
        .output()
        .with_context(|| format!("failed to run {} topdir", west.display()))?;
    if !out.status.success() {
        bail!(
            "{} is not inside a west workspace ({} topdir: {}); build --each needs one to make a \
             commit's checkout visible to west",
            project.source_path.display(),
            west.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let topdir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let scratch = topdir.join(SCRATCH_DIR).join(crate::build::key_file_stem(&project.name));
    // The same directory name as the module's own: a module.yml with no
    // `name:` is named after its directory, and the override must match it.
    let name = project.source_path.file_name().context("source_path has no final component")?;
    let clone = scratch.join(name);
    if !clone.join(".git").exists() {
        std::fs::create_dir_all(&scratch).with_context(|| format!("failed to create {}", scratch.display()))?;
        git(&scratch, &["clone", "--quiet", "--shared", "--no-checkout", &project.source_path.display().to_string(), &name.to_string_lossy()])?;
    }
    Ok((scratch, clone))
}

/// Points the clone at `sha`. Only the clone moves; its untracked build
/// directories stay, so the next commit's build is incremental.
fn move_to(clone: &Path, sha: &str) -> Result<()> {
    git(clone, &["checkout", "--quiet", "--force", "--detach", sha]).map(|_| ())
}

/// The plan for `variant` built from `clone`, with the module override added
/// while the directory has no CMake cache yet (afterwards the cache keeps it).
/// Called right before each build: it also clears the stale version header.
fn plan_for(project: &ProjectConfig, clone: &Path, scratch: &Path, selection: Selection<'_>, variant: &str) -> Result<BuildPlan> {
    let mut p = project.clone();
    p.source_path = clone.to_path_buf();
    if p.is_zephyr_west() {
        p.build_dir_root = Some(scratch.join("build"));
    }
    let choice = BuildChoice { build_variant: Some(variant), pristine: false };
    let mut plan = resolve::resolve_build_with(&p, selection, choice)?.plan;
    // A static default build goes to west's `build/` under its working
    // directory; every other plan names its directory.
    let dir = plan.manifest.as_ref().map(|m| m.dir.clone()).unwrap_or_else(|| p.build_dir().join("build"));
    if !dir.join("CMakeCache.txt").is_file() {
        if !plan.command.iter().any(|a| a == "--") {
            plan.command.push("--".to_string());
        }
        plan.command.push(format!("-DEXTRA_ZEPHYR_MODULES={}", clone.display()));
    }
    plan.lock_key = format!("each::{}", plan.lock_key);
    // Zephyr regenerates `app_version.h` (`APP_BUILD_VERSION`, the app's
    // `git describe`) only when the app's VERSION file or Zephyr's own git
    // index changes, so an incremental build at the next commit keeps the
    // previous commit's id [measured 2026-11-10, Zephyr 4.4]. Removing the
    // generated file from this tool's own build directory makes ninja write
    // it again for every commit.
    let _ = std::fs::remove_file(dir.join("zephyr/include/generated/zephyr/app_version.h"));
    Ok(plan)
}

/// The image two builds are compared by: the raw `zephyr.bin` beside the
/// artifact when there is one (a hex carries per-record checksums that a
/// changed byte also changes), else the artifact itself.
fn image_path(outcome: &BuildOutcome) -> PathBuf {
    let bin = outcome.artifact_path.with_file_name("zephyr.bin");
    if bin.is_file() {
        bin
    } else {
        outcome.artifact_path.clone()
    }
}

/// How two images of the same tree differ.
#[derive(Debug, PartialEq, Eq)]
pub enum ImageDiff {
    Identical,
    /// Every differing byte sits in a run of hex digits that is part of the
    /// respective commit id in each image.
    OnlyCommitIds { ranges: usize },
    Differs { reason: String },
}

/// Compares two images of one tree built at two commits.
pub fn compare_images(tip: &[u8], other: &[u8], tip_sha: &str, other_sha: &str) -> ImageDiff {
    if tip.len() != other.len() {
        return ImageDiff::Differs { reason: format!("sizes differ: {} B against {} B", tip.len(), other.len()) };
    }
    let hex = |b: u8| b.is_ascii_hexdigit();
    let mut ranges = 0;
    let mut i = 0;
    while i < tip.len() {
        if tip[i] == other[i] {
            i += 1;
            continue;
        }
        let (mut s, mut e) = (i, i);
        while s > 0 && hex(tip[s - 1]) && hex(other[s - 1]) {
            s -= 1;
        }
        while e < tip.len() && hex(tip[e]) && hex(other[e]) {
            e += 1;
        }
        let ours = String::from_utf8_lossy(&tip[s..e]).to_ascii_lowercase();
        let theirs = String::from_utf8_lossy(&other[s..e]).to_ascii_lowercase();
        if e - s < MIN_COMMIT_ID_HEX || !tip_sha.contains(&ours) || !other_sha.contains(&theirs) {
            return ImageDiff::Differs { reason: format!("byte 0x{i:x} differs outside an embedded commit id") };
        }
        ranges += 1;
        i = e.max(i + 1);
    }
    if ranges == 0 {
        ImageDiff::Identical
    } else {
        ImageDiff::OnlyCommitIds { ranges }
    }
}

/// A region's change for the table: `+4`, or `-` with nothing to compare.
fn signed(delta: Option<&Value>, region: &str) -> String {
    delta
        .and_then(|d| d[region]["delta_bytes"].as_i64())
        .map_or("-".to_string(), |d| format!("{d:+}"))
}

struct Last {
    memory: BTreeMap<String, Region>,
    warnings: Vec<String>,
    image: Option<PathBuf>,
}

/// Runs the whole range and answers `(json, text)`; `Err` only when nothing
/// could be built at all (a bad range, no workspace).
pub async fn run(req: Request<'_>, locks: &BuildLocks) -> Result<(Value, String)> {
    let project = req.project;
    let (base, tip) = parse_range(req.range)?;
    let source = &project.source_path;
    let shas: Vec<String> = git(source, &["rev-list", "--reverse", &format!("{base}..{tip}")])?
        .lines()
        .map(str::to_string)
        .collect();
    if shas.is_empty() {
        bail!("{base}..{tip} has no commits in {}", source.display());
    }
    let tip_sha = git(source, &["rev-parse", &format!("{tip}^{{commit}}")])?;
    let names = variants(project, req.build_variant);
    let west = west_binary(project)?;
    let (scratch, clone) = checkout(project, &west)?;

    let mut last: BTreeMap<String, Last> = BTreeMap::new();
    let mut commits = Vec::new();
    let mut lines = Vec::new();
    let mut stopped = false;
    'commits: for sha in &shas {
        move_to(&clone, sha)?;
        let subject = git(source, &["log", "-1", "--format=%s", sha])?;
        let mut builds = Vec::new();
        for name in &names {
            let plan = plan_for(project, &clone, &scratch, req.selection, name)?;
            let outcome = locks.run_build(&plan).await?;
            let v = &outcome.verdict;
            let prev = last.get(name);
            let new_warnings: Vec<&String> = match prev {
                Some(p) => v.all_warnings.iter().filter(|w| !p.warnings.contains(w)).collect(),
                None => Vec::new(),
            };
            let delta = prev.map(|p| crate::verdict::memory_delta(&v.memory, &p.memory));
            let headroom = v.memory.get("FLASH").map(|r| r.total_bytes.saturating_sub(r.used_bytes));
            let flash = v.memory.get("FLASH").map(|r| r.used_bytes);
            let ram = v.memory.get("RAM").map(|r| r.used_bytes);
            let ok = outcome.build_succeeded();
            lines.push(format!(
                "{} {:<12} {:<4} {:>5.1}s warn {:>3} new {:>2}  FLASH {:>9} ({:>7})  RAM {:>8} ({:>6})  headroom {:>9}  {}",
                &sha[..sha.len().min(10)],
                name,
                if ok { "ok" } else { "FAIL" },
                outcome.seconds,
                v.warnings,
                new_warnings.len(),
                flash.map_or("-".to_string(), |b| b.to_string()),
                signed(delta.as_ref(), "FLASH"),
                ram.map_or("-".to_string(), |b| b.to_string()),
                signed(delta.as_ref(), "RAM"),
                headroom.map_or("-".to_string(), |b| b.to_string()),
                subject.chars().take(40).collect::<String>(),
            ));
            builds.push(json!({
                "build_variant": name,
                "success": ok,
                "exit_code": outcome.exit_code,
                "timed_out": outcome.timed_out,
                "seconds": (outcome.seconds * 10.0).round() / 10.0,
                "warnings": v.warnings,
                "new_warnings": new_warnings,
                "first_error": v.first_error,
                "memory": v.memory,
                "delta_from_previous_commit": delta,
                "flash_headroom_bytes": headroom,
                "log_path": outcome.log_path.as_ref().map(|p| p.display().to_string()),
            }));
            last.insert(
                name.clone(),
                Last {
                    memory: v.memory.clone(),
                    warnings: v.all_warnings.clone(),
                    image: (ok && sha == &tip_sha).then(|| image_path(&outcome)),
                },
            );
            if !ok && req.stop_at_first_failure {
                commits.push(json!({ "commit": sha, "subject": subject, "builds": builds }));
                stopped = true;
                break 'commits;
            }
        }
        commits.push(json!({ "commit": sha, "subject": subject, "builds": builds }));
    }

    let mut value = json!({
        "project": project.name,
        "range": req.range,
        "checkout": clone.display().to_string(),
        "commits": commits,
        "stopped_at_first_failure": stopped,
    });
    let all_ok = !stopped
        && value["commits"]
            .as_array()
            .is_some_and(|c| c.iter().all(|c| c["builds"].as_array().is_some_and(|b| b.iter().all(|b| b["success"] == true))));
    let mut success = all_ok;

    if let (Some(reference), false) = (req.same_tree_as, stopped) {
        let same = same_tree(req, &clone, &scratch, &names, &last, reference, &tip_sha, tip, locks).await?;
        success &= same["passed"] == true;
        lines.push(same_tree_text(&same));
        value["same_tree_as"] = same;
    }
    value["success"] = json!(success);
    let header = format!(
        "build --each {} '{}': {} commit(s) x {} variant(s), {}",
        req.range,
        project.name,
        value["commits"].as_array().map_or(0, Vec::len),
        names.len(),
        if success { "all passed" } else { "FAILED" }
    );
    lines.insert(0, header);
    Ok((value, lines.join("\n")))
}

#[allow(clippy::too_many_arguments)]
async fn same_tree(
    req: Request<'_>,
    clone: &Path,
    scratch: &Path,
    names: &[String],
    last: &BTreeMap<String, Last>,
    reference: &str,
    tip_sha: &str,
    tip: &str,
    locks: &BuildLocks,
) -> Result<Value> {
    let source = &req.project.source_path;
    let ref_sha = git(source, &["rev-parse", &format!("{reference}^{{commit}}")])?;
    let tree = |r: &str| git(source, &["rev-parse", &format!("{r}^{{tree}}")]);
    let tree_equal = tree(tip)? == tree(&ref_sha)?;
    let differing_paths: Vec<String> = if tree_equal {
        Vec::new()
    } else {
        git(source, &["diff", "--name-only", &ref_sha, tip_sha])?.lines().map(str::to_string).collect()
    };
    let mut images = Vec::new();
    let mut images_ok = true;
    if tree_equal {
        // The tip images are copied aside, then the reference is built in
        // the same directories, so both carry the same paths.
        let mut copies = BTreeMap::new();
        for name in names {
            if let Some(image) = last.get(name).and_then(|l| l.image.clone()) {
                let copy = scratch.join(format!("tip-{name}.img"));
                std::fs::copy(&image, &copy).with_context(|| format!("failed to copy {}", image.display()))?;
                copies.insert(name.clone(), copy);
            }
        }
        move_to(clone, &ref_sha)?;
        for name in names {
            let Some(copy) = copies.get(name) else {
                images_ok = false;
                images.push(json!({ "build_variant": name, "result": "not compared: the tip did not build" }));
                continue;
            };
            let plan = plan_for(req.project, clone, scratch, req.selection, name)?;
            let outcome = locks.run_build(&plan).await?;
            if !outcome.build_succeeded() {
                images_ok = false;
                images.push(json!({ "build_variant": name, "result": "not compared: the reference did not build", "log_path": outcome.log_path }));
                continue;
            }
            let ours = std::fs::read(copy)?;
            let theirs = std::fs::read(image_path(&outcome))?;
            let (result, ok) = match compare_images(&ours, &theirs, tip_sha, &ref_sha) {
                ImageDiff::Identical => ("identical".to_string(), true),
                ImageDiff::OnlyCommitIds { ranges } => (format!("differs only in embedded commit ids ({ranges} run(s))"), true),
                ImageDiff::Differs { reason } => (reason, false),
            };
            images_ok &= ok;
            images.push(json!({ "build_variant": name, "result": result, "passed": ok }));
        }
        move_to(clone, tip_sha)?;
    }
    Ok(json!({
        "reference": reference,
        "reference_commit": ref_sha,
        "tree_equal": tree_equal,
        "differing_paths": differing_paths,
        "images": images,
        "passed": tree_equal && images_ok,
    }))
}

fn same_tree_text(v: &Value) -> String {
    let reference = v["reference"].as_str().unwrap_or("?");
    if v["tree_equal"] != true {
        let paths: Vec<&str> = v["differing_paths"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        return format!("same tree as {reference}: NO — differs in {}", paths.join(", "));
    }
    let images: Vec<String> = v["images"]
        .as_array()
        .map(|a| a.iter().map(|i| format!("{}: {}", i["build_variant"].as_str().unwrap_or("?"), i["result"].as_str().unwrap_or("?"))).collect())
        .unwrap_or_default();
    format!(
        "same tree as {reference}: {} — {}",
        if v["passed"] == true { "yes" } else { "NO" },
        images.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_base_dot_dot_tip() {
        assert_eq!(parse_range("main..HEAD").unwrap(), ("main", "HEAD"));
        assert!(parse_range("main...HEAD").is_err());
        assert!(parse_range("HEAD").is_err());
        assert!(parse_range("..HEAD").is_err());
    }

    const TIP: &str = "4f2c9a1b7d3e5f60718293a4b5c6d7e8f9012345";
    const REF: &str = "a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f70819";

    #[test]
    fn identical_images_are_identical() {
        assert_eq!(compare_images(b"abc", b"abc", TIP, REF), ImageDiff::Identical);
    }

    #[test]
    fn images_differing_only_in_the_embedded_commit_id_pass() {
        let ours = b"\x00\x01ver g4f2c9a1b7d3e\x00\xff";
        let theirs = b"\x00\x01ver ga0b1c2d3e4f5\x00\xff";
        assert_eq!(compare_images(ours, theirs, TIP, REF), ImageDiff::OnlyCommitIds { ranges: 1 });
    }

    #[test]
    fn any_other_difference_fails_naming_where() {
        let ours = b"\x00\x01ver g4f2c9a1b7d3e\x00\xff";
        let theirs = b"\x00\x02ver ga0b1c2d3e4f5\x00\xff";
        assert!(matches!(compare_images(ours, theirs, TIP, REF), ImageDiff::Differs { reason } if reason.contains("0x1")));
        // A hex run that is not the other commit's id is not tolerated.
        let theirs = b"\x00\x01ver gdeadbeefdead\x00\xff";
        assert!(matches!(compare_images(ours, theirs, TIP, REF), ImageDiff::Differs { .. }));
        assert!(matches!(compare_images(b"ab", b"abc", TIP, REF), ImageDiff::Differs { reason } if reason.contains("sizes")));
    }
}
