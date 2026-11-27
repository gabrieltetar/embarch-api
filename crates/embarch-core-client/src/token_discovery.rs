use anyhow::{anyhow, bail, Context, Result};
use embarch_topology::software::TopologyClass;
use std::path::{Path, PathBuf};

/// Resolve the bearer token embarch-api sends to embarch-core, in order:
/// 1. `token_env` (an env var name) if set and present in the environment.
/// 2. `token` (inline in config) if set.
/// 3. The machine-wide token file embarch-core generates when it has no
///    explicit `EMBARCH_TOKEN` — same-OS path, or the WSL2-translated path
///    to the Windows-side file when running under WSL2.
pub fn resolve_token(explicit_token: Option<String>, explicit_token_env: Option<String>) -> Result<String> {
    resolve_token_for(TokenScope::Agent, explicit_token, explicit_token_env)
}

/// Which of Core's two tokens a client sends (`embarch-core` decision 109).
/// The agent's is what every caller has always used; the person's is for a
/// surface the engineer drives. Never chosen by a tool call: a caller picks
/// it once, when it builds its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    Agent,
    Person,
}

impl TokenScope {
    /// The environment variable Core itself reads for this scope's token.
    /// `None` for the agent: the agent's resolution has never read it
    /// implicitly, and stays byte-for-byte as it was.
    fn well_known_env(self) -> Option<&'static str> {
        match self {
            TokenScope::Agent => None,
            TokenScope::Person => Some("EMBARCH_PERSON_TOKEN"),
        }
    }

    /// The file beside the other under Core's machine-wide data directory.
    fn file_name(self) -> &'static str {
        match self {
            TokenScope::Agent => "token",
            TokenScope::Person => "person-token",
        }
    }
}

/// [`resolve_token`] for either scope. Same order for both: the named env
/// var, the inline value, then (person only) `EMBARCH_PERSON_TOKEN`, then the
/// scope's file. The two scopes share no fallback: a missing person token is
/// an error, never the agent's token.
pub fn resolve_token_for(
    scope: TokenScope,
    explicit_token: Option<String>,
    explicit_token_env: Option<String>,
) -> Result<String> {
    if let Some(var) = &explicit_token_env {
        if let Ok(value) = std::env::var(var) {
            return Ok(value);
        }
        // token_env configured but not actually present in the environment
        // does not resolve — per embarch-token.md §2, that falls through to
        // `token` and then file discovery below, rather than failing
        // immediately. A stale `token_env` left over in config (e.g. from
        // before the machine-wide token file existed) would otherwise
        // permanently block that fallback.
    }
    if let Some(token) = explicit_token {
        return Ok(token);
    }
    if let Some(var) = scope.well_known_env() {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }

    let path = discover_token_path(scope.file_name()).context("failed to determine embarch-core token file path")?;
    read_token_file(&path).map_err(|_| no_token_error(scope, &path))
}

/// Whether `EMBARCH_PERSON_TOKEN` is set and non-blank: the person's token is then
/// one the caller supplied, not a file's.
pub(crate) fn person_env_set() -> bool {
    std::env::var("EMBARCH_PERSON_TOKEN").is_ok_and(|v| !v.trim().is_empty())
}

/// Path of `file_name` in Core's machine-wide data directory (`token`,
/// `person-token`, `core-fingerprint`), the same discovery for all.
pub(crate) fn machine_file(file_name: &str) -> Result<PathBuf> {
    discover_token_path(file_name)
}

/// Path of `file_name` in Core's development store: `%LOCALAPPDATA%\embarch\dev`
/// (through WSL2 when running there), `~/.local/state/embarch/dev` on Linux.
pub(crate) fn dev_file(file_name: &str) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
        Ok(Path::new(&local).join("embarch").join("dev").join(file_name))
    }
    #[cfg(unix)]
    {
        if is_wsl2() {
            let local = embarch_topology::windows_env_var("LOCALAPPDATA")
                .context("could not determine Windows %LOCALAPPDATA% from within WSL2")?;
            Ok(embarch_topology::windows_path_to_wsl(&local)?.join("embarch").join("dev").join(file_name))
        } else {
            let home = std::env::var("HOME").context("HOME is not set")?;
            Ok(Path::new(&home).join(".local/state/embarch/dev").join(file_name))
        }
    }
}

/// The token in `path`, or `None` when the file is absent or empty.
pub(crate) fn read_optional_token(path: &Path) -> Option<String> {
    read_token_file(path).ok()
}

