//! Which `embarch.toml` an EmbArch process reads: the one rule, so
//! `embarch-api`, `embarch-umbrella` and `embarch-ui` cannot each resolve it
//! differently (api decision 152). The order is an explicit path (`--config`),
//! then the `EMBARCH_API_CONFIG` value, then a walk up from a start directory
//! for `embarch/embarch.toml`. Reads only; never creates a file.

use anyhow::{bail, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The variable that names the config file.
pub const CONFIG_ENV_VAR: &str = "EMBARCH_API_CONFIG";

/// [`find_config_path_with`] reading `EMBARCH_API_CONFIG` from the process
/// environment.
pub fn find_config_path(explicit: Option<&Path>, start: &Path) -> Result<PathBuf> {
    find_config_path_with(explicit, std::env::var_os(CONFIG_ENV_VAR).as_deref(), start)
}

/// The config path: `explicit` if given, else `env_value` (an empty value
/// reads as unset), else the nearest `embarch/embarch.toml` in `start` or any
/// ancestor. An explicit path or variable that names a file which does not
/// exist is an error naming it, never a fall-through to the walk.
pub fn find_config_path_with(explicit: Option<&Path>, env_value: Option<&OsStr>, start: &Path) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return named(p, "--config");
    }
    if let Some(v) = env_value.filter(|v| !v.is_empty()) {
        return named(Path::new(&OsString::from(v)), CONFIG_ENV_VAR);
    }
    match walk_up(start) {
        Some(p) => Ok(p),
        None => bail!(
            "no config path given: pass --config <path>, set {CONFIG_ENV_VAR}, \
             or run from within (or under) a firmware repo containing embarch/embarch.toml"
        ),
    }
}

fn named(path: &Path, source: &str) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("{source} names {}, which is not a file", path.display());
    }
    Ok(path.to_path_buf())
}

fn walk_up(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join("embarch").join("embarch.toml"))
        .find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embarch-cfgdisc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo_with_config(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join("embarch")).unwrap();
        let p = root.join("embarch/embarch.toml");
        fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn explicit_wins_over_variable_and_walk() {
        let d = scratch("explicit");
        repo_with_config(&d);
        let flag = d.join("flag.toml");
        let var = d.join("var.toml");
        fs::write(&flag, "").unwrap();
        fs::write(&var, "").unwrap();
        assert_eq!(find_config_path_with(Some(&flag), Some(var.as_os_str()), &d).unwrap(), flag);
    }

    #[test]
    fn variable_wins_over_walk() {
        let d = scratch("var");
        repo_with_config(&d);
        let var = d.join("var.toml");
        fs::write(&var, "").unwrap();
        assert_eq!(find_config_path_with(None, Some(var.as_os_str()), &d).unwrap(), var);
    }

    #[test]
    fn empty_variable_reads_as_unset() {
        let d = scratch("empty");
        let found = repo_with_config(&d);
        assert_eq!(find_config_path_with(None, Some(OsStr::new("")), &d).unwrap(), found);
    }

    #[test]
    fn walk_finds_it_in_start_and_several_levels_up() {
        let d = scratch("walk");
        let found = repo_with_config(&d);
        assert_eq!(find_config_path_with(None, None, &d).unwrap(), found);
        let deep = d.join("app/widget/src");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_config_path_with(None, None, &deep).unwrap(), found);
    }

    #[test]
    fn walk_does_not_pick_up_a_siblings_config() {
        let d = scratch("sibling");
        repo_with_config(&d.join("other"));
        let mine = d.join("mine/src");
        fs::create_dir_all(&mine).unwrap();
        assert!(find_config_path_with(None, None, &mine).is_err());
    }

    #[test]
    fn no_config_anywhere_is_an_error_saying_how_to_give_one() {
        let d = scratch("none");
        let deep = d.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        let msg = find_config_path_with(None, None, &deep).unwrap_err().to_string();
        assert!(msg.contains("--config") && msg.contains(CONFIG_ENV_VAR), "{msg}");
    }

    #[test]
    fn a_missing_explicit_file_is_an_error_naming_it_not_a_walk() {
        let d = scratch("missing-flag");
        repo_with_config(&d);
        let gone = d.join("gone.toml");
        let msg = find_config_path_with(Some(&gone), None, &d).unwrap_err().to_string();
        assert!(msg.contains("gone.toml") && msg.contains("--config"), "{msg}");
    }

    #[test]
    fn a_missing_variable_file_is_an_error_naming_it_not_a_walk() {
        let d = scratch("missing-var");
        repo_with_config(&d);
        let gone = d.join("gone.toml");
        let msg = find_config_path_with(None, Some(gone.as_os_str()), &d).unwrap_err().to_string();
        assert!(msg.contains("gone.toml") && msg.contains(CONFIG_ENV_VAR), "{msg}");
    }
}
