use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Resolve the bearer token embarch-api sends to embarch-core, in order:
/// 1. `token_env` (an env var name) if set and present in the environment.
/// 2. `token` (inline in config) if set.
/// 3. The machine-wide token file embarch-core generates when it has no
///    explicit `EMBARCH_TOKEN` — same-OS path, or the WSL2-translated path
///    to the Windows-side file when running under WSL2.
pub fn resolve_token(explicit_token: Option<String>, explicit_token_env: Option<String>) -> Result<String> {
    if let Some(var) = &explicit_token_env {
        if let Ok(value) = std::env::var(var) {
            return Ok(value);
        }
        // token_env configured but not actually present in the environment
        // does not resolve — per embarch-token.md §2 / milestone-2.md §3.1,
        // that falls through to `token` and then file discovery below,
        // rather than failing immediately. A stale `token_env` left over in
        // config (e.g. from before the machine-wide token file existed)
        // would otherwise permanently block the fallback this milestone
        // exists to provide.
    }
    if let Some(token) = explicit_token {
        return Ok(token);
    }

    let path = discover_token_path().context("failed to determine embarch-core token file path")?;
    read_token_file(&path).map_err(|_| no_token_error(&path))
}

fn read_token_file(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read token file at {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("token file at {} is empty", path.display());
    }
    Ok(trimmed.to_string())
}

fn no_token_error(checked_path: &Path) -> anyhow::Error {
    anyhow!(
        "no embarch-core token available: [core] config has neither `token` nor `token_env` set, \
         and no token file was found at {}. Set [core].token or token_env in config, or start \
         embarch-core (it will generate one at {}) and retry.",
        checked_path.display(),
        checked_path.display()
    )
}

/// The same-OS or WSL2-translated path to embarch-core's generated token
/// file, per the precedence documented on `resolve_token`.
fn discover_token_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(windows_token_path())
    }
    #[cfg(unix)]
    {
        if is_wsl2() {
            wsl2_token_path().map_err(|msg| anyhow!(msg))
        } else {
            Ok(PathBuf::from("/var/lib/embarch/token"))
        }
    }
}

#[cfg(windows)]
fn windows_token_path() -> PathBuf {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    Path::new(&program_data).join("embarch").join("token")
}

/// Detects WSL2 by reading `/proc/version`, since `$WSL_DISTRO_NAME` can be
/// stripped depending on how the process was spawned (e.g. some MCP client
/// launchers scrub the environment). Case-insensitive match on "microsoft"
/// or "wsl", matching what Microsoft's own WSL2 kernel build stamps there.
#[cfg(unix)]
fn is_wsl2() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|version| {
            let lower = version.to_lowercase();
            lower.contains("microsoft") || lower.contains("wsl")
        })
        .unwrap_or(false)
}

/// The WSL2-translated path to `%ProgramData%\embarch\token`, computed once
/// per process and cached — the shell-out to resolve `%ProgramData%` should
/// not run on every token resolution.
#[cfg(unix)]
fn wsl2_token_path() -> Result<PathBuf, String> {
    static CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    CACHE
        .get_or_init(|| compute_wsl2_token_path().map_err(|e| format!("{e:#}")))
        .clone()
}

#[cfg(unix)]
fn compute_wsl2_token_path() -> Result<PathBuf> {
    let program_data = windows_program_data_via_shellout()
        .context("could not determine Windows %ProgramData% from within WSL2")?;
    let mnt_root = translate_windows_path_to_wsl(&program_data)
        .with_context(|| format!("could not translate Windows path '{program_data}' to its WSL2 mount form"))?;
    Ok(mnt_root.join("embarch").join("token"))
}

#[cfg(unix)]
fn windows_program_data_via_shellout() -> Result<String> {
    if let Some(v) = run_shellout("cmd.exe", &["/C", "echo", "%ProgramData%"]) {
        if !v.is_empty() && !v.contains('%') {
            return Ok(v);
        }
    }
    if let Some(v) = run_shellout("powershell.exe", &["-NoProfile", "-Command", "$env:ProgramData"]) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    bail!("neither cmd.exe nor powershell.exe returned a usable %ProgramData% value")
}

#[cfg(unix)]
fn run_shellout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_string())
}

/// Translates a Windows-side path (e.g. `C:\ProgramData`) to its WSL2 mount
/// form (e.g. `/mnt/c/ProgramData`). Prefers the `wslpath` utility, since it
/// correctly reflects however this WSL2 instance actually has drives
/// mounted; falls back to hand-parsing the drive letter only if `wslpath`
/// isn't available or doesn't behave as expected.
#[cfg(unix)]
fn translate_windows_path_to_wsl(win_path: &str) -> Result<PathBuf> {
    if let Some(translated) = run_shellout("wslpath", &["-u", win_path]) {
        if translated.starts_with('/') {
            return Ok(PathBuf::from(translated));
        }
    }

    hand_parse_windows_path(win_path)
}

#[cfg(unix)]
fn hand_parse_windows_path(win_path: &str) -> Result<PathBuf> {
    let mut chars = win_path.chars();
    let drive = chars
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .with_context(|| format!("'{win_path}' doesn't start with a drive letter"))?;
    let rest = chars.as_str();
    let rest = rest.strip_prefix(':').with_context(|| format!("'{win_path}' is missing the ':' after the drive letter"))?;
    let rest = rest.replace('\\', "/");
    Ok(PathBuf::from(format!(
        "/mnt/{}{}",
        drive.to_ascii_lowercase(),
        rest
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_token_env_wins() {
        std::env::set_var("EMBARCH_TEST_TOKEN_XYZ", "from-env");
        let result = resolve_token(Some("inline".to_string()), Some("EMBARCH_TEST_TOKEN_XYZ".to_string()));
        assert_eq!(result.unwrap(), "from-env");
        std::env::remove_var("EMBARCH_TEST_TOKEN_XYZ");
    }

    #[test]
    fn inline_token_used_when_no_env_set() {
        let result = resolve_token(Some("inline-token".to_string()), None);
        assert_eq!(result.unwrap(), "inline-token");
    }

    #[test]
    fn falls_through_to_inline_token_when_token_env_not_present() {
        // token_env names a variable that isn't actually set in the
        // environment — this must fall through to the inline token, not
        // fail immediately (the bug this test guards against).
        std::env::remove_var("EMBARCH_TEST_TOKEN_ABSENT");
        let result = resolve_token(
            Some("inline-token".to_string()),
            Some("EMBARCH_TEST_TOKEN_ABSENT".to_string()),
        );
        assert_eq!(result.unwrap(), "inline-token");
    }

    #[test]
    fn file_read_trims_whitespace() {
        let dir = std::env::temp_dir().join(format!("embarch-token-test-{:?}", std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "  file-token\n").unwrap();
        assert_eq!(read_token_file(&path).unwrap(), "file-token");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_produces_actionable_error() {
        let path = PathBuf::from("/nonexistent/embarch/token/for/test");
        let err = no_token_error(&path);
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()));
        assert!(msg.contains("token or token_env"));
        assert!(msg.contains("start embarch-core"));
    }

    #[cfg(unix)]
    #[test]
    fn hand_parse_translates_drive_letter() {
        let translated = hand_parse_windows_path(r"C:\ProgramData").unwrap();
        assert_eq!(translated, PathBuf::from("/mnt/c/ProgramData"));
    }

    #[cfg(unix)]
    #[test]
    fn hand_parse_lowercases_drive_letter() {
        let translated = hand_parse_windows_path(r"D:\Foo\Bar").unwrap();
        assert_eq!(translated, PathBuf::from("/mnt/d/Foo/Bar"));
    }
}
