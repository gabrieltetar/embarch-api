use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

/// Cap on captured stdout/stderr text handed back through MCP, so a runaway
/// build log doesn't blow up the tool response.
///
/// This bounds the **retained log bytes**: head plus tail together never
/// exceed it. The one marker line describing the cut sits on top of it, as
/// it always has — the cap is not doubled by the split.
///
/// `pub` so `tests/build_capture.rs` can size its fixtures against the real
/// cap instead of restating 65536 and drifting from it.
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// The share of [`OUTPUT_CAP_BYTES`] spent on the *head* of an over-cap log;
/// the rest goes to the tail.
///
/// A Zephyr build's first error is usually the actionable one and everything
/// after it is cascade, so the head has to survive — but the tail carries the
/// failing recipe and the final summary, which is where a reader looks first.
/// 16 KB is far more than one compiler diagnostic plus the `cmake`/Kconfig
/// preamble it follows, and cheap against the 48 KB left for the tail.
pub const OUTPUT_HEAD_BYTES: usize = 16 * 1024;

/// Everything a build actually needs to run, independent of whether it came
/// from a `discovery = "static"` project (today's fully-static schema) or a
/// `discovery = "zephyr-west"` project's live, per-call target resolution
/// (`resolve.rs`, `design.md` §3 decision 12) — `build.rs` itself doesn't
/// know or care which produced it.
pub struct BuildPlan {
    /// Locks per distinct build output, not just per project: two different
    /// targets of the same `zephyr-west` project (different board/variant)
    /// build into different directories and shouldn't serialize against
    /// each other, only against themselves.
    pub lock_key: String,
    /// Working directory the build command runs in.
    pub cwd: PathBuf,
    /// Full argv, program included (split via `.split_first()` below).
    pub command: Vec<String>,
    pub artifact_path: PathBuf,
    pub timeout_secs: u64,
    pub env: HashMap<String, String>,
    /// The provenance file dropped beside the build output, or `None` where
    /// the directory is already self-describing (`design.md` §3 decision
    /// 19). `Some` only for a `zephyr-west` target, whose directory *name*
    /// is lossy: `extra_args` is folded into it as a hash, and `-` is legal
    /// inside a board, app and snippet name, so the name cannot be parsed
    /// back into the selection that produced it. A `static` project resolves
    /// no selection at all and dev-bench builds into west's default `build/`.
    pub manifest: Option<TargetManifest>,
}

/// The resolved selection, and the per-target build directory it is written
/// into as [`TARGET_MANIFEST_NAME`].
pub struct TargetManifest {
    /// The build directory itself — `BuildPlan::cwd` is the *source* tree
    /// for a `zephyr-west` build (`west build -d` is absolute), and
    /// `artifact_path` is a file well inside the output, so neither of them
    /// names this.
    pub dir: PathBuf,
    /// **The same `serde_json::Value` the tool response echoes back as its
    /// descriptor**, not a second serialization of the same facts: a
    /// directory's provenance and the answer the caller was given cannot
    /// then drift apart.
    pub target: serde_json::Value,
}

/// What a build directory's provenance file is called.
pub const TARGET_MANIFEST_NAME: &str = "target.json";

/// Tolerance absorbing wall-clock read jitter between the parent's
/// pre-spawn `SystemTime::now()` and whatever clock stamped the child's
/// file write — observed on WSL2 as the child's mtime landing a few ms
/// *before* the parent's own timestamp (Hyper-V/WSL2 clock-sync jitter
/// between two reads taken microseconds apart, not mtime-resolution
/// truncation). A build takes at least seconds, so this grace can't mask a
/// genuinely stale artifact from a previous run.
pub const FRESHNESS_CLOCK_GRACE: Duration = Duration::from_millis(500);

pub struct BuildOutcome {
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub artifact_path: PathBuf,
    pub artifact_fresh: bool,
}

impl BuildOutcome {
    pub fn build_succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    pub fn ready_to_flash(&self) -> bool {
        self.build_succeeded() && self.artifact_fresh
    }
}

