//! `build --each` builds every commit from its own module sources and checks
//! a rewritten tip against a reference (decision 104), end to end through the
//! binary. `west` is a shell script: `topdir` walks up to `.west/`, and
//! `build` reads the "board" file from whichever module path the build was
//! given — `-DEXTRA_ZEPHYR_MODULES`, else the workspace's own checkout, as
//! real west would — and stamps the module's commit into `zephyr.bin`.
//! Needs a POSIX shell and git, so unix only.
#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    let d = support::unique_dir("build-each");
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

const FAKE_WEST: &str = r#"#!/bin/sh
if [ "$1" = "topdir" ]; then
  d=$PWD
  while [ "$d" != "/" ]; do
    if [ -d "$d/.west" ]; then echo "$d"; exit 0; fi
    d=$(dirname "$d")
  done
  echo "not in a west workspace" >&2; exit 1
fi
shift
dir=build; prev=""; after=""; cm=""
for a in "$@"; do
  if [ -n "$after" ]; then cm="$cm $a"; continue; fi
  if [ "$a" = "--" ]; then after=1; continue; fi
  if [ "$prev" = "-d" ]; then dir="$a"; fi
  prev="$a"
done
echo "$PWD :: $*" >> "$WEST_ARGV_LOG"
mkdir -p "$dir/zephyr"
if [ -n "$cm" ]; then echo "$cm" > "$dir/CMakeCache.txt"; fi
cache=$(cat "$dir/CMakeCache.txt" 2>/dev/null)
module=$(echo "$cache" | tr ' ' '\n' | sed -n 's/^-DEXTRA_ZEPHYR_MODULES=//p')
if [ -z "$module" ]; then module=$WORKSPACE_MODULE; fi
used=$(cat "$module/boards/flash_used")
case "$cache" in *BENCH=1*) used=$((used+100));; esac
sha=$(git -C "$module" rev-parse --short=12 HEAD)
printf 'image commit=%s end' "$sha" > "$dir/zephyr/zephyr.bin"
: > "$dir/zephyr/zephyr.hex"
echo "Memory region         Used Size  Region Size  %age Used"
echo "           FLASH:       $used B         4 KB      1.00%"
echo "             RAM:        100 B         1 KB      1.00%"
"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(repo: &Path, file: &str, body: &str, message: &str) -> String {
    let path = repo.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

struct Fixture {
    _t: TempDir,
    root: PathBuf,
    module: PathBuf,
    config: PathBuf,
    shas: [String; 3],
}

/// A workspace with one module repository: C0 the base, C1 changes only the
/// board file, C2 only the app. The working tree is then left dirty with a
/// board value no commit has, so a build reading it is caught.
fn fixture() -> Fixture {
    let t = tempdir();
    let root = t.0.clone();
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join(".west")).unwrap();
    let module = ws.join("fw");
    std::fs::create_dir_all(&module).unwrap();
    git(&module, &["init", "-q"]);
    commit(&module, "boards/flash_used", "1000", "board");
    let c0 = commit(&module, "app/main.c", "int main(void) { return 0; }\n", "app");
    let c1 = commit(&module, "boards/flash_used", "1500", "board: bigger");
    let c2 = commit(&module, "app/main.c", "int main(void) { return 1; }\n", "app: return 1");
    git(&module, &["checkout", "-q", &c0]);
    std::fs::write(module.join("boards/flash_used"), "9999").unwrap();

    let west = root.join("west");
    std::fs::write(&west, FAKE_WEST).unwrap();
    std::fs::set_permissions(&west, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.join("api.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[core]
base_url = "http://127.0.0.1:1"
token = "build-each-test-token"

[[projects]]
name = "fw"
source_path = "{module}"
build_command = ["{west}", "build", "-b", "b", "app"]
artifact_path = "build/zephyr/zephyr.hex"
chip = "nRF54L15"
flash_format = "hex"

[projects.build_variants.bench]
defines = ["BENCH=1"]
"#,
            module = module.display(),
            west = west.display(),
        ),
    )
    .unwrap();
    Fixture { _t: t, root, module, config, shas: [c0, c1, c2] }
}