/// The fingerprint in `path`, lowercased; `None` when the file is absent.
pub(crate) fn read_fingerprint(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let t = raw.trim().to_ascii_lowercase();
    (!t.is_empty()).then_some(t)
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

fn no_token_error(scope: TokenScope, checked_path: &Path) -> anyhow::Error {
    if scope == TokenScope::Person {
        return anyhow!(
            "no embarch-core person token available: [core] config has neither `person_token` nor \
             `person_token_env` set, EMBARCH_PERSON_TOKEN is unset, and no token file was found at {}. \
             Start embarch-core (it generates one) or set one of those, and retry.",
            checked_path.display()
        );
    }
    anyhow!(
        "no embarch-core token available: [core] config has neither `token` nor `token_env` set, \
         and no token file was found at {}. Set [core].token or token_env in config, or start \
         embarch-core (it will generate one at {}) and retry.",
        checked_path.display(),
        checked_path.display()
    )
}

/// The path to embarch-core's generated token file, per the precedence
/// documented on `resolve_token`. The directory is `embarch-topology`'s
/// resolver (decision 52): `Local` here, `WslHost` under WSL2. An unset
/// `ProgramData` is an error naming it, surfaced as "no token".
fn discover_token_path(file_name: &str) -> Result<PathBuf> {
    let dir = embarch_topology::machine_data_dir(machine_data_class())?
        .context("the machine data directory has no local path for this topology class")?;
    Ok(dir.join(file_name))
}

/// Which machine's data directory holds Core's files: the Windows host's
/// under WSL2, this machine's own everywhere else.
fn machine_data_class() -> TopologyClass {
    #[cfg(unix)]
    if is_wsl2() {
        return TopologyClass::WslHost;
    }
    TopologyClass::Local
}

/// Detects WSL2 by delegating to `embarch_topology::software::detect_wsl2`
/// (`embarch-topology` decisions 4, 8: one implementation, called live, not
/// mirrored) — the same union-of-two-signals rule `embarch-umbrella/src/env.rs`
/// already uses, rather than this crate's own former stricter-on-one-signal
/// rule (`embarch-api` decision, see `embarch-api/decisions/core-link.md`).
/// This crate already links `embarch-topology` (`Cargo.toml`), so the call
/// was one hop away.
///
/// This used to also accept a `/proc/version` containing "wsl" without
/// "microsoft", reasoning that `$WSL_DISTRO_NAME` can be scrubbed by an MCP
/// launcher. That reasoning argues for keeping *both* signals available (a
/// union), which `detect_wsl2` already is — it does not argue for a third,
/// looser string match on the kernel release alone. No real WSL2 kernel
/// build is known to stamp "wsl" into `/proc/version` without also stamping
/// "microsoft" (every Microsoft-built WSL2 kernel release does both,
/// including this process's own), so the extra branch covered no observed
/// case; dropping it removes the one way these two calls in the same binary
/// could disagree.
#[cfg(unix)]
fn is_wsl2() -> bool {
    let proc_version = std::fs::read_to_string("/proc/version").ok();
    let wsl_distro_env = std::env::var("WSL_DISTRO_NAME").ok();
    embarch_topology::software::detect_wsl2(proc_version.as_deref(), wsl_distro_env.as_deref())
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
        let err = no_token_error(TokenScope::Agent, &path);
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()));
        assert!(msg.contains("token or token_env"));
        assert!(msg.contains("start embarch-core"));
    }

    // The env-var tests below each use their own variable name, so they do
    // not race under the parallel test runner. EMBARCH_PERSON_TOKEN is the
    // one shared name and is touched by exactly one test.

    #[test]
    fn person_resolves_from_explicit_token() {
        let r = resolve_token_for(TokenScope::Person, Some("person-inline".into()), None);
        assert_eq!(r.unwrap(), "person-inline");
    }

    #[test]
    fn person_resolves_from_named_env() {
        std::env::set_var("EMBARCH_TEST_PERSON_NAMED", "person-from-env");
        let r = resolve_token_for(
            TokenScope::Person,
            Some("person-inline".into()),
            Some("EMBARCH_TEST_PERSON_NAMED".into()),
        );
        std::env::remove_var("EMBARCH_TEST_PERSON_NAMED");
        assert_eq!(r.unwrap(), "person-from-env");
    }

    #[test]
    fn person_resolves_from_well_known_env_and_agent_does_not() {
        std::env::set_var("EMBARCH_PERSON_TOKEN", "person-well-known");
        let person = resolve_token_for(TokenScope::Person, None, None);
        let agent_inline = resolve_token_for(TokenScope::Agent, Some("agent-inline".into()), None);
        std::env::remove_var("EMBARCH_PERSON_TOKEN");
        assert_eq!(person.unwrap(), "person-well-known");
        assert_eq!(agent_inline.unwrap(), "agent-inline");
    }

    #[test]
    fn each_scope_reads_its_own_file_name() {
        assert_eq!(TokenScope::Agent.file_name(), "token");
        assert_eq!(TokenScope::Person.file_name(), "person-token");
        assert_eq!(TokenScope::Agent.well_known_env(), None);
        assert_eq!(TokenScope::Person.well_known_env(), Some("EMBARCH_PERSON_TOKEN"));
    }

    #[test]
    fn scope_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!("embarch-scope-test-{:?}", std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        for scope in [TokenScope::Agent, TokenScope::Person] {
            let path = dir.join(scope.file_name());
            std::fs::write(&path, format!("  {}-file\n", scope.file_name())).unwrap();
            assert_eq!(read_token_file(&path).unwrap(), format!("{}-file", scope.file_name()));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn person_missing_error_names_the_person_sources() {
        let msg = no_token_error(TokenScope::Person, Path::new("/x/person-token")).to_string();
        assert!(msg.contains("person_token"));
        assert!(msg.contains("EMBARCH_PERSON_TOKEN"));
        assert!(msg.contains("/x/person-token"));
    }
}
