use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// `CoreConfig` (base_url/host/port/token/token_env/*_timeout_secs) and its
// `resolve_token`/`is_auto` moved into the shared `embarch-core-client`
// crate 2026-08-24 — embarch-ui/design.md §3 decision 5's resolution,
// embarch-ui/milestone-1.md §4.1 — so embarch-api and embarch-ui depend on
// one implementation of "how do I reach embarch-core" rather than each
// carrying their own. Re-exported here so `crate::config::CoreConfig`
// keeps working unchanged for every existing caller in this repo.
pub use embarch_core_client::CoreConfig;

fn default_build_timeout_secs() -> u64 {
    300
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
    /// Disambiguates this project's own debug probe when more than one is
    /// attached (`embarch-core/design.md` §3 decision 9) — matched against
    /// `ProbeInfo.serial_number` (`status`'s own `probes` list). Documented
    /// in this file's own §4 table since that decision was written, but
    /// never actually wired up here until dev-bench's own build/flash
    /// pipeline made a second real probe common enough to need it for real.
    #[serde(default)]
    pub probe_serial: Option<String>,
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
    /// Only meaningful for `discovery = "zephyr-west"`: extra `west build`
    /// flags (e.g. `-p always` for a pristine rebuild) applied when a call
    /// omits `extra_args` entirely (`resolve::Selection`). Opaque, unlike
    /// `default_snippets` — there's no real-file list to validate arbitrary
    /// flags against, so these are passed straight through to `west build`,
    /// same posture `discovery = "static"`'s `build_command` already has for
    /// its whole argv.
    #[serde(default)]
    pub default_extra_args: Vec<String>,
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

/// The one dev-bench build target this suite knows about — deliberately not
/// a `[[projects]]` entry (`design.md`'s dev-bench-flashing-pipeline
/// decision): a DUT project is something a firmware engineer or an agent
/// adds/discovers per repo, but dev-bench is EmbArch's own fixed test rig —
/// there's exactly one, and its board/chip/flash format/flash base address
/// are facts this suite already knows (`dev_bench.rs`'s own constants), not
/// per-project knobs. Only `source_path`/`west_binary` are genuinely
/// machine-specific (where the sibling repo is checked out, and where
/// `west` actually lives, since it's often not on bare `PATH` — see
/// `config.example.toml`), so those are the only fields this table declares.
#[derive(Debug, Deserialize)]
pub struct DevBenchConfig {
    /// Absolute path to the `embarch-dev-bench` workspace this suite builds
    /// today (`workspaces/espressif` — `dev_bench.rs`'s own `BOARD` constant
    /// names the exact board). Not auto-derived from a sibling-repo
    /// convention at runtime: an explicit, declared fact, same posture every
    /// DUT project's own `source_path` already has, deliberately not
    /// guessed the way an earlier `artifact_path_for_core` UNC-guessing
    /// scheme was (`embarch-core/design.md` §7's retrospective on exactly
    /// that class of mistake).
    pub source_path: PathBuf,
    /// The `west` binary to invoke — often not on bare `PATH` (see
    /// `config.example.toml`), same reasoning as a `discovery = "zephyr-west"`
    /// project's own `west_binary` field.
    pub west_binary: PathBuf,
    #[serde(default = "default_build_timeout_secs")]
    pub build_timeout_secs: u64,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Disambiguates dev-bench's own debug probe from a DUT's, whenever both
    /// are attached at once (`embarch-core/design.md` §3 decision 9) — a
    /// real, not hypothetical, need the moment a DUT probe is also plugged
    /// in: Core's default `open_first_probe()` isn't guaranteed to pick the
    /// right one, and picking wrong fails outright (wrong debug interface
    /// for the wrong chip), not silently. Find it via embarch-api's own
    /// `status` tool/CLI subcommand's `probes` list.
    #[serde(default)]
    pub probe_serial: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub core: CoreConfig,
    #[serde(default, rename = "projects")]
    pub projects: Vec<ProjectConfig>,
    /// Absent means dev-bench build/flash tools are unavailable — a clear
    /// "not configured" error rather than a silent no-op or a guessed path.
    #[serde(default)]
    pub dev_bench: Option<DevBenchConfig>,
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

        if let Some(dev_bench) = &self.dev_bench {
            if !dev_bench.source_path.exists() {
                bail!(
                    "[dev_bench] has source_path {} which does not exist",
                    dev_bench.source_path.display()
                );
            }
        }

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
    fn dev_bench_absent_by_default() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"
"#,
        );
        assert!(config.dev_bench.is_none());
    }

    #[test]
    fn dev_bench_missing_source_path_fails_validation() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[dev_bench]
source_path = "/definitely/does/not/exist/anywhere"
west_binary = "/usr/bin/west"
"#,
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(err.to_string().contains("[dev_bench]"), "{err}");
    }

    #[test]
    fn dev_bench_loads_when_source_path_exists() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            &format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[dev_bench]
source_path = "{}"
west_binary = "/usr/bin/west"
"#,
                dir.path().display()
            ),
        );
        let dev_bench = config.dev_bench.expect("dev_bench should be present");
        assert_eq!(dev_bench.build_timeout_secs, default_build_timeout_secs());
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