/// Per-project build locks, so two overlapping build/build_and_flash calls
/// for the same project can't stomp the same output directory. Separate
/// concern from Core's own hardware lock (USB contention) — this guards the
/// build workspace only.
#[derive(Default)]
pub struct BuildLocks {
    locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl BuildLocks {
    pub fn new() -> BuildLocks {
        BuildLocks::default()
    }

    fn lock_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("build locks poisoned");
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub async fn run_build(&self, plan: &BuildPlan) -> Result<BuildOutcome> {
        let lock = self.lock_for(&plan.lock_key);
        let _guard = lock.lock().await;
        run_build_locked(plan).await
    }
}

/// Keeps the first [`OUTPUT_HEAD_BYTES`] and the last
/// `OUTPUT_CAP_BYTES - OUTPUT_HEAD_BYTES` of a captured stream, dropping the
/// middle behind a marker that says how much went and what was kept. Under
/// the cap the text is returned untouched and unmarked.
///
/// **Both cuts land on a UTF-8 character boundary, and that is the whole
/// point**: slicing a `str` at an index inside a codepoint **panics**, so a
/// build whose log happens to cross either offset mid-`é` would take the MCP
/// server down rather than return a truncated log. The head cut rounds
/// *down* to a boundary and the tail cut rounds *up*, so an adjustment can
/// only ever drop bytes — head plus tail stays within the cap by
/// construction.
///
/// `pub` so `tests/build_capture.rs` can hold both boundaries directly as
/// well as through a real child process.
pub fn truncate_log(s: String) -> String {
    if s.len() <= OUTPUT_CAP_BYTES {
        return s;
    }
    let original_len = s.len();
    let head_end = floor_char_boundary(&s, OUTPUT_HEAD_BYTES);
    let tail_start = ceil_char_boundary(&s, original_len - (OUTPUT_CAP_BYTES - OUTPUT_HEAD_BYTES));
    // `tail_start` starts strictly above `OUTPUT_HEAD_BYTES` whenever the log
    // is over the cap, and rounding only moves the two further apart, so the
    // kept halves never overlap and something is always dropped.
    let dropped = tail_start - head_end;
    let head_len = head_end;
    let tail_len = original_len - tail_start;
    format!(
        "{head}\n...[{dropped} bytes dropped from the middle of a {original_len}-byte log; \
         kept the first {head_len} and last {tail_len}, cap {OUTPUT_CAP_BYTES}]...\n{tail}",
        head = &s[..head_end],
        tail = &s[tail_start..],
    )
}

/// The largest character boundary at or below `i`. `str::floor_char_boundary`
/// is still unstable, and a UTF-8 codepoint is at most 4 bytes, so this walks
/// at most 3 steps.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    (0..=i)
        .rev()
        .find(|&j| s.is_char_boundary(j))
        .expect("index 0 is always a character boundary")
}

/// The smallest character boundary at or above `i`.
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    (i..=s.len())
        .find(|&j| s.is_char_boundary(j))
        .unwrap_or(s.len())
}

