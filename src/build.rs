use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

/// Cap on captured stdout/stderr text handed back through MCP, so a runaway
/// build log doesn't blow up the tool response.
const OUTPUT_CAP_BYTES: usize = 64 * 1024;

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
const FRESHNESS_CLOCK_GRACE: Duration = Duration::from_millis(500);

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

fn truncate_tail(mut s: String) -> String {
    if s.len() <= OUTPUT_CAP_BYTES {
        return s;
    }
    let start = s.len() - OUTPUT_CAP_BYTES;
    // Avoid splitting in the middle of a UTF-8 codepoint.
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s.replace_range(0..start, "");
    format!("...[truncated to last {OUTPUT_CAP_BYTES} bytes]...\n{s}")
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

    let stdout_text = truncate_tail(stdout_task.await.unwrap_or_default());
    let stderr_text = truncate_tail(stderr_task.await.unwrap_or_default());

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
fn artifact_is_fresh(path: &PathBuf, existed_before: bool, build_start: SystemTime) -> bool {
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
