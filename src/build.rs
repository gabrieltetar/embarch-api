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
}

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

async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(reader: R) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut out = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        out.push_str(&line);
        out.push('\n');
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

    Ok(BuildOutcome {
        timed_out,
        exit_code,
        stdout: stdout_text,
        stderr: stderr_text,
        artifact_path,
        artifact_fresh,
    })
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
