use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn default_status_timeout_secs() -> u64 {
    10
}
fn default_reset_timeout_secs() -> u64 {
    10
}
fn default_flash_timeout_secs() -> u64 {
    120
}
fn default_serial_timeout_secs() -> u64 {
    15
}
fn default_build_timeout_secs() -> u64 {
    300
}

fn default_core_port() -> u16 {
    crate::topology::DEFAULT_CORE_PORT
}

#[derive(Debug, Deserialize)]
pub struct CoreConfig {
    /// Core's base URL, or the literal `"auto"` to resolve it at first use
    /// (design.md §3.11). `"auto"` exists because the WSL2 host-gateway
    /// address changes on every WSL restart, so any literal IP written here
    /// is guaranteed to go stale — and did, before this field accepted it.
    pub base_url: String,
    /// Only consulted by `base_url = "auto"`, as its last candidate: a Core
    /// on a genuinely separate machine.
    #[serde(default)]
    pub host: Option<String>,
    /// Only consulted by `base_url = "auto"`, when building candidates.
    #[serde(default = "default_core_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_status_timeout_secs")]
    pub status_timeout_secs: u64,
    #[serde(default = "default_reset_timeout_secs")]
    pub reset_timeout_secs: u64,
    #[serde(default = "default_flash_timeout_secs")]
    pub flash_timeout_secs: u64,
    #[serde(default = "default_serial_timeout_secs")]
    pub serial_timeout_secs: u64,
}

impl CoreConfig {
    /// Resolve the bearer token: `token_env` wins if set, then inline
    /// `token`, then the machine-wide token file embarch-core generates
    /// (see `token_discovery`) — so a secret never has to live in the
    /// config file at all, and a fresh embarch-core can be discovered with
    /// no config changes.
    pub fn resolve_token(&self) -> Result<String> {
        crate::token_discovery::resolve_token(self.token.clone(), self.token_env.clone())
    }

    /// Is Core's address to be discovered rather than declared?
    pub fn is_auto(&self) -> bool {
        self.base_url.trim().eq_ignore_ascii_case("auto")
    }
}

/// How a project's build command / chip / artifact path are determined
/// (`design.md` §3 decision 12). `Static` is the default and today's
/// fully-unchanged schema; `ZephyrWest` defers all of that to a live,
/// per-call scan (`zephyr.rs`) instead of a hand-maintained config entry.
#[derive(Debug, Default, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum Discovery {
    #[default]
    Static,
    ZephyrWest,
}

/// A hand-authored target row for a `discovery = "static"` project that
/// wants a selectable menu (`design.md` §3 decision 12's escape hatch) —
/// `list_targets` returns these verbatim. Each field overrides the
/// project-level field of the same name when this target is selected;
/// selection itself is not yet wired into `build`/`flash` (§3's own
/// `build`/`flash` rows: the four new params are ignored for a `static`
/// project), so today this only changes what `list_targets` reports.
#[derive(Debug, Deserialize)]
pub struct StaticTarget {
    pub name: String,
    #[serde(default)]
    pub build_command: Option<Vec<String>>,
    #[serde(default)]
    pub chip: Option<String>,
    #[serde(default)]
    pub artifact_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub source_path: PathBuf,
    #[serde(default)]
    pub discovery: Discovery,
    #[serde(default)]
    pub build_cwd: Option<PathBuf>,
    /// Required when `discovery = "static"` (the default); absent for
    /// `discovery = "zephyr-west"`, where it's assembled per call instead
    /// (`zephyr::build_command`).
    #[serde(default)]
    pub build_command: Option<Vec<String>>,
    /// Required when `discovery = "static"`; absent for `zephyr-west`,
    /// where it's computed per call (`zephyr::artifact_path`).
    #[serde(default)]
    pub artifact_path: Option<PathBuf>,
    /// Required when `discovery = "static"`; absent for `zephyr-west`,
    /// where it's resolved per call via Core's `POST /resolve-chip`
    /// (`embarch-core/design.md` §3 decision 8).
    #[serde(default)]
    pub chip: Option<String>,
    pub flash_format: String,
    #[serde(default = "default_build_timeout_secs")]
    pub build_timeout_secs: u64,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub serial_port: Option<String>,
    #[serde(default)]
    pub serial_baud: Option<u32>,
    #[serde(default)]
    pub artifact_path_for_core: Option<String>,
    /// Only meaningful for `discovery = "zephyr-west"`: the `west` binary to
    /// invoke (often not on bare `PATH` — see `config.example.toml`).
    #[serde(default)]
    pub west_binary: Option<PathBuf>,
    /// Only meaningful for `discovery = "zephyr-west"`: parent directory
    /// under which each distinct target gets its own build subdirectory
    /// (`embarch-umbrella/design.md` §3 decision 10's no-shared-build-dir
    /// rule), named by `zephyr::Target::build_dir_name`.
    #[serde(default)]
    pub build_dir_root: Option<PathBuf>,
    /// Only meaningful for `discovery = "static"`: a hand-authored menu
    /// `list_targets` can return verbatim (see `StaticTarget`).
    #[serde(default, rename = "targets")]
    pub static_targets: Vec<StaticTarget>,
    /// Only meaningful for `discovery = "zephyr-west"`: the `-S` snippets a
    /// build uses when a call omits `snippets` entirely (`resolve::Selection`).
    /// Exists because a repo's normal dev build often always wants the same
    /// snippet(s) (e.g. this repo's own prior static config always built
    /// with `-S ble-shell`) — without this, moving that project to
    /// `zephyr-west` would silently drop it on every call unless a caller
    /// remembered to pass `snippets` by hand every time.
    #[serde(default)]
    pub default_snippets: Vec<String>,
}

