//! `embarch-api --version` names the commit it was built from (decision 93),
//! and keeps the crate version as the first field after the name, which is
//! what `embarch doctor` reads (`embarch-umbrella` decision 57).

use std::process::Command;

#[test]
fn version_line_names_the_commit_after_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--version")
        .output()
        .expect("failed to run embarch-api --version");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout was not UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected one line, got {stdout:?}");

    let line = lines[0];
    assert_eq!(line, format!("embarch-api {}", env!("EMBARCH_BUILD_VERSION")));
    let mut fields = line.split_whitespace();
    assert_eq!(fields.next(), Some("embarch-api"));
    assert_eq!(fields.next(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(fields.next(), Some("(commit"));

    let commit = env!("EMBARCH_BUILD_COMMIT");
    let short: String = commit.chars().take(8).collect();
    assert!(line.contains(&short), "{line:?} does not name {short}");
    assert_eq!(line.ends_with(", dirty)"), env!("EMBARCH_BUILD_DIRTY") == "1", "{line:?}");
}
