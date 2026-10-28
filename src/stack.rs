//! `embarch-api stack` — fold review fixes into the commits they belong to and
//! restack a PR stack, as one command family with its own safeguards.
//! `tasks/api/130`, `embarch-api` decision 96.
//!
//! * `plan` splits every hunk above the last reviewed point (uncommitted work,
//!   and commits above `--since`) by which commit in `<base>..` last touched
//!   its lines, and commits one `fixup! <sha>` per target. Hunks no commit owns
//!   (new files, lines from before the base, deletions, binaries) stay in the
//!   working tree and are written to a generated plan file.
//! * `fold` backs up every branch it will move, queues message corrections as
//!   `amend!` commits, autosquashes non-interactively with `--update-refs`,
//!   and proves the tree is the one it started with. It never pushes.
//! * `restack <bottom>..<top>` rebases the PR branches onto the new tip of the
//!   one below with `--update-refs`, then pushes them all in one
//!   `git push --atomic` with a `--force-with-lease` per branch.
//!
//! Host-side only: it needs `git` (2.38+ for `merge-tree --write-tree`), no
//! config and no embarch-core, so `main` dispatches it before either is
//! looked for, like `patches`.

use crate::patches::{parse_diff, Hunk};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum StackAction {
    /// Commit `fixup!` commits for every placeable hunk above --since (and in
    /// the working tree); write the hunks it cannot place to the plan file.
    Plan {
        /// Bottom of the range commits are attributed to (default: the
        /// branch's upstream, else origin/main, main, master).
        base: Option<String>,
        /// The last reviewed commit: commits above it are split by hunk and
        /// replaced by fixups. Default: HEAD (only the working tree is split).
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Where to write the plan (default: `<git dir>/embarch-stack-plan.md`).
        #[arg(long)]
        plan_file: Option<PathBuf>,
        /// Write the plan file only; commit and move nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Back up, autosquash `fixup!`/`amend!` commits and apply the curation
    /// file; prove the tree is unchanged. Never pushes.
    Fold {
        base: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Message corrections, one per line: `<commit> | <new subject>`
        /// (`#` starts a comment). The body is kept.
        #[arg(long)]
        curation: Option<PathBuf>,
    },
    /// Rebase each branch of `<bottom>..<top>` onto the new tip of the one
    /// below, then push them all atomically with leases.
    Restack {
        /// `<bottom>..<top>`, two branch names.
        range: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Rebase the whole stack onto this ref (e.g. `origin/main` after the
        /// bottom PR landed by rebase merge); commits already in it are
        /// dropped by patch-id.
        #[arg(long)]
        onto: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Restack locally and push nothing.
        #[arg(long)]
        no_push: bool,
    },
}

pub fn run(action: StackAction, json: bool) -> i32 {
    let result = match action {
        StackAction::Plan { base, since, repo, plan_file, dry_run } => {
            plan(&repo, base.as_deref(), since.as_deref(), plan_file.as_deref(), dry_run)
        }
        StackAction::Fold { base, repo, curation } => fold(&repo, base.as_deref(), curation.as_deref()),
        StackAction::Restack { range, repo, onto, remote, no_push } => {
            restack(&repo, &range, onto.as_deref(), &remote, no_push)
        }
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

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run_git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Out> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args).env("GIT_EDITOR", "true").env("GIT_SEQUENCE_EDITOR", "true");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().context("could not run `git`")?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let o = run_git(dir, args, env)?;
    if !o.ok {
        bail!("`git {}` failed: {} {}", args.join(" "), o.stderr.trim(), o.stdout.trim());
    }
    Ok(o.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[])
}

fn rev(dir: &Path, what: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", "--quiet", what])
        .map_err(|_| anyhow!("`{what}` is not a revision here"))?
        .trim()
        .to_string())
}

fn try_rev(dir: &Path, what: &str) -> Option<String> {
    rev(dir, what).ok()
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    run_git(dir, &["merge-base", "--is-ancestor", a, b], &[]).map(|o| o.ok).unwrap_or(false)
}

fn count(dir: &Path, what: &str) -> usize {
    git(dir, &["rev-list", "--count", what]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn git_dir(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"])?.trim()))
}

fn current_branch(dir: &Path) -> Result<String> {
    let o = run_git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], &[])?;
    if !o.ok {
        bail!("HEAD is detached; check out the branch to operate on");
    }
    Ok(o.stdout.trim().to_string())
}

fn require_clean(dir: &Path) -> Result<()> {
    if !git(dir, &["status", "--porcelain"])?.trim().is_empty() {
        bail!("the working tree is dirty; commit, stash or run `stack plan` first (this command refuses to rewrite history under uncommitted work)");
    }
    Ok(())
}

fn default_base(dir: &Path) -> Result<String> {
    if let Some(u) = try_rev(dir, "@{upstream}") {
        let _ = u;
        return Ok("@{upstream}".to_string());
    }
    for cand in ["origin/main", "main", "master"] {
        if try_rev(dir, cand).is_some() {
            return Ok(cand.to_string());
        }
    }
    bail!("no <base> given and no upstream, origin/main, main or master to default to");
}

fn no_merges(dir: &Path, range: &str) -> Result<()> {
    if !git(dir, &["rev-list", "--merges", range])?.trim().is_empty() {
        bail!("{range} contains a merge commit; only linear history can be folded");
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Every local branch except `backup/*`, with its tip.
fn branches(dir: &Path) -> Result<Vec<(String, String)>> {
    let out = git(dir, &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(n, _)| !n.starts_with("backup/"))
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect())
}

/// The shas recorded by `refs/backup/<branch>-<12 hex>` refs.
fn backups_of(dir: &Path, name: &str) -> Result<Vec<String>> {
    let prefix = format!("backup/{name}-");
    let out = git(dir, &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/backup"])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(n, _)| {
            n.strip_prefix(&prefix).is_some_and(|r| r.len() == 12 && r.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .map(|(_, s)| s.to_string())
        .collect())
}

#[derive(Clone, Debug)]
struct Backup {
    branch: String,
    sha: String,
    name: String,
}

fn backup(dir: &Path, branch: &str, sha: &str) -> Result<Backup> {
    let name = format!("backup/{branch}-{}", short(sha));
    // Outside refs/heads on purpose: `rebase --update-refs` moves every
    // *branch* that points into the range, and a backup it moved is no backup.
    git(dir, &["update-ref", &format!("refs/{name}"), sha])?;
    Ok(Backup { branch: branch.to_string(), sha: sha.to_string(), name })
}

/// Put every backed-up branch back at its backup sha.
fn restore(dir: &Path, current: &str, backups: &[Backup]) {
    for b in backups {
        if b.branch == current {
            let _ = git(dir, &["reset", "--hard", "--quiet", &b.sha]);
        } else {
            let _ = git(dir, &["update-ref", &format!("refs/heads/{}", b.branch), &b.sha]);
        }
    }
}

/// What a stopped rebase was doing: the commit it stopped on and the
/// conflicted files. Read before the rebase is aborted.
fn conflict_report(dir: &Path) -> String {
    let files = git(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    let files: Vec<&str> = files.lines().collect();
    let commit = try_rev(dir, "REBASE_HEAD")
        .map(|c| {
            let subject = git(dir, &["log", "-1", "--format=%s", &c]).unwrap_or_default();
            format!("{} '{}'", short(&c), subject.trim())
        })
        .unwrap_or_else(|| "an unknown commit".to_string());
    format!("conflict applying {commit} in {}", if files.is_empty() { "(no unmerged files listed)".to_string() } else { files.join(", ") })
}

// ---------------------------------------------------------------- plan

fn split_file_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = vec![];
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            blocks.push(String::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks
}

struct Unplaced {
    file: String,
    reason: String,
    text: String,
}

/// The newest target (by position) whose lines `h` replaces, or `None` when
/// the lines are older than the range or blame cannot say.
fn attribute(dir: &Path, since: &str, targets: &[String], file: &str, h: &Hunk) -> Option<usize> {
    let (a, b) = if h.old_len == 0 {
        let l = h.old_start.max(1);
        (l, l)
    } else {
        (h.old_start, h.old_start + h.old_len - 1)
    };
    let out = git(dir, &["blame", "--porcelain", "-L", &format!("{a},{b}"), since, "--", file]).ok()?;
    let mut best: Option<usize> = None;
    for line in out.lines() {
        let mut parts = line.split(' ');
        let tok = parts.next().unwrap_or("");
        if tok.len() == 40 && tok.chars().all(|c| c.is_ascii_hexdigit()) && line.split(' ').count() >= 3 {
            if let Some(i) = targets.iter().position(|c| c == tok) {
                best = Some(best.map_or(i, |x: usize| x.max(i)));
            }
        }
    }
    best
}

fn plan(repo: &Path, base: Option<&str>, since: Option<&str>, plan_file: Option<&Path>, dry_run: bool) -> Result<(Value, String)> {
    let base = match base {
        Some(b) => b.to_string(),
        None => default_base(repo)?,
    };
    let head = rev(repo, "HEAD")?;
    let since = match since {
        Some(s) => rev(repo, s)?,
        None => head.clone(),
    };
    if !is_ancestor(repo, &since, &head) {
        bail!("--since must be an ancestor of HEAD");
    }
    if !is_ancestor(repo, &base, &since) {
        bail!("<base> must be an ancestor of --since (the reviewed point)");
    }
    let range = format!("{base}..{since}");
    no_merges(repo, &format!("{base}..{head}"))?;
    let targets: Vec<String> = git(repo, &["rev-list", "--reverse", &range])?.lines().map(str::to_string).collect();
    if targets.is_empty() {
        bail!("no commits in {range} to attribute hunks to");
    }
    let subject_of = |c: &str| git(repo, &["log", "-1", "--format=%s", c]).map(|s| s.trim().to_string()).unwrap_or_default();

    // Everything that changed since the reviewed point, commits and working tree alike.
    let diff = git(repo, &["diff", "-U0", "--no-color", "--no-ext-diff", "--no-renames", &since])?;
    let mut texts: BTreeMap<usize, String> = BTreeMap::new();
    let mut hunks_per_target: BTreeMap<usize, usize> = BTreeMap::new();
    let mut unplaced: Vec<Unplaced> = vec![];
    for block in split_file_blocks(&diff) {
        let file = block
            .lines()
            .next()
            .and_then(|l| l.rsplit_once(" b/"))
            .map(|(_, p)| p.to_string())
            .unwrap_or_default();
        if block.contains("\nBinary files") || block.contains("\nGIT binary patch") {
            unplaced.push(Unplaced { file, reason: "binary change".into(), text: block });
            continue;
        }
        if block.contains("\ndeleted file mode") || block.contains("\nrename ") {
            unplaced.push(Unplaced { file, reason: "deletion or rename".into(), text: block });
            continue;
        }
        for f in parse_diff(&block)? {
            for h in &f.hunks {
                let hunk_text = h.lines.join("\n") + "\n";
                if f.is_new {
                    unplaced.push(Unplaced { file: f.path.clone(), reason: "new file: no commit in range owns it".into(), text: hunk_text });
                    continue;
                }
                match attribute(repo, &since, &targets, &f.path, h) {
                    Some(t) => {
                        let e = texts.entry(t).or_default();
                        if !e.contains(&format!("{}\n@@", f.header.join("\n"))) {
                            e.push_str(&f.header.join("\n"));
                            e.push('\n');
                        }
                        e.push_str(&hunk_text);
                        *hunks_per_target.entry(t).or_default() += 1;
                    }
                    None => unplaced.push(Unplaced {
                        file: f.path.clone(),
                        reason: "no commit in range owns these lines".into(),
                        text: format!("{}\n{hunk_text}", f.header.join("\n")),
                    }),
                }
            }
        }
    }
    for f in git(repo, &["ls-files", "--others", "--exclude-standard"])?.lines() {
        unplaced.push(Unplaced { file: f.to_string(), reason: "untracked new file".into(), text: String::new() });
    }

    // One commit per target, each on its own off `since`, then chained with a
    // three-way merge so no working tree is touched while they are built.
    let gd = git_dir(repo)?;
    let idx = gd.join("embarch-stack-index");
    let idx_s = idx.to_string_lossy().to_string();
    let patch_file = gd.join("embarch-stack-group.diff");
    let mut tip = since.clone();
    let mut chain = vec![];
    for (t, text) in &texts {
        let env = [("GIT_INDEX_FILE", idx_s.as_str())];
        git_env(repo, &["read-tree", &since], &env)?;
        std::fs::write(&patch_file, text)?;
        let applied = git_env(repo, &["apply", "--cached", "--unidiff-zero", &patch_file.to_string_lossy()], &env);
        let _ = std::fs::remove_file(&patch_file);
        applied.with_context(|| format!("applying the hunks that belong to {}", short(&targets[*t])))?;
        let tree = git_env(repo, &["write-tree"], &env)?.trim().to_string();
        let _ = std::fs::remove_file(&idx);
        let group = git(repo, &["commit-tree", &tree, "-p", &since, "-m", "group"])?.trim().to_string();
        let merged = run_git(repo, &["merge-tree", "--write-tree", &format!("--merge-base={since}"), &tip, &group], &[])?;
        if !merged.ok {
            bail!("the hunks for {} conflict with an earlier fixup: {}", short(&targets[*t]), merged.stdout.trim());
        }
        let merged_tree = merged.stdout.lines().next().unwrap_or("").trim().to_string();
        tip = git(repo, &["commit-tree", &merged_tree, "-p", &tip, "-m", &format!("fixup! {}", targets[*t])])?.trim().to_string();
        chain.push(*t);
    }

    let mut backup_name = None;
    if !dry_run && !chain.is_empty() {
        let branch = current_branch(repo)?;
        backup_name = Some(backup(repo, &branch, &head)?.name);
        git(repo, &["reset", "--mixed", "--quiet", &tip])?;
    }

    let plan_path = match plan_file {
        Some(p) => p.to_path_buf(),
        None => gd.join("embarch-stack-plan.md"),
    };
    let mut md = format!("# stack plan\n\nbase `{base}`, reviewed point `{}`, HEAD `{}`\n\n## Placed\n\n", short(&since), short(&head));
    for t in &chain {
        md.push_str(&format!(
            "- `fixup! {}` {} ({} hunk{})\n",
            short(&targets[*t]),
            subject_of(&targets[*t]),
            hunks_per_target[t],
            if hunks_per_target[t] == 1 { "" } else { "s" }
        ));
    }
    if chain.is_empty() {
        md.push_str("- nothing placed\n");
    }
    md.push_str("\n## Not placed (left in the working tree)\n\n");
    for u in &unplaced {
        md.push_str(&format!("### {} - {}\n\n", u.file, u.reason));
        if !u.text.is_empty() {
            md.push_str(&format!("```diff\n{}```\n\n", u.text));
        }
    }
    if unplaced.is_empty() {
        md.push_str("- nothing\n");
    }
    std::fs::write(&plan_path, &md).with_context(|| format!("writing {}", plan_path.display()))?;

    let placed_json: Vec<Value> = chain
        .iter()
        .map(|t| json!({ "target": targets[*t], "subject": subject_of(&targets[*t]), "hunks": hunks_per_target[t] }))
        .collect();
    let unplaced_json: Vec<Value> = unplaced.iter().map(|u| json!({ "file": u.file, "reason": u.reason })).collect();
    let human = format!(
        "{} fixup commit(s){}; {} hunk(s)/file(s) not placed. Plan: {}{}",
        chain.len(),
        if dry_run { " (dry run, nothing committed)" } else { "" },
        unplaced.len(),
        plan_path.display(),
        backup_name.as_ref().map(|b| format!("\nbackup: {b}")).unwrap_or_default()
    );
    Ok((
        json!({
            "success": true, "dry_run": dry_run, "base": base, "since": since, "head": tip,
            "backup": backup_name, "placed": placed_json, "unplaced": unplaced_json,
            "plan_file": plan_path.to_string_lossy(),
        }),
        human,
    ))
}

// ---------------------------------------------------------------- fold

fn fold(repo: &Path, base: Option<&str>, curation: Option<&Path>) -> Result<(Value, String)> {
    require_clean(repo)?;
    let branch = current_branch(repo)?;
    let base = match base {
        Some(b) => b.to_string(),
        None => default_base(repo)?,
    };
    let head = rev(repo, "HEAD")?;
    if !is_ancestor(repo, &base, &head) {
        bail!("<base> `{base}` is not an ancestor of HEAD");
    }
    let range = format!("{base}..{head}");
    no_merges(repo, &range)?;
    let in_range: Vec<String> = git(repo, &["rev-list", &range])?.lines().map(str::to_string).collect();
    if in_range.is_empty() {
        bail!("no commits in {range}");
    }
    // Resolve the curation up front: a bad line must stop before anything moves.
    let mut amends: Vec<(String, String)> = vec![];
    if let Some(file) = curation {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (target, subject) = line
                .split_once('|')
                .ok_or_else(|| anyhow!("{}:{}: expected `<commit> | <new subject>`", file.display(), n + 1))?;
            let sha = rev(repo, &format!("{}^{{commit}}", target.trim()))
                .with_context(|| format!("{}:{}", file.display(), n + 1))?;
            if !in_range.contains(&sha) {
                bail!("{}:{}: {} is not in {range}", file.display(), n + 1, target.trim());
            }
            amends.push((sha, subject.trim().to_string()));
        }
    }

    // A backup of every branch the rebase can move: the checked-out one and
    // every other branch pointing into the range.
    let mut backups = vec![backup(repo, &branch, &head)?];
    for (name, sha) in branches(repo)? {
        if name != branch && in_range.contains(&sha) {
            backups.push(backup(repo, &name, &sha)?);
        }
    }
    let tree_before = rev(repo, "HEAD^{tree}")?;

    let gd = git_dir(repo)?;
    for (sha, subject) in &amends {
        let body = git(repo, &["log", "-1", "--format=%b", sha])?;
        let msg_file = gd.join("embarch-stack-msg");
        let mut msg = format!("amend! {sha}\n\n{subject}\n");
        if !body.trim().is_empty() {
            msg.push_str(&format!("\n{}\n", body.trim_end()));
        }
        std::fs::write(&msg_file, msg)?;
        let r = git(repo, &["commit", "--allow-empty", "--no-verify", "--quiet", "--cleanup=verbatim", "-F", &msg_file.to_string_lossy()]);
        let _ = std::fs::remove_file(&msg_file);
        if let Err(e) = r {
            restore(repo, &branch, &backups);
            return Err(e.context("queueing a message correction"));
        }
    }

    let rebased = run_git(repo, &["rebase", "-i", "--autosquash", "--update-refs", &base], &[])?;
    if !rebased.ok {
        let report = conflict_report(repo);
        let _ = git(repo, &["rebase", "--abort"]);
        restore(repo, &branch, &backups);
        bail!("fold stopped: {report}. {branch} is back at {} (nothing else moved).", backups[0].name);
    }
    let tree_after = rev(repo, "HEAD^{tree}")?;
    if tree_after != tree_before {
        restore(repo, &branch, &backups);
        bail!("the folded tip's tree ({}) differs from the pre-fold tip's ({}); {branch} is back at {}", short(&tree_after), short(&tree_before), backups[0].name);
    }
    let new_head = rev(repo, "HEAD")?;
    let after = count(repo, &format!("{base}..HEAD"));
    let human = format!(
        "folded {} commit(s) into {after}; tree {} unchanged; nothing pushed.\nbackups: {}",
        in_range.len(),
        short(&tree_after),
        backups.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    Ok((
        json!({
            "success": true, "branch": branch, "base": base, "head_before": head, "head_after": new_head,
            "tree": tree_after, "commits_before": in_range.len(), "commits_after": after,
            "amended": amends.len(),
            "backups": backups.iter().map(|b| json!({ "branch": b.branch, "ref": b.name, "sha": b.sha })).collect::<Vec<_>>(),
            "pushed": false,
        }),
        human,
    ))
}

// ---------------------------------------------------------------- restack

/// The branch's own tip or the newest backup of it that `top` still contains.
fn effective_tip(dir: &Path, name: &str, tip: &str, top: &str) -> Result<Option<String>> {
    let mut cands = vec![tip.to_string()];
    cands.extend(backups_of(dir, name)?);
    Ok(cands
        .into_iter()
        .filter(|c| is_ancestor(dir, c, top))
        .max_by_key(|c| count(dir, c)))
}

fn stack_branches(dir: &Path, bottom: &str, top: &str) -> Result<Vec<String>> {
    let all = branches(dir)?;
    let tip_of = |n: &str| all.iter().find(|(b, _)| b == n).map(|(_, s)| s.clone());
    let bottom_tip = tip_of(bottom).ok_or_else(|| anyhow!("`{bottom}` is not a local branch"))?;
    let top_tip = tip_of(top).ok_or_else(|| anyhow!("`{top}` is not a local branch"))?;
    let mut bottom_cands = vec![bottom_tip.clone()];
    bottom_cands.extend(backups_of(dir, bottom)?);
    if !bottom_cands.iter().any(|c| is_ancestor(dir, c, &top_tip)) {
        bail!("`{top}` does not descend from `{bottom}` or from any backup of it");
    }
    let mut members: Vec<(usize, String)> = vec![];
    for (name, tip) in &all {
        let Some(eff) = effective_tip(dir, name, tip, &top_tip)? else { continue };
        if name != bottom && bottom_cands.iter().any(|c| is_ancestor(dir, &eff, c)) {
            continue; // at or below the bottom of the stack
        }
        members.push((count(dir, &eff), name.clone()));
    }
    members.sort();
    let names: Vec<String> = members.into_iter().map(|(_, n)| n).collect();
    if names.first().map(String::as_str) != Some(bottom) || names.last().map(String::as_str) != Some(top) {
        bail!("could not order a linear stack from `{bottom}` to `{top}`; found {}", names.join(", "));
    }
    Ok(names)
}

fn restack(repo: &Path, range: &str, onto: Option<&str>, remote: &str, no_push: bool) -> Result<(Value, String)> {
    let (bottom, top) = range
        .split_once("..")
        .filter(|(a, b)| !a.is_empty() && !b.is_empty() && !b.starts_with('.'))
        .ok_or_else(|| anyhow!("expected `<bottom>..<top>` (two branch names), got `{range}`"))?;
    require_clean(repo)?;
    let original = run_git(repo, &["symbolic-ref", "--short", "-q", "HEAD"], &[])?.stdout.trim().to_string();
    let stack = stack_branches(repo, bottom, top)?;
    let before: BTreeMap<String, String> = branches(repo)?.into_iter().filter(|(n, _)| stack.contains(n)).collect();
    let mut backups = vec![];
    for n in &stack {
        backups.push(backup(repo, n, &before[n])?);
    }
    let fail = |msg: String, backups: &[Backup]| -> anyhow::Error {
        let report = conflict_report(repo);
        let _ = git(repo, &["rebase", "--abort"]);
        restore(repo, &original, backups);
        anyhow!("{msg}: {report}. Every branch is back at its backup/ ref; nothing was pushed.")
    };

    let mut landed: Vec<String> = vec![];
    if let Some(onto) = onto {
        let onto_sha = rev(repo, onto)?;
        for n in &stack {
            let cherry = git(repo, &["cherry", &onto_sha, n])?;
            if !cherry.lines().any(|l| l.starts_with('+')) {
                landed.push(n.clone());
            }
        }
        let live: Vec<&String> = stack.iter().filter(|n| !landed.contains(n)).collect();
        if let Some(live_top) = live.last() {
            let r = run_git(repo, &["rebase", "--update-refs", &onto_sha, live_top], &[])?;
            if !r.ok {
                return Err(fail(format!("rebasing the stack onto {onto} failed"), &backups));
            }
        }
    } else {
        for _ in 0..stack.len() {
            let tips: BTreeMap<String, String> = branches(repo)?.into_iter().collect();
            let Some(i) = (1..stack.len()).find(|&i| !is_ancestor(repo, &tips[&stack[i - 1]], &tips[&stack[i]])) else { break };
            let prev = &stack[i - 1];
            let cur_tip = &tips[&stack[i]];
            let old_base = backups_of(repo, prev)?
                .into_iter()
                .filter(|c| is_ancestor(repo, c, cur_tip))
                .max_by_key(|c| count(repo, c))
                .ok_or_else(|| {
                    restore(repo, &original, &backups);
                    anyhow!("`{}` is not based on `{prev}` and no backup of `{prev}` is an ancestor of it, so what it was based on is unknown; nothing moved", stack[i])
                })?;
            let r = run_git(repo, &["rebase", "--update-refs", "--onto", &tips[prev], &old_base, top], &[])?;
            if !r.ok {
                return Err(fail(format!("rebasing `{}` onto `{prev}` failed", stack[i]), &backups));
            }
        }
    }
    let after: BTreeMap<String, String> = branches(repo)?.into_iter().filter(|(n, _)| stack.contains(n)).collect();
    let live: Vec<&String> = stack.iter().filter(|n| !landed.contains(n)).collect();
    for w in live.windows(2) {
        if !is_ancestor(repo, &after[w[0]], &after[w[1]]) {
            restore(repo, &original, &backups);
            bail!("`{}` is still not on top of `{}` after restacking; every branch is back at its backup", w[1], w[0]);
        }
    }
    if !original.is_empty() && current_branch(repo).ok().as_deref() != Some(original.as_str()) {
        let _ = git(repo, &["switch", "--quiet", &original]);
    }

    // One atomic push; every lease names the sha this checkout last saw.
    let mut pushed = false;
    if !no_push && !live.is_empty() {
        let mut args: Vec<String> = vec!["push".into(), "--atomic".into()];
        for n in &live {
            let expected = try_rev(repo, &format!("refs/remotes/{remote}/{n}")).unwrap_or_default();
            args.push(format!("--force-with-lease={n}:{expected}"));
        }
        args.push(remote.to_string());
        for n in &live {
            args.push(format!("refs/heads/{n}:refs/heads/{n}"));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = run_git(repo, &arg_refs, &[])?;
        if !o.ok {
            bail!(
                "the atomic push was refused, so nothing was sent (a remote branch moved since this checkout last fetched, or the remote is unreachable): {}\nthe branches are restacked locally; backups: {}",
                o.stderr.trim(),
                backups.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        pushed = true;
    }
    let human = format!(
        "restacked {} branch(es){}{}.\nbackups: {}",
        live.len(),
        if landed.is_empty() { String::new() } else { format!(", {} already landed ({})", landed.len(), landed.join(", ")) },
        if pushed { ", pushed atomically with leases" } else { ", nothing pushed" },
        backups.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    Ok((
        json!({
            "success": true, "pushed": pushed, "landed": landed,
            "branches": stack.iter().map(|n| json!({ "branch": n, "before": before[n], "after": after[n] })).collect::<Vec<_>>(),
            "backups": backups.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
        }),
        human,
    ))
}
