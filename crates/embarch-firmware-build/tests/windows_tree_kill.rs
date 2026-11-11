//! A timed-out build's whole process tree dies on Windows, not only the
//! immediate child (embarch-api decision 75, `tasks/api/108`).
//!
//! The build command is this test binary re-executed as `parent`, which
//! spawns a second copy as `grandchild` and waits on it; the grandchild writes
//! its PID to a file and sleeps. Both would outlive the build timeout by
//! minutes. `child.start_kill()` alone ends the parent and orphans the
//! grandchild, which is the failure this pins: the assertion is on the
//! grandchild, because the immediate child always died.
//!
//! The grandchild inherits the build's pipe handles even with its own stdio
//! nulled (Windows `CreateProcess` inherits every inheritable handle), as a
//! `west` tree's `cmake` and `ninja` do. So a surviving grandchild shows up
//! twice: `tasklist` still lists it, and `run_build` cannot return while it
//! holds the pipes. The build runs on its own thread so both are observed.
#![cfg(windows)]

use embarch_firmware_build::build::{BuildLocks, BuildPlan};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Far past [`BUILD_TIMEOUT_SECS`], so only a kill ends the grandchild early.
const HELPER_SLEEP: Duration = Duration::from_secs(180);
const BUILD_TIMEOUT_SECS: u64 = 3;
/// How long a killed process may take to leave the process table.
const GONE_WITHIN: Duration = Duration::from_secs(10);
/// How long `run_build` may take to come back once its timeout fires.
const RETURN_WITHIN: Duration = Duration::from_secs(BUILD_TIMEOUT_SECS + 15);
const ROLE_ENV: &str = "TREE_KILL_ROLE";
const PID_FILE_ENV: &str = "TREE_KILL_PID_FILE";

fn helper_command(role: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "helper", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role);
    c
}

/// Not a test on its own: the parent and grandchild bodies, run only when
/// re-executed with [`ROLE_ENV`] set.
#[test]
fn helper() {
    let Ok(role) = std::env::var(ROLE_ENV) else { return };
    match role.as_str() {
        "parent" => {
            // Waiting on the grandchild keeps the parent alive exactly as
            // long, the way `west` waits on `cmake`.
            let _ = helper_command("grandchild")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
                .wait();
        }
        "grandchild" => {
            let pid_file = std::env::var(PID_FILE_ENV).unwrap();
            std::fs::write(pid_file, std::process::id().to_string()).unwrap();
            std::thread::sleep(HELPER_SLEEP);
        }
        other => panic!("unknown {ROLE_ENV} {other}"),
    }
}

fn is_running(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

fn wait_gone(pid: u32) -> bool {
    let start = Instant::now();
    while start.elapsed() < GONE_WITHIN {
        if !is_running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("embarch-fwb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn plan(dir: &Path, pid_file: &Path) -> BuildPlan {
    let exe = std::env::current_exe().unwrap();
    let mut env = HashMap::new();
    env.insert(ROLE_ENV.to_string(), "parent".to_string());
    env.insert(PID_FILE_ENV.to_string(), pid_file.display().to_string());
    BuildPlan {
        lock_key: format!("tree-kill-{}", std::process::id()),
        cwd: dir.to_path_buf(),
        command: vec![
            exe.display().to_string(),
            "--exact".into(),
            "helper".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        artifact_path: dir.join("artifact.bin"),
        timeout_secs: BUILD_TIMEOUT_SECS,
        env,
        manifest: None,
    }
}

#[test]
fn a_timed_out_build_kills_the_grandchild_too() {
    let dir = tmp("tree-kill");
    let pid_file = dir.join("grandchild.pid");
    let build = plan(&dir, &pid_file);
    let locks = BuildLocks::with_lock_dir(dir.join("locks"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _ = tx.send(rt.block_on(locks.run_build(&build)).map(|out| out.timed_out));
    });
    let returned = rx.recv_timeout(RETURN_WITHIN);

    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the grandchild started and wrote its PID before the timeout")
        .trim()
        .parse()
        .unwrap();
    let gone = wait_gone(pid);
    if !gone {
        // Leave nothing behind for the next run, then fail.
        let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status();
    }
    assert!(gone, "grandchild PID {pid} outlived the timed-out build: tasklist still lists it");
    match returned {
        Ok(Ok(timed_out)) => assert!(timed_out, "the build should have hit its timeout"),
        Ok(Err(e)) => panic!("run_build failed: {e:#}"),
        Err(_) => panic!("run_build had not returned {RETURN_WITHIN:?} after starting"),
    }
}