impl ProjectConfig {
    /// The directory the build command should run in: `source_path` joined
    /// with `build_cwd` if set, else `source_path` itself. Only meaningful
    /// for `discovery = "static"` — a `zephyr-west` project's build
    /// directory is per-target (`zephyr::Target::build_dir_name`), not
    /// project-wide.
    pub fn build_dir(&self) -> PathBuf {
        match &self.build_cwd {
            Some(cwd) => self.source_path.join(cwd),
            None => self.source_path.clone(),
        }
    }

    /// The artifact path resolved relative to the build directory. Only
    /// meaningful for `discovery = "static"`; panics if `artifact_path` is
    /// unset, which `validate()` already guarantees can't happen for a
    /// `static` project.
    pub fn resolved_artifact_path(&self) -> PathBuf {
        self.build_dir().join(
            self.artifact_path
                .as_ref()
                .expect("static project always has artifact_path (validate() enforces this)"),
        )
    }

    pub fn is_zephyr_west(&self) -> bool {
        self.discovery == Discovery::ZephyrWest
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub core: CoreConfig,
    #[serde(default, rename = "projects")]
    pub projects: Vec<ProjectConfig>,
}

impl Config {
    pub fn load_from_path(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file at {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file at {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn project(&self, name: &str) -> Result<&ProjectConfig> {
        self.projects
            .iter()
            .find(|p| p.name == name)
            .with_context(|| {
                let known: Vec<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
                format!("no project named '{name}' in config (known projects: {known:?})")
            })
    }

    fn validate(&self) -> Result<()> {
        self.core.resolve_token().context("invalid [core] config")?;

        let mut seen = std::collections::HashSet::new();
        for project in &self.projects {
            if !seen.insert(project.name.as_str()) {
                bail!("duplicate project name '{}' in config", project.name);
            }
            if !project.source_path.exists() {
                bail!(
                    "project '{}' has source_path {} which does not exist",
                    project.name,
                    project.source_path.display()
                );
            }

            match project.discovery {
                Discovery::Static => {
                    if project.build_command.as_ref().is_none_or(|c| c.is_empty()) {
                        bail!(
                            "project '{}' (discovery = \"static\") has no build_command",
                            project.name
                        );
                    }
                    if project.chip.is_none() {
                        bail!(
                            "project '{}' (discovery = \"static\") has no chip",
                            project.name
                        );
                    }
                    if project.artifact_path.is_none() {
                        bail!(
                            "project '{}' (discovery = \"static\") has no artifact_path",
                            project.name
                        );
                    }
                }
                Discovery::ZephyrWest => {
                    if project.west_binary.is_none() {
                        bail!(
                            "project '{}' (discovery = \"zephyr-west\") has no west_binary",
                            project.name
                        );
                    }
                    if project.build_dir_root.is_none() {
                        bail!(
                            "project '{}' (discovery = \"zephyr-west\") has no build_dir_root",
                            project.name
                        );
                    }
                    if project.build_command.is_some()
                        || project.chip.is_some()
                        || project.artifact_path.is_some()
                    {
                        bail!(
                            "project '{}' (discovery = \"zephyr-west\") must not set build_command/chip/artifact_path — these are resolved per call instead",
                            project.name
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal tempdir helper, same pattern as `zephyr.rs`'s tests — avoids
    // pulling in the `tempfile` crate for a handful of directory-existence
    // checks.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "embarch-api-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }

    fn write_config(dir: &Path, body: &str) -> Config {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        Config::load_from_path(&path).expect("config should load")
    }

    #[test]
    fn discovery_defaults_to_static_when_omitted() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            &format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "p"
source_path = "{}"
build_command = ["true"]
chip = "nRF54L15"
artifact_path = "out.hex"
flash_format = "hex"
"#,
                dir.path().display()
            ),
        );
        assert_eq!(config.projects[0].discovery, Discovery::Static);
        assert!(!config.projects[0].is_zephyr_west());
    }

    #[test]
    fn static_project_missing_chip_fails_validation() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "p"
source_path = "{}"
build_command = ["true"]
artifact_path = "out.hex"
flash_format = "hex"
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(err.to_string().contains("no chip"), "{err}");
    }

    #[test]
    fn zephyr_west_project_setting_chip_fails_validation() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "p"
source_path = "{}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{}"
chip = "nRF54L15"
flash_format = "hex"
"#,
                dir.path().display(),
                dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(err.to_string().contains("must not set"), "{err}");
    }

    #[test]
    fn zephyr_west_project_loads_cleanly_without_static_fields() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            &format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "p"
source_path = "{}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{}"
flash_format = "hex"
"#,
                dir.path().display(),
                dir.path().display()
            ),
        );
        let project = &config.projects[0];
        assert!(project.is_zephyr_west());
        assert!(project.chip.is_none());
        assert!(project.build_command.is_none());
        assert!(project.artifact_path.is_none());
    }
}
