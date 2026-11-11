//! `build` answers with a verdict, keeps the whole log on disk, and builds
//! named variants in their own directories (decisions 102, 103), end to end
//! through the binary. `west` here is a shell script that records its argv,
//! writes the artifact under its `-d` directory and prints a Zephyr-shaped
//! log, so no toolchain is needed. Needs a POSIX shell, so unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The answer sizes the task set: a clean build in 2 KB, a failed one in 6.
const CLEAN_ANSWER_MAX: usize = 2 * 1024;
const FAILED_ANSWER_MAX: usize = 6 * 1024;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "embarch-api-build-verdict-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `west` that behaves like `west build` as far as `build` can see.
const FAKE_WEST: &str = r#"#!/bin/sh
dir=build
prev=""
for a in "$@"; do
  if [ "$prev" = "-d" ]; then dir="$a"; fi
  prev="$a"
done
echo "$*" >> "$WEST_ARGV_LOG"
mkdir -p "$dir/zephyr"
: > "$dir/zephyr/zephyr.hex"
# Like CMake: arguments after `--` land in the cache and stay there.
case "$*" in *" -- "*) echo "${*#* -- }" > "$dir/CMakeCache.txt";; esac
touch "$dir/CMakeCache.txt"
case "$(cat "$dir/CMakeCache.txt")" in *EXTRA_CONF_FILE=bench.conf*) used=50000;; *) used=45000;; esac
echo "[1/2] Building C object app/src/main.c.obj"
echo "$PWD/app/src/main.c:3:5: warning: unused variable 'x' [-Wunused-variable]"
echo "[2/2] Linking C executable zephyr/zephyr.elf"
echo "Memory region         Used Size  Region Size  %age Used"
echo "           FLASH:       $used B         1 MB      4.36%"
echo "             RAM:        8448 B       256 KB      3.22%"
"#;

/// A build that fails once and then prints a long cascade.
const BROKEN: &str = r#"echo "$PWD/app/src/main.c:31:46: error: expected ';' before 'return'"
echo "   31 |         printk(\"hello\")"
echo "      |                       ^"
i=0
while [ $i -lt 3000 ]; do echo "$PWD/app/src/main.c:$i:1: error: cascade line $i"; i=$((i+1)); done
echo "ninja: build stopped: subcommand failed."
exit 1"#;

fn setup(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    let west = dir.join("west");
    std::fs::write(&west, FAKE_WEST).unwrap();
    std::fs::set_permissions(&west, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("api.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[core]
base_url = "http://127.0.0.1:1"
token = "build-verdict-test-token"

[[projects]]
name = "demo"
source_path = "{src}"
build_command = ["{west}", "build", "-b", "b", "app"]
artifact_path = "build/zephyr/zephyr.hex"
chip = "nRF54L15"
flash_format = "hex"

[projects.build_variants.bench]
extra_conf_files = ["bench.conf"]

[[projects]]
name = "broken"
source_path = "{src}"
build_command = ["sh", "-c", "{broken}"]
artifact_path = "build/zephyr/zephyr.hex"
chip = "nRF54L15"
flash_format = "hex"
"#,
            src = src.display(),
            west = west.display(),
            broken = BROKEN.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"),
        ),
    )
    .unwrap();
    config
}

/// Runs the binary with this test's own temp dir, so logs and locks kept
/// under `<temp dir>` never meet another test's.
fn run(dir: &Path, config: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(config)
        .args(args)
        .env("TMPDIR", dir)
        .env("XDG_DATA_HOME", dir)
        .env("WEST_ARGV_LOG", dir.join("argv.log"))
        .output()
        .unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code().unwrap_or(-1))
}

fn json(dir: &Path, config: &Path, args: &[&str]) -> (serde_json::Value, usize) {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let (stdout, _) = run(dir, config, &all);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    (value, stdout.len())
}

