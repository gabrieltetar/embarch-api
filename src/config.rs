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

#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub source_path: PathBuf,
    #[serde(default)]
    pub build_cwd: Option<PathBuf>,
    pub build_command: Vec<String>,
    pub artifact_path: PathBuf,
    pub chip: String,
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
}

impl ProjectConfig {
    /// The directory the build command should run in: `source_path` joined
    /// with `build_cwd` if set, else `source_path` itself.
    pub fn build_dir(&self) -> PathBuf {
        match &self.build_cwd {
            Some(cwd) => self.source_path.join(cwd),
            None => self.source_path.clone(),
        }
    }

    /// The artifact path resolved relative to the build directory.
    pub fn resolved_artifact_path(&self) -> PathBuf {
        self.build_dir().join(&self.artifact_path)
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
            if project.build_command.is_empty() {
                bail!("project '{}' has an empty build_command", project.name);
            }
        }

        Ok(())
    }
}
