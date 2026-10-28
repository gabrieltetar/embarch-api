//! `embarch-api patches` — round-trip a west manifest's module patch series
//! (`patches.yml` plus the patch files `west patch` applies) through one
//! scratch checkout. `tasks/api/129`, `embarch-api` decision 92.
//!
//! * `export` clones the module at a base revision into a scratch directory
//!   and applies the series with `git am`: one commit per patch, in
//!   `patches.yml` order, message and author from the patch header.
//! * `import` writes patch files back from that branch. A commit whose id is
//!   the one `export` recorded is left byte-for-byte alone; a rewritten one
//!   is regenerated with stable headers. Commits stacked on top of the
//!   series ("Fix:" commits) are split hunk by hunk — by which patch last
//!   touched the lines — and folded into those patches' commits first.
//! * `regroup` merges/renumbers per a map file and proves the module tree is
//!   identical (same git tree id) before and after.
//!
//! Host-side only: it needs `git`, no config and no embarch-core, so `main`
//! dispatches it before either is looked for, like `versions`.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const BRANCH: &str = "embarch-patches/series";
const STATE_FILE: &str = "embarch-patches.json";

#[derive(Subcommand, Debug)]
pub enum PatchesAction {
    /// Create (or reuse) the scratch checkout: module at --base with one
    /// commit per patch file, in patches.yml order.
    Export {
        /// The manifest's patches.yml.
        #[arg(long)]
        series: PathBuf,
        /// Git repository of the module the patches apply to.
        #[arg(long)]
        repo: PathBuf,
        /// Revision of --repo the series applies on (the manifest's pin).
        #[arg(long)]
        base: String,
        /// Scratch directory to create or reuse.
        #[arg(long)]
        scratch: PathBuf,
        /// Discard an existing scratch tree and start again.
        #[arg(long)]
        force: bool,
    },
    /// Write patch files and patches.yml sha256 fields back from the scratch
    /// branch, folding any commits stacked on the series into the patches
    /// their hunks belong to.
    Import {
        #[arg(long)]
        scratch: PathBuf,
    },
    /// Merge and renumber patches per --map, proving the module tree is
    /// unchanged. Map lines: `new/path.patch: 0001, 0002 | optional subject`.
    Regroup {
        #[arg(long)]
        scratch: PathBuf,
        /// The map file; `#` starts a comment.
        #[arg(long)]
        map: PathBuf,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StateEntry {
    path: String,
    commit: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct State {
    series: PathBuf,
    base: String,
    entries: Vec<StateEntry>,
}

pub fn run(action: PatchesAction, json: bool) -> i32 {
    let result = match action {
        PatchesAction::Export { series, repo, base, scratch, force } => {
            export(&series, &repo, &base, &scratch, force)
        }
        PatchesAction::Import { scratch } => import(&scratch),
        PatchesAction::Regroup { scratch, map } => regroup(&scratch, &map),
    };
    match result {
        Ok((value, human)) => {
            if json {
                println!("{}", embarch_api::json_out::pretty(value));
            } else {
                println!("{human}");
            }
            0
        }
        Err(e) => {
            let message = format!("{e:#}");
            if json {
                println!(
                    "{}",
                    embarch_api::json_out::pretty(json!({ "success": false, "error": message }))
                );
            } else {
                eprintln!("{message}");
            }
            1
        }
    }
}

// ---------------------------------------------------------------- git

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_COMMITTER_NAME", "embarch-patches")
        .env("GIT_COMMITTER_EMAIL", "patches@embarch.invalid");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().context("could not run `git`")?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            format!("{} {}", String::from_utf8_lossy(&out.stderr).trim(), String::from_utf8_lossy(&out.stdout).trim()).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git").current_dir(dir).args(args).output().context("could not run `git`")?;
    if !out.status.success() {
        bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn rev(dir: &Path, what: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", what])?.trim().to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- patches.yml

#[derive(Clone, Debug)]
struct Entry {
    path: String,
    lines: Vec<String>,
}

#[derive(Debug)]
struct Yml {
    pre: Vec<String>,
    entries: Vec<Entry>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.split(" #").next().unwrap_or(s).trim();
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

fn parse_yml(text: &str) -> Result<Yml> {
    let mut y = Yml { pre: vec![], entries: vec![] };
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("- path:") {
            y.entries.push(Entry { path: unquote(rest), lines: vec![line.to_string()] });
        } else if let Some(e) = y.entries.last_mut() {
            e.lines.push(line.to_string());
        } else {
            y.pre.push(line.to_string());
        }
    }
    if y.entries.is_empty() {
        bail!("patches.yml lists no `- path:` entries");
    }
    Ok(y)
}

fn render_yml(y: &Yml) -> String {
    let mut out = String::new();
    for l in y.pre.iter().chain(y.entries.iter().flat_map(|e| e.lines.iter())) {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn set_path(e: &mut Entry, path: &str) {
    let indent: String = e.lines[0].chars().take_while(|c| c.is_whitespace()).collect();
    e.lines[0] = format!("{indent}- path: {path}");
    e.path = path.to_string();
}

/// Set the entry's `sha256sum`, returning whether the text changed.
fn set_sha(e: &mut Entry, sha: &str) -> bool {
    let key_indent = {
        let first = &e.lines[0];
        let lead = first.len() - first.trim_start().len();
        " ".repeat(lead + 2)
    };
    for l in e.lines.iter_mut().skip(1) {
        if l.trim_start().starts_with("sha256sum:") {
            let new = format!("{key_indent}sha256sum: {sha}");
            let changed = *l != new;
            *l = new;
            return changed;
        }
    }
    e.lines.insert(1, format!("{key_indent}sha256sum: {sha}"));
    true
}

fn series_dir(series: &Path) -> PathBuf {
    series.parent().map(Path::to_path_buf).unwrap_or_default()
}

// ---------------------------------------------------------------- state

fn state_path(scratch: &Path) -> PathBuf {
    scratch.join(".git").join(STATE_FILE)
}

fn load_state(scratch: &Path) -> Result<State> {
    let p = state_path(scratch);
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("{} is not an `embarch-api patches export` scratch tree", scratch.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn save_state(scratch: &Path, s: &State) -> Result<()> {
    std::fs::write(state_path(scratch), serde_json::to_string_pretty(s)?)?;
    Ok(())
}

fn require_clean(scratch: &Path) -> Result<()> {
    if !git(scratch, &["status", "--porcelain"])?.trim().is_empty() {
        bail!("{} has uncommitted changes; commit or discard them first", scratch.display());
    }
    Ok(())
}

fn series_commits(scratch: &Path, base: &str) -> Result<Vec<String>> {
    Ok(git(scratch, &["rev-list", "--reverse", &format!("{base}..HEAD")])?
        .lines()
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------- export

fn export(series: &Path, repo: &Path, base: &str, scratch: &Path, force: bool) -> Result<(Value, String)> {
    let series = std::fs::canonicalize(series).with_context(|| format!("{}", series.display()))?;
    let yml = parse_yml(&std::fs::read_to_string(&series)?)?;
    if scratch.exists() {
        if state_path(scratch).exists() && !force {
            let st = load_state(scratch)?;
            return Ok((
                json!({"success": true, "reused": true, "scratch": scratch, "patches": st.entries.len(),
                       "head": rev(scratch, "HEAD")?}),
                format!("reused {} ({} patches); pass --force to rebuild", scratch.display(), st.entries.len()),
            ));
        }
        if !state_path(scratch).exists() {
            bail!("{} exists and is not an embarch-patches scratch tree; refusing to touch it", scratch.display());
        }
        std::fs::remove_dir_all(scratch)?;
    }
    let repo = std::fs::canonicalize(repo)?;
    let base_sha = rev(&repo, &format!("{base}^{{commit}}"))?;
    let parent = scratch.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    git(parent, &["clone", "--quiet", "--shared", "--no-checkout", &repo.to_string_lossy(), &scratch.to_string_lossy()])?;
    for (k, v) in [("user.name", "embarch-patches"), ("user.email", "patches@embarch.invalid"),
                   ("commit.gpgsign", "false"), ("core.autocrlf", "false"), ("rebase.autoSquash", "true")] {
        git(scratch, &["config", k, v])?;
    }
    git(scratch, &["checkout", "--quiet", "-b", BRANCH, &base_sha])?;
    let dir = series_dir(&series);
    let mut entries = vec![];
    for e in &yml.entries {
        let file = dir.join(&e.path);
        git(scratch, &["am", "--quiet", "--committer-date-is-author-date", &file.to_string_lossy()])
            .with_context(|| format!("applying {}", e.path))?;
        entries.push(StateEntry { path: e.path.clone(), commit: rev(scratch, "HEAD")? });
    }
    save_state(scratch, &State { series, base: base_sha.clone(), entries: entries.clone() })?;
    Ok((
        json!({"success": true, "reused": false, "scratch": scratch, "base": base_sha,
               "patches": entries.iter().map(|e| json!({"path": e.path, "commit": e.commit})).collect::<Vec<_>>()}),
        format!("exported {} patches onto {} in {}", entries.len(), &base_sha[..12], scratch.display()),
    ))
}

// ---------------------------------------------------------------- diff parsing for folds

pub(crate) struct Hunk {
    pub(crate) old_start: usize,
    pub(crate) old_len: usize,
    pub(crate) lines: Vec<String>,
}

pub(crate) struct FileDiff {
    pub(crate) path: String,
    pub(crate) header: Vec<String>,
    pub(crate) is_new: bool,
    pub(crate) hunks: Vec<Hunk>,
}

fn parse_range(s: &str) -> (usize, usize) {
    let s = s.trim_start_matches(['-', '+']);
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

pub(crate) fn parse_diff(text: &str) -> Result<Vec<FileDiff>> {
    let mut files: Vec<FileDiff> = vec![];
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map(|(_, p)| p.to_string()).unwrap_or_default();
            files.push(FileDiff { path, header: vec![line.to_string()], is_new: false, hunks: vec![] });
            continue;
        }
        let f = files.last_mut().ok_or_else(|| anyhow!("unexpected diff output"))?;
        if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            parts.next();
            let (old_start, old_len) = parse_range(parts.next().unwrap_or(""));
            f.hunks.push(Hunk { old_start, old_len, lines: vec![line.to_string()] });
        } else if let Some(h) = f.hunks.last_mut() {
            h.lines.push(line.to_string());
        } else {
            if line.starts_with("new file mode") {
                f.is_new = true;
            }
            if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                bail!("{}: a binary change cannot be attributed hunk by hunk; amend it into its patch by hand", f.path);
            }
            if line.starts_with("deleted file mode") || line.starts_with("rename ") {
                bail!("{}: a deletion or rename cannot be attributed hunk by hunk; amend it into its patch by hand", f.path);
            }
            f.header.push(line.to_string());
        }
    }
    Ok(files)
}

fn commit_files(scratch: &Path, c: &str) -> Result<Vec<String>> {
    Ok(git(scratch, &["diff-tree", "--no-commit-id", "--name-only", "-r", c])?.lines().map(str::to_string).collect())
}

/// Fold every commit stacked on `series_head` into the series commits its
/// hunks belong to. Returns `(fix subject, patch paths it landed in)`.
fn fold_fixes(scratch: &Path, st: &State, fixes: &[String]) -> Result<Vec<(String, Vec<String>)>> {
    let mut report = vec![];
    for fix in fixes {
        let parents = git(scratch, &["rev-list", "--parents", "-n", "1", fix])?;
        if parents.split_whitespace().count() != 2 {
            bail!("{fix} is a merge or root commit; only linear fix commits can be folded");
        }
        let subject = git(scratch, &["log", "-1", "--format=%s", fix])?.trim().to_string();
        let head = rev(scratch, "HEAD")?;
        // The series as it stands now: positional commits base..(head minus unfolded fixes).
        let all = series_commits(scratch, &st.base)?;
        let n = st.entries.len();
        let series: Vec<String> = all.into_iter().take(n).collect();
        let tip = series.last().cloned().unwrap_or_else(|| st.base.clone());
        // Rebuilt series head must carry the tree the fix applies to.
        let fix_parent = rev(scratch, &format!("{fix}^"))?;
        if rev(scratch, &format!("{tip}^{{tree}}"))? != rev(scratch, &format!("{fix_parent}^{{tree}}"))? {
            bail!("series tip and the parent of fix {} differ in tree; fold fixes in order", &fix[..12]);
        }
        let mut files_of = vec![];
        for c in &series {
            files_of.push(commit_files(scratch, c)?);
        }
        let diff = git(scratch, &["diff", "-U0", "--no-color", "--no-ext-diff", "--no-renames", &fix_parent, fix])?;
        // One patch text per target commit: each file's header once, then its hunks.
        let mut texts: BTreeMap<usize, String> = BTreeMap::new();
        for f in parse_diff(&diff)? {
            for h in &f.hunks {
                let target = if f.is_new { n - 1 } else { attribute(scratch, &tip, &series, &files_of, &f.path, h)? };
                let t = texts.entry(target).or_default();
                if !t.contains(&format!("{}\n@@", f.header.join("\n"))) {
                    t.push_str(&f.header.join("\n"));
                    t.push('\n');
                }
                t.push_str(&h.lines.join("\n"));
                t.push('\n');
            }
        }
        // The fixups stack on the series tip, not on the fix commits (still
        // reachable by id; `head` restores them if a fold fails).
        git(scratch, &["reset", "--quiet", "--hard", &tip])?;
        let mut landed = vec![];
        let mut fixups = 0;
        for (target, text) in &texts {
            // C_G = fix's parent tree + only this group's hunks, applied to a temp index.
            let idx = scratch.join(".git").join("embarch-patches-index");
            let idx_s = idx.to_string_lossy().to_string();
            let env = [("GIT_INDEX_FILE", idx_s.as_str())];
            git_env(scratch, &["read-tree", &fix_parent], &env)?;
            let patch_file = scratch.join(".git").join("embarch-patches-group.diff");
            std::fs::write(&patch_file, text)?; 
            git_env(scratch, &["apply", "--cached", "--unidiff-zero", &patch_file.to_string_lossy()], &env)
                .with_context(|| format!("applying the hunks of fix {} that belong to {}", &fix[..12], st.entries[*target].path))?;
            let tree = git_env(scratch, &["write-tree"], &env)?.trim().to_string();
            let _ = std::fs::remove_file(&idx);
            let _ = std::fs::remove_file(&patch_file);
            let c_g = git(scratch, &["commit-tree", &tree, "-p", &fix_parent, "-m", "group"])?.trim().to_string();
            git(scratch, &["cherry-pick", "--no-commit", &c_g]).map_err(|e| {
                let _ = git(scratch, &["cherry-pick", "--abort"]);
                let _ = git(scratch, &["reset", "--hard", &head]);
                anyhow!("{e:#} (while folding into {})", st.entries[*target].path)
            })?;
            git(scratch, &["commit", "--quiet", "-m", &format!("fixup! {}", series[*target])])?;
            fixups += 1;
            landed.push(st.entries[*target].path.clone());
        }
        if fixups == 0 {
            continue;
        }
        let final_tree = rev(scratch, "HEAD^{tree}")?;
        git(scratch, &["rebase", "--quiet", "-i", "--autosquash", &st.base]).map_err(|e| {
            let _ = git(scratch, &["rebase", "--abort"]);
            anyhow!("{e:#} (folding fix {})", &fix[..12])
        })?;
        if rev(scratch, "HEAD^{tree}")? != final_tree {
            bail!("folding fix {} changed the tree; refusing to continue", &fix[..12]);
        }
        report.push((subject, landed));
    }
    Ok(report)
}

/// Which series commit last touched the lines a hunk changes.
fn attribute(scratch: &Path, tip: &str, series: &[String], files_of: &[Vec<String>], file: &str, h: &Hunk) -> Result<usize> {
    let (a, b) = if h.old_len == 0 {
        let l = h.old_start.max(1);
        (l, l)
    } else {
        (h.old_start, h.old_start + h.old_len - 1)
    };
    let out = git(scratch, &["blame", "--porcelain", "-L", &format!("{a},{b}"), tip, "--", file])?;
    let mut best: Option<usize> = None;
    for line in out.lines() {
        let tok = line.split(' ').next().unwrap_or("");
        if tok.len() == 40 && tok.chars().all(|c| c.is_ascii_hexdigit()) && line.split(' ').count() >= 3 {
            if let Some(i) = series.iter().position(|c| c == tok) {
                best = Some(best.map_or(i, |b: usize| b.max(i)));
            }
        }
    }
    if let Some(i) = best {
        return Ok(i);
    }
    // Lines no patch introduced: the last patch that touches this file.
    files_of
        .iter()
        .rposition(|fs| fs.iter().any(|f| f == file))
        .ok_or_else(|| anyhow!("{file}: no patch touches this file, so the hunk at line {a} belongs to none; add it to a patch by hand"))
}

// ---------------------------------------------------------------- import

/// The text inside `[...]` of an existing patch file's `Subject:` line, so a
/// regenerated patch keeps its `PATCH 2/3` numbering (or lack of it) and a
/// commit that is only re-parented does not show up as a changed file.
fn existing_prefix(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let line = text.lines().find(|l| l.starts_with("Subject: ["))?;
    let inner = line.strip_prefix("Subject: [")?.split(']').next()?;
    Some(inner.to_string())
}

fn format_patch(scratch: &Path, commit: &str, prefix: Option<String>) -> Result<Vec<u8>> {
    let prefix = format!("--subject-prefix={}", prefix.unwrap_or_else(|| "PATCH".into()));
    git_bytes(scratch, &["format-patch", "--stdout", "--zero-commit", "--no-signature", &prefix, "-1", commit])
}

fn import(scratch: &Path) -> Result<(Value, String)> {
    let mut st = load_state(scratch)?;
    require_clean(scratch)?;
    let n = st.entries.len();
    let all = series_commits(scratch, &st.base)?;
    let recorded_tip = st.entries.last().map(|e| e.commit.clone()).unwrap_or_else(|| st.base.clone());
    let mut folded = vec![];
    let fixes: Vec<String> = match all.iter().position(|c| *c == recorded_tip) {
        Some(i) if i + 1 == n => all[i + 1..].to_vec(),
        _ => {
            if all.len() != n {
                bail!(
                    "the branch has {} commits over the base but the series has {} patches, and the series tip was rewritten; \
                     import cannot tell edited patches from fix commits. Use `regroup` to change the patch count",
                    all.len(), n
                );
            }
            vec![]
        }
    };
    if !fixes.is_empty() {
        folded = fold_fixes(scratch, &st, &fixes)?;
    }
    let commits = series_commits(scratch, &st.base)?;
    if commits.len() != n {
        bail!("after folding, the branch has {} commits for {} patches", commits.len(), n);
    }
    let dir = series_dir(&st.series);
    let mut yml = parse_yml(&std::fs::read_to_string(&st.series)?)?;
    if yml.entries.len() != n || yml.entries.iter().zip(&st.entries).any(|(a, b)| a.path != b.path) {
        bail!("patches.yml no longer lists the paths that were exported; re-export (or use `regroup`)");
    }
    let mut changed = vec![];
    let mut unchanged = 0;
    let mut yml_changed = false;
    for (i, c) in commits.iter().enumerate() {
        let file = dir.join(&st.entries[i].path);
        if *c != st.entries[i].commit {
            let bytes = format_patch(scratch, c, existing_prefix(&file))?;
            if std::fs::read(&file).ok().as_deref() != Some(&bytes[..]) {
                std::fs::write(&file, &bytes)?;
                changed.push(st.entries[i].path.clone());
            } else {
                unchanged += 1;
            }
        } else {
            unchanged += 1;
        }
        let sha = sha256_hex(&std::fs::read(&file)?);
        yml_changed |= set_sha(&mut yml.entries[i], &sha);
        st.entries[i].commit = c.clone();
    }
    if yml_changed {
        std::fs::write(&st.series, render_yml(&yml))?;
    }
    save_state(scratch, &st)?;
    let human = format!(
        "{} patch file(s) rewritten{}{}; {} unchanged; patches.yml {}",
        changed.len(),
        if changed.is_empty() { String::new() } else { format!(": {}", changed.join(", ")) },
        if folded.is_empty() { String::new() } else { format!("; folded {} fix commit(s)", folded.len()) },
        unchanged,
        if yml_changed { "updated" } else { "unchanged" }
    );
    Ok((
        json!({"success": true, "changed": changed, "unchanged": unchanged, "patches_yml_updated": yml_changed,
               "folded": folded.iter().map(|(s, p)| json!({"fix": s, "into": p})).collect::<Vec<_>>()}),
        human,
    ))
}

// ---------------------------------------------------------------- regroup

struct Group {
    new_path: String,
    sources: Vec<usize>,
    subject: Option<String>,
}

fn leading_digits(path: &str) -> Option<String> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let d: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
    if d.is_empty() { None } else { Some(d) }
}

fn parse_map(text: &str, entries: &[StateEntry]) -> Result<Vec<Group>> {
    let mut groups = vec![];
    let mut used = vec![false; entries.len()];
    for (ln, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (new_path, rest) = line.split_once(':').ok_or_else(|| anyhow!("map line {}: expected `new.patch: old, old`", ln + 1))?;
        let (olds, subject) = match rest.split_once('|') {
            Some((o, s)) => (o, Some(s.trim().to_string())),
            None => (rest, None),
        };
        let mut sources = vec![];
        for tok in olds.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
            let idx = entries
                .iter()
                .position(|e| e.path == tok || e.path.rsplit('/').next() == Some(tok))
                .or_else(|| {
                    let want: Option<u64> = tok.parse().ok();
                    let hits: Vec<usize> = entries.iter().enumerate()
                        .filter(|(_, e)| leading_digits(&e.path).and_then(|d| d.parse::<u64>().ok()) == want && want.is_some())
                        .map(|(i, _)| i).collect();
                    if hits.len() == 1 { Some(hits[0]) } else { None }
                })
                .ok_or_else(|| anyhow!("map line {}: `{tok}` names no single patch in the series", ln + 1))?;
            if used[idx] {
                bail!("map line {}: patch {} is used twice", ln + 1, entries[idx].path);
            }
            used[idx] = true;
            sources.push(idx);
        }
        if sources.is_empty() {
            bail!("map line {}: no source patches", ln + 1);
        }
        groups.push(Group { new_path: new_path.trim().to_string(), sources, subject });
    }
    if let Some(i) = used.iter().position(|u| !u) {
        bail!("the map leaves out {}; every patch must appear exactly once", entries[i].path);
    }
    Ok(groups)
}

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !(c.is_alphanumeric() || c == '.' || c == '_'))
}

/// After a number a `.` is just a full stop; only a letter, digit or `_` joins it to a word.
fn ends_token(c: Option<char>) -> bool {
    c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
}

/// Rewrite patch-number references in a message. Returns the new text, the
/// `(from, to)` pairs rewritten, and the numbers it saw but could not map.
fn rewrite_refs(msg: &str, width: usize, map: &BTreeMap<String, String>) -> (String, Vec<(String, String)>, Vec<String>) {
    let mut out = String::new();
    let (mut done, mut missed) = (vec![], vec![]);
    for line in msg.split_inclusive('\n') {
        let chars: Vec<char> = line.chars().collect();
        let lower = line.to_lowercase();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_digit() && is_boundary(i.checked_sub(1).map(|j| chars[j])) {
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let tok: String = chars[i..j].iter().collect();
                let before: String = chars[..i].iter().collect::<String>().to_lowercase();
                let named = chars.get(j) == Some(&'-') && lower[before.len().min(lower.len())..].contains(".patch");
                if tok.len() == width && ends_token(chars.get(j).copied().filter(|c| *c != '-')) && (before.contains("patch") || named) {
                    match map.get(&tok) {
                        Some(new) => {
                            done.push((tok.clone(), new.clone()));
                            out.push_str(new);
                        }
                        None => {
                            missed.push(tok.clone());
                            out.push_str(&tok);
                        }
                    }
                } else {
                    out.push_str(&tok);
                }
                i = j;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    (out, done, missed)
}

fn regroup(scratch: &Path, map_file: &Path) -> Result<(Value, String)> {
    let mut st = load_state(scratch)?;
    require_clean(scratch)?;
    let head = rev(scratch, "HEAD")?;
    let tip = st.entries.last().map(|e| e.commit.clone()).unwrap_or_else(|| st.base.clone());
    if head != tip {
        bail!("the branch has commits the patch files do not; run `patches import` first");
    }
    let groups = parse_map(&std::fs::read_to_string(map_file)?, &st.entries)?;
    let width = st.entries.iter().filter_map(|e| leading_digits(&e.path)).map(|d| d.len()).next().unwrap_or(0);
    let mut numbers: BTreeMap<String, String> = BTreeMap::new();
    for g in &groups {
        if let Some(newd) = leading_digits(&g.new_path) {
            for s in &g.sources {
                if let Some(oldd) = leading_digits(&st.entries[*s].path) {
                    numbers.insert(oldd, newd.clone());
                }
            }
        }
    }
    let old_count = st.entries.len();
    let old_tree = rev(scratch, "HEAD^{tree}")?;
    git(scratch, &["branch", "-f", "embarch-patches/before-regroup", &head])?;
    git(scratch, &["checkout", "--quiet", "-B", "embarch-patches/regroup", &st.base])?;
    let mut new_entries = vec![];
    let (mut rewritten, mut unresolved) = (vec![], vec![]);
    let restore = |scratch: &Path| {
        let _ = git(scratch, &["cherry-pick", "--abort"]);
        let _ = git(scratch, &["reset", "--hard", "--quiet"]);
        let _ = git(scratch, &["checkout", "--quiet", "-B", BRANCH, "embarch-patches/before-regroup"]);
    };
    for g in &groups {
        for s in &g.sources {
            if let Err(e) = git(scratch, &["cherry-pick", "--no-commit", &st.entries[*s].commit]) {
                restore(scratch);
                bail!("merging {} into {}: {e:#}", st.entries[*s].path, g.new_path);
            }
        }
        let first = &st.entries[g.sources[0]].commit;
        let meta = git(scratch, &["log", "-1", "--format=%an%n%ae%n%aI", first])?;
        let mut m = meta.lines();
        let (an, ae, ad) = (m.next().unwrap_or(""), m.next().unwrap_or(""), m.next().unwrap_or(""));
        let mut message = String::new();
        for (k, s) in g.sources.iter().enumerate() {
            let body = git(scratch, &["log", "-1", "--format=%B", &st.entries[*s].commit])?;
            let body = body.trim_end();
            if k == 0 {
                match &g.subject {
                    Some(sub) => {
                        let rest = body.split_once('\n').map(|(_, r)| r).unwrap_or("");
                        message.push_str(sub);
                        message.push('\n');
                        message.push_str(rest);
                    }
                    None => message.push_str(body),
                }
            } else {
                message.push_str("\n\n");
                message.push_str(body);
            }
        }
        message.push('\n');
        let (message, done, missed) = if width > 0 { rewrite_refs(&message, width, &numbers) } else { (message, vec![], vec![]) };
        for (a, b) in done {
            rewritten.push(json!({"in": g.new_path, "from": a, "to": b}));
        }
        for t in missed {
            unresolved.push(json!({"in": g.new_path, "number": t}));
        }
        let res = git_env(
            scratch,
            &["commit", "--quiet", "--allow-empty", "-m", &message],
            &[("GIT_AUTHOR_NAME", an), ("GIT_AUTHOR_EMAIL", ae), ("GIT_AUTHOR_DATE", ad), ("GIT_COMMITTER_DATE", ad)],
        );
        if let Err(e) = res {
            restore(scratch);
            bail!("{e:#}");
        }
        new_entries.push(StateEntry { path: g.new_path.clone(), commit: rev(scratch, "HEAD")? });
    }
    let new_tree = rev(scratch, "HEAD^{tree}")?;
    if new_tree != old_tree {
        let _ = git(scratch, &["checkout", "--quiet", "-B", BRANCH, "embarch-patches/before-regroup"]);
        bail!("regroup would change the module tree ({} before, {} after); nothing was written", &old_tree[..12], &new_tree[..12]);
    }
    git(scratch, &["checkout", "--quiet", "-B", BRANCH])?;
    // Patch files and patches.yml.
    let dir = series_dir(&st.series);
    let mut yml = parse_yml(&std::fs::read_to_string(&st.series)?)?;
    let mut out_entries = vec![];
    for (g, ne) in groups.iter().zip(&new_entries) {
        let file = dir.join(&g.new_path);
        let bytes = format_patch(scratch, &ne.commit, None)?;
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&file, &bytes)?;
        let mut e = yml.entries[g.sources[0]].clone();
        set_path(&mut e, &g.new_path);
        set_sha(&mut e, &sha256_hex(&bytes));
        out_entries.push(e);
    }
    let kept: Vec<&str> = groups.iter().map(|g| g.new_path.as_str()).collect();
    let mut removed = vec![];
    for old in &st.entries {
        if !kept.contains(&old.path.as_str()) {
            let _ = std::fs::remove_file(dir.join(&old.path));
            removed.push(old.path.clone());
        }
    }
    yml.entries = out_entries;
    std::fs::write(&st.series, render_yml(&yml))?;
    st.entries = new_entries;
    save_state(scratch, &st)?;
    let human = format!(
        "{} patches -> {}; module tree {} identical; {} reference(s) rewritten, {} could not be",
        old_count,
        groups.len(),
        &new_tree[..12],
        rewritten.len(),
        unresolved.len()
    );
    Ok((
        json!({"success": true, "tree": new_tree, "tree_identical": true, "patches": groups.len(),
               "removed": removed, "references_rewritten": rewritten, "references_unresolved": unresolved}),
        human,
    ))
}