fn run(f: &Fixture, args: &[&str]) -> (serde_json::Value, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&f.config)
        .arg("--json")
        .args(args)
        .env("TMPDIR", &f.root)
        .env("XDG_DATA_HOME", &f.root)
        .env("WEST_ARGV_LOG", f.root.join("argv.log"))
        .env("WORKSPACE_MODULE", &f.module)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    (serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}")), out.status.code().unwrap_or(-1))
}

fn build<'a>(v: &'a serde_json::Value, commit: usize, variant: &str) -> &'a serde_json::Value {
    v["commits"][commit]["builds"].as_array().unwrap().iter().find(|b| b["build_variant"] == variant).unwrap()
}

#[test]
fn each_commit_builds_from_its_own_module_not_the_working_tree() {
    let f = fixture();
    let range = format!("{}..{}", f.shas[0], f.shas[2]);
    let (v, code) = run(&f, &["build", "fw", "--each", &range]);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["commits"].as_array().unwrap().len(), 2);
    assert_eq!(v["commits"][0]["commit"], f.shas[1].as_str());

    // C1 changed only the board file; its build has it, not the dirty 9999.
    assert_eq!(build(&v, 0, "default")["memory"]["FLASH"]["used_bytes"], 1500, "{v:#}");
    assert_eq!(build(&v, 0, "bench")["memory"]["FLASH"]["used_bytes"], 1600, "{v:#}");
    assert_eq!(build(&v, 0, "default")["flash_headroom_bytes"], 4096 - 1500);
    assert!(build(&v, 0, "default")["delta_from_previous_commit"].is_null());
    assert_eq!(build(&v, 1, "default")["delta_from_previous_commit"]["FLASH"]["delta_bytes"], 0);

    // The engineer's tree did not move.
    assert_eq!(git(&f.module, &["rev-parse", "HEAD"]), f.shas[0]);
    assert_eq!(std::fs::read_to_string(f.module.join("boards/flash_used")).unwrap(), "9999");

    // The module override goes on the first, configuring, build of a
    // directory; the next commit's build reuses the cache.
    let argv = std::fs::read_to_string(f.root.join("argv.log")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert!(lines[0].contains("-DEXTRA_ZEPHYR_MODULES=") && lines[0].contains(".embarch-build-each"), "{argv}");
    assert!(!lines[2].contains("EXTRA_ZEPHYR_MODULES"), "{argv}");
}

#[test]
fn same_tree_as_passes_on_an_identical_tree_and_names_paths_otherwise() {
    let f = fixture();
    // The same tree as C2, on one commit straight after C0: a squashed rewrite.
    let tree = git(&f.module, &["rev-parse", &format!("{}^{{tree}}", f.shas[2])]);
    let rewritten = git(&f.module, &["commit-tree", &tree, "-p", &f.shas[0], "-m", "squashed"]);
    let range = format!("{}..{}", f.shas[0], f.shas[2]);

    let (v, code) = run(&f, &["build", "fw", "--each", &range, "--build-variant", "default", "--same-tree-as", &rewritten]);
    assert_eq!(code, 0, "{v:#}");
    let same = &v["same_tree_as"];
    assert_eq!(same["tree_equal"], true);
    assert_eq!(same["passed"], true, "{same:#}");
    assert!(same["images"][0]["result"].as_str().unwrap().contains("only in embedded commit ids"), "{same:#}");

    let (v, code) = run(&f, &["build", "fw", "--each", &range, "--build-variant", "default", "--same-tree-as", &f.shas[1]]);
    assert_eq!(code, 1);
    assert_eq!(v["same_tree_as"]["tree_equal"], false);
    assert_eq!(v["same_tree_as"]["differing_paths"], serde_json::json!(["app/main.c"]));
}