/// Drains a child stream line-by-line **as bytes**, not as UTF-8 text.
///
/// `AsyncBufReadExt::lines()`/`next_line()` decode each line as UTF-8 and
/// return `Err(InvalidData)` on the first byte that isn't — and the caller
/// here used to treat that identically to a clean EOF (`while let Ok(Some(..))
/// = ...`), so a single stray non-UTF-8 byte anywhere in the log silently
/// ended the drain and dropped everything after it, with no error and no
/// marker. `read_until(b'\n', ..)` has no such failure mode: it hands back
/// raw bytes regardless of their encoding, so a toolchain emitting a
/// latin-1 path or a stray control byte only ever costs that one line, not
/// the rest of the compiler output.
///
/// Each line is decoded with [`String::from_utf8`] first; only a line that
/// actually fails gets the lossy fallback (`from_utf8_lossy`, substituting
/// U+FFFD), and only then is it counted. Every replaced line is named in a
/// summary marker appended to the end of the capture, in the same
/// "arithmetic that adds back up" style [`truncate_log`]'s marker uses, so a
/// caller relying on the returned text ever being silently wrong sees that
/// it happened instead of an output that merely looks complete.
async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(reader: R) -> String {
    let mut reader = BufReader::new(reader);
    let mut out = String::new();
    let mut raw = Vec::new();
    let mut bad_lines: Vec<usize> = Vec::new();
    let mut line_no = 0usize;
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw).await {
            Ok(0) => break,
            Ok(_) => {
                line_no += 1;
                // Mirror `Lines`' own normalization: a trailing `\n` (and, for
                // a `\r\n` terminator, the `\r` ahead of it) is stripped, then
                // re-added below, uniformly, whether or not the child's last
                // line was newline-terminated at all.
                if raw.last() == Some(&b'\n') {
                    raw.pop();
                    if raw.last() == Some(&b'\r') {
                        raw.pop();
                    }
                }
                match String::from_utf8(std::mem::take(&mut raw)) {
                    Ok(line) => out.push_str(&line),
                    Err(err) => {
                        bad_lines.push(line_no);
                        out.push_str(&String::from_utf8_lossy(err.as_bytes()));
                    }
                }
                out.push('\n');
            }
            Err(_) => break,
        }
    }
    if !bad_lines.is_empty() {
        out.push_str(&format!(
            "...[{count} line(s) contained non-UTF-8 bytes and were decoded lossily \
             (invalid bytes replaced with U+FFFD): line(s) {lines}]...\n",
            count = bad_lines.len(),
            lines = bad_lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    out
}

async fn run_build_locked(plan: &BuildPlan) -> Result<BuildOutcome> {
    if !plan.cwd.exists() {
        anyhow::bail!("build working directory {} does not exist", plan.cwd.display());
    }

    let artifact_path = plan.artifact_path.clone();
    let artifact_existed_before = artifact_path.exists();
    let build_start = SystemTime::now();

    let (program, args) = plan
        .command
        .split_first()
        .context("build command must have at least one element")?;

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&plan.cwd)
        .envs(&plan.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        // Put the child in its own process group so a timeout can kill the
        // whole tree (west/cmake/make chains fork sub-processes that a plain
        // kill() on just the immediate child would orphan).
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn build command ({})", plan.lock_key))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(drain_stream(stdout));
    let stderr_task = tokio::spawn(drain_stream(stderr));

    let timeout = Duration::from_secs(plan.timeout_secs);
    let wait_result = tokio::time::timeout(timeout, child.wait()).await;

    let timed_out = wait_result.is_err();
    if timed_out {
        kill_process_tree(&mut child);
    }

    let exit_code = match wait_result {
        Ok(Ok(status)) => status.code(),
        Ok(Err(_)) | Err(_) => None,
    };

    let stdout_text = truncate_log(stdout_task.await.unwrap_or_default());
    let stderr_text = truncate_log(stderr_task.await.unwrap_or_default());

    let artifact_fresh = !timed_out
        && exit_code == Some(0)
        && artifact_path.exists()
        && artifact_is_fresh(&artifact_path, artifact_existed_before, build_start);

    // Regardless of exit code: a failed build still leaves a directory, and
    // an unattributable one is exactly what decision 19 exists to prevent.
    // Best-effort — provenance losing a build that would otherwise have
    // succeeded is the wrong trade, and the file's absence is already
    // defined as "unattributable" rather than as any positive claim.
    if let Some(manifest) = &plan.manifest {
        if let Err(e) = write_target_manifest(manifest) {
            tracing::warn!(
                "build {} produced no {TARGET_MANIFEST_NAME}: {e:#}",
                plan.lock_key
            );
        }
    }

    Ok(BuildOutcome {
        timed_out,
        exit_code,
        stdout: stdout_text,
        stderr: stderr_text,
        artifact_path,
        artifact_fresh,
    })
}

/// Writes the resolved selection to `<build_dir>/target.json`, so a human
/// (or `embarch-umbrella doctor`) staring at a directory listing can recover
/// what produced a given directory instead of reverse-engineering the
/// `-args<hash>` segment of its name (`design.md` §3 decision 19).
///
/// **It never creates the directory**, and returns `Ok(false)` when it is
/// absent: the file is evidence *about* a build directory, so writing one
/// beside a directory no build has produced would manufacture the evidence.
/// That is also why this runs after the build command rather than before it
/// — nothing this crate does has to be correct for `west build -d` to treat
/// an empty-but-existing directory as its own.
///
/// **An absent `target.json` therefore means "unattributable", never
/// "orphaned".** Every directory built before this shipped has none, and a
/// consumer that reads absence as "no live target claims this" would delete
/// exactly the directories it has no evidence about.
pub fn write_target_manifest(manifest: &TargetManifest) -> Result<bool> {
    if !manifest.dir.is_dir() {
        return Ok(false);
    }
    // Through the one serializer, like every other JSON object this crate
    // emits (`json_out`, decision 50): the file is read by another program,
    // so it gets `schema_version` for the same reason a `--json` object
    // does, rather than a second versioning story of its own.
    let mut json = crate::json_out::pretty(manifest.target.clone());
    json.push('\n');
    let path = manifest.dir.join(TARGET_MANIFEST_NAME);
    std::fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// An artifact only counts as "fresh" if it exists after a zero exit code
/// AND (when it already existed before the build) its mtime advanced past
/// the recorded build-start time. Without this, a build that fails partway
/// through — or a misconfigured artifact_path pointing at a leftover file
/// from a previous build — could silently "succeed" by flashing stale
/// firmware, which is the worst failure mode for hardware bring-up.
///
/// `pub` so `tests/build_capture.rs` can pin the rule on every platform,
/// not only where its end-to-end companion can spawn a shell.
pub fn artifact_is_fresh(path: &Path, existed_before: bool, build_start: SystemTime) -> bool {
    if !existed_before {
        return true;
    }
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.checked_add(FRESHNESS_CLOCK_GRACE) {
            Some(grace_adjusted_mtime) => grace_adjusted_mtime >= build_start,
            None => true,
        },
        Err(_) => false,
    }
}

#[cfg(unix)]
fn kill_process_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Negative pid targets the whole process group created via
        // process_group(0) above.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
    let _ = child.start_kill();
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}
