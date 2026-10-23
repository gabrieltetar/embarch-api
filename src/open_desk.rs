//! The open desk as `embarch-api`'s default project
//! (`embarch-open-desk-proposal.md`).
//!
//! A tool call or CLI command that names no project — or names the literal
//! `desk` — builds the firmware repo of the desk the whole suite has open.
//! embarch-desk publishes that desk as `open-desk.json` beside its state; this
//! reads the file rather than the desk's HTTP surface, so a stopped desk still
//! answers with the last desk opened.

use anyhow::{bail, Context, Result};
use crate::config::Config;
use std::path::{Path, PathBuf};

/// The word a project argument uses to mean "the open desk's".
pub const DESK: &str = "desk";

fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("EMBARCH_DESK_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local").join("share"),
    };
    Ok(base.join("embarch-desk"))
}

/// `(desk name, firmware repos)` of the open desk, or why there is none.
fn open_desk() -> Result<(String, Vec<PathBuf>)> {
    let path = data_dir()?.join("open-desk.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no open desk published at {} — is embarch-desk installed?", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let desk = &v["desk"];
    if desk.is_null() {
        bail!("no desk is open — name a project, or open a desk");
    }
    let repos = desk["firmware_repos"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x.as_str()).map(PathBuf::from).collect())
        .unwrap_or_default();
    Ok((desk["name"].as_str().unwrap_or("?").to_string(), repos))
}

fn same_tree(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

/// The configured project a name means: itself, or for an empty name or
/// `desk`, the project whose `source_path` is the open desk's firmware repo.
pub fn resolve(config: &Config, name: &str) -> Result<String> {
    let name = name.trim();
    if !name.is_empty() && name != DESK {
        return Ok(name.to_string());
    }
    let (desk, repos) = open_desk()?;
    resolve_in(config, &desk, &repos)
}

fn resolve_in(config: &Config, desk: &str, repos: &[PathBuf]) -> Result<String> {
    if repos.is_empty() {
        bail!("the open desk {desk:?} names no firmware repo");
    }
    for repo in repos {
        if let Some(p) = config.projects.iter().find(|p| same_tree(&p.source_path, repo)) {
            return Ok(p.name.clone());
        }
    }
    let known: Vec<String> = config.projects.iter().map(|p| format!("{} ({})", p.name, p.source_path.display())).collect();
    bail!(
        "the open desk {desk:?}'s firmware repo {} is not a project in embarch-api's config; configured: {}",
        repos[0].display(),
        known.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_project_is_never_redirected() {
        let config: Config = toml::from_str("[core]\nbase_url = \"http://127.0.0.1:1\"\ntoken = \"x\"\n").unwrap_or_else(|_| panic!("minimal config"));
        assert_eq!(resolve(&config, "fw").unwrap(), "fw");
    }

    #[test]
    fn the_desks_repo_finds_its_project_and_an_unknown_repo_says_so() {
        let dir = std::env::temp_dir().join(format!("api-desk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let toml_text = format!(
            "[core]\nbase_url = \"http://127.0.0.1:1\"\ntoken = \"x\"\n\n[[projects]]\nname = \"fw\"\nsource_path = \"{}\"\nchip = \"x\"\nflash_format = \"hex\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\n",
            dir.display()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(resolve_in(&config, "D", std::slice::from_ref(&dir)).unwrap(), "fw");
        assert_eq!(resolve_in(&config, "D", &[dir.join("app")]).unwrap(), "fw");
        let err = resolve_in(&config, "D", &[PathBuf::from("/nowhere/else")]).unwrap_err().to_string();
        assert!(err.contains("not a project") && err.contains("fw"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