#[test]
fn a_clean_build_answers_with_the_verdict_and_keeps_the_log_on_disk() {
    let t = TempDir::new();
    let config = setup(&t.0);
    let (v, size) = json(&t.0, &config, &["build", "demo"]);
    assert_eq!(v["success"], true, "{v:#}");
    assert!(v.get("stdout").is_none(), "the log is not the answer: {v:#}");
    assert_eq!(v["warnings"], 1);
    assert_eq!(v["first_warnings"][0], "app/src/main.c:3: unused variable 'x' [-Wunused-variable]");
    assert_eq!(v["memory"]["FLASH"]["used_bytes"], 45000);
    assert_eq!(v["memory"]["RAM"]["total_bytes"], 256 * 1024);
    assert!(size <= CLEAN_ANSWER_MAX, "{size} bytes: {v:#}");
    let log = std::fs::read_to_string(v["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("[1/2] Building") && log.contains("Memory region"), "{log}");

    let (verbose, _) = json(&t.0, &config, &["build", "demo", "--verbose"]);
    assert!(verbose["stdout"].as_str().unwrap().contains("Memory region"), "{verbose:#}");

    let (human, code) = run(&t.0, &config, &["build", "demo"]);
    assert_eq!(code, 0);
    assert!(human.starts_with("build ok 'demo' in "), "{human}");
    assert!(human.contains("FLASH 45000/1048576 B"), "{human}");
}

#[test]
fn a_variant_builds_in_its_own_directory_and_vs_gives_the_memory_change() {
    let t = TempDir::new();
    let config = setup(&t.0);
    let (default, _) = json(&t.0, &config, &["build", "demo"]);
    assert_eq!(default["success"], true, "{default:#}");

    let (v, _) = json(&t.0, &config, &["build", "demo", "--build-variant", "bench", "--vs", "default"]);
    assert_eq!(v["success"], true, "{v:#}");
    assert_eq!(v["target"]["build_variant"]["name"], "bench");
    assert_eq!(v["memory"]["FLASH"]["used_bytes"], 50000);
    assert_eq!(v["vs"]["build_variant"], "default");
    assert_eq!(v["vs"]["delta"]["FLASH"]["delta_bytes"], 5000, "{v:#}");
    assert_eq!(v["vs"]["delta"]["RAM"]["delta_bytes"], 0);
    let bench_dir = t.0.join("src/build-bench");
    assert_eq!(v["artifact_path"], bench_dir.join("zephyr/zephyr.hex").display().to_string());
    assert_eq!(v["log_path"], bench_dir.join("embarch-build.log").display().to_string());

    let argv = std::fs::read_to_string(t.0.join("argv.log")).unwrap();
    let last = argv.lines().last().unwrap();
    assert!(last.ends_with(&format!("-d {} -- -DEXTRA_CONF_FILE=bench.conf", bench_dir.display())), "{last}");

    // Again: the cache holds the variant's settings, so no reconfigure and
    // no wipe, and the build is still the variant's.
    let (again, _) = json(&t.0, &config, &["build", "demo", "--build-variant", "bench"]);
    assert_eq!(again["memory"]["FLASH"]["used_bytes"], 50000, "{again:#}");
    assert!(again.get("pristine").is_none());
    let argv = std::fs::read_to_string(t.0.join("argv.log")).unwrap();
    let last = argv.lines().last().unwrap();
    assert!(last.ends_with(&format!("-d {}", bench_dir.display())), "{last}");

    // Asked for, a pristine build says so and passes -p always.
    let (p, _) = json(&t.0, &config, &["build", "demo", "--build-variant", "bench", "--pristine"]);
    assert_eq!(p["pristine"], "asked");
    assert!(std::fs::read_to_string(t.0.join("argv.log")).unwrap().lines().last().unwrap().contains("-p always"));
}

#[test]
fn a_failed_build_answers_with_its_first_error_not_the_cascade() {
    let t = TempDir::new();
    let config = setup(&t.0);
    let (v, size) = json(&t.0, &config, &["build", "broken"]);
    assert_eq!(v["success"], false);
    assert_eq!(v["exit_code"], 1);
    let e = v["first_error"].as_str().unwrap();
    assert!(e.starts_with("app/src/main.c:31:46: error: expected ';'"), "{e}");
    assert!(e.contains("^"), "{e}");
    assert!(!e.contains("cascade"), "{e}");
    assert!(size <= FAILED_ANSWER_MAX, "{size} bytes");
    let log = std::fs::read_to_string(v["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("cascade line 2999"), "the whole log is kept");
}
