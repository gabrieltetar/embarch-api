//! `BuildLocks` holds across processes (embarch-api decision 133): the guard
//! is an OS advisory lock on a file keyed by the lock key, so two processes
//! building one key never overlap, a killed holder frees it, and different
//! keys stay parallel. Needs a POSIX shell, so unix only.
//!
//! The "processes" are this test binary re-executed with `HELPER_*` env set,
//! running the `helper` test below as a build worker.
#![cfg(unix)]

use embarch_firmware_build::build::{BuildLocks, BuildPlan};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn plan(key: &str, cwd: &Path, script: &str) -> BuildPlan {
    BuildPlan {
        lock_key: key.to_string(),
        cwd: cwd.to_path_buf(),
        command: vec!["sh".into(), "-c".into(), script.into()],
        artifact_path: cwd.join("artifact.bin"),
        timeout_secs: 30,
        env: HashMap::new(),
        manifest: None,
    }
}

/// Not a test on its own: the worker body, run only when re-executed.
#[test]
fn helper() {
    let Ok(dir) = std::env::var("HELPER_DIR") else { return };
    let dir = PathBuf::from(dir);
    let key = std::env::var("HELPER_KEY").unwrap();
    let script = std::env::var("HELPER_SCRIPT").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let locks = BuildLocks::with_lock_dir(dir.join("locks"));
        let out = locks.run_build(&plan(&key, &dir, &script)).await.unwrap();
        // Handed back to the parent through stdout.
        println!("HELPER_STDERR_BEGIN{}HELPER_STDERR_END", out.stderr.replace('\n', "|"));
    });
}

fn spawn(dir: &Path, key: &str, script: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper", "--nocapture", "--test-threads=1"])
        .env("HELPER_DIR", dir)
        .env("HELPER_KEY", key)
        .env("HELPER_SCRIPT", script)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("embarch-fwb-lock-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Appends `start`/`end` lines with a time-ordered marker around a sleep.
fn recording(log: &Path, tag: &str, secs: &str) -> String {
    format!(
        "echo start-{tag} >> {l}; sleep {secs}; echo end-{tag} >> {l}",
        l = log.display()
    )
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap().lines().map(str::to_string).collect()
}

#[test]
fn same_key_in_two_processes_never_overlaps() {
    let dir = tmp("same");
    let log = dir.join("log.txt");
    let a = spawn(&dir, "proj::dir", &recording(&log, "a", "1"));
    let b = spawn(&dir, "proj::dir", &recording(&log, "b", "1"));
    let (oa, ob) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    assert!(oa.status.success() && ob.status.success());
    let l = lines(&log);
    assert_eq!(l.len(), 4, "{l:?}");
    // Each run's start is immediately followed by its own end.
    assert!(l[0].starts_with("start-") && l[1] == l[0].replace("start-", "end-"), "{l:?}");
    assert!(l[2].starts_with("start-") && l[3] == l[2].replace("start-", "end-"), "{l:?}");
    // The second to run was told another process held the build.
    let out = String::from_utf8_lossy(&oa.stdout).to_string() + &String::from_utf8_lossy(&ob.stdout);
    assert!(out.contains("another process's build"), "no wait note: {out}");
}

#[test]
fn different_keys_run_in_parallel() {
    let dir = tmp("diff");
    let log = dir.join("log.txt");
    let t = Instant::now();
    let a = spawn(&dir, "proj::one", &recording(&log, "a", "1.5"));
    let b = spawn(&dir, "proj::two", &recording(&log, "b", "1.5"));
    a.wait_with_output().unwrap();
    b.wait_with_output().unwrap();
    let l = lines(&log);
    // Both started before either ended.
    assert!(l[0].starts_with("start-") && l[1].starts_with("start-"), "{l:?}");
    assert!(t.elapsed() < Duration::from_millis(3200), "ran serially: {:?}", t.elapsed());
}

#[test]
fn killed_holder_does_not_block_the_next_build() {
    let dir = tmp("kill");
    let log = dir.join("log.txt");
    let mut holder = spawn(&dir, "proj::dir", &recording(&log, "holder", "20"));
    // Wait until the holder's build command is running, i.e. it has the lock.
    let t = Instant::now();
    while !log.exists() {
        assert!(t.elapsed() < Duration::from_secs(20), "holder never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    holder.kill().unwrap();
    holder.wait().unwrap();
    let t = Instant::now();
    let next = spawn(&dir, "proj::dir", &recording(&log, "next", "0"));
    let out = next.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(t.elapsed() < Duration::from_secs(10), "blocked behind a dead holder");
    assert!(lines(&log).contains(&"end-next".to_string()));
}

#[test]
fn distinct_keys_that_sanitize_alike_get_distinct_locks() {
    let d = tmp("names");
    let l = BuildLocks::with_lock_dir(d);
    assert_ne!(l.run_build_path_for_test("a::b"), l.run_build_path_for_test("a__b"));
}
