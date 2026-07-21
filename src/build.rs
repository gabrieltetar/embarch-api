use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::ProjectConfig;

/// Cap on captured stdout/stderr text handed back through MCP, so a runaway
/// build log doesn't blow up the tool response.
const OUTPUT_CAP_BYTES: usize = 64 * 1024;

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

    fn lock_for(&self, project: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("build locks poisoned");
        locks
            .entry(project.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub async fn run_build(&self, project: &ProjectConfig) -> Result<BuildOutcome> {
        let project_lock = self.lock_for(&project.name);
        let _guard = project_lock.lock().await;
        run_build_locked(project).await
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

async fn run_build_locked(project: &ProjectConfig) -> Result<BuildOutcome> {
    let build_dir = project.build_dir();
    if !build_dir.exists() {
        anyhow::bail!(
            "build directory {} does not exist for project '{}'",
            build_dir.display(),
            project.name
        );
    }

    let artifact_path = project.resolved_artifact_path();
    let artifact_existed_before = artifact_path.exists();
    let build_start = SystemTime::now();

    let (program, args) = project
        .build_command
        .split_first()
        .context("build_command must have at least one element")?;

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&build_dir)
        .envs(&project.env)
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
        .with_context(|| format!("failed to spawn build command for project '{}'", project.name))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(drain_stream(stdout));
    let stderr_task = tokio::spawn(drain_stream(stderr));

    let timeout = Duration::from_secs(project.build_timeout_secs);
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
        Ok(mtime) => mtime >= build_start,
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
