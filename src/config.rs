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

/// A per-project **base** target selection for a `discovery =
/// "zephyr-west"` project (`design.md` §3 decision 20). Every field is
/// optional: what is set here fills in the corresponding call-time param
/// when the call omits it, and a call that names a field wins over this one
/// for that field alone.
///
/// **Why it exists**, stated rather than left implicit: decision 12's
/// narrowing resolves a selection against a *live* scan, so a repo with
/// exactly one board resolves a bare `build` today and starts erroring
/// `Ambiguous` the moment a second board lands — the behaviour of an
/// unchanged call changing because somebody else's commit grew the repo.
/// Pinning the common case here makes that growth a non-event.
#[derive(Debug, Default, Deserialize)]
pub struct DefaultTarget {
    #[serde(default)]
    pub board: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
}

impl DefaultTarget {
    /// True when the table was written but says nothing — `validate()`
    /// refuses that rather than accepting a no-op an operator plainly meant
    /// to fill in.
    fn is_empty(&self) -> bool {
        self.board.is_none()
            && self.variant.is_none()
            && self.revision.is_none()
            && self.app.is_none()
    }
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
    /// Flash offset for this project's artifact (`design.md` §3 decision 42),
    /// passed straight through to Core's `POST /flash` — a **build-config
    /// fact of the project**, not a per-call parameter, for the same reason
    /// `flash_format` isn't one: an agent should never have to know or
    /// supply a hex number to flash a board.
    ///
    /// Required in practice for a `flash_format = "bin"` project, which has
    /// no self-describing load address of its own
    /// (`embarch-core/design.md` §3 decision 18); silently ignored by Core
    /// for a self-locating format like `hex`/`elf`, so leaving it set across
    /// a format change is harmless.
    ///
    /// Opaque pass-through — deliberately **not** validated here against the
    /// target's memory map, same posture `chip`/`flash_format` already have
    /// (§3 decision 8). Written as a TOML integer, hex literal included
    /// (`base_address = 0x2000`); `resolve.rs` formats it back to the hex
    /// string Core's endpoint takes.
    #[serde(default)]
    pub base_address: Option<u64>,
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
    /// Only meaningful for `discovery = "zephyr-west"`: the base
    /// (board, variant, revision, app) selection a call narrows from
    /// (`design.md` §3 decision 20). See `DefaultTarget`; refused outright
    /// for a `static` project, which honours no selection at all
    /// (decision 51).
    #[serde(default)]
    pub default_target: Option<DefaultTarget>,
    /// Only meaningful for `discovery = "zephyr-west"`: extra `west build`
    /// flags (e.g. `-p always` for a pristine rebuild) applied when a call
    /// omits `extra_args` entirely (`resolve::Selection`). Opaque, unlike
    /// `default_snippets` — there's no real-file list to validate arbitrary
    /// flags against, so these are passed straight through to `west build`,
    /// same posture `discovery = "static"`'s `build_command` already has for
    /// its whole argv.
    #[serde(default)]
    pub default_extra_args: Vec<String>,
    /// How to produce **this project's own firmware version string**, run in
    /// `source_path`, for `run_study --reflash dut|both`
    /// (`design.md` §3 decision 40). Defaults to
    /// `["git", "describe", "--always", "--dirty", "--abbrev=8"]` — the same
    /// invocation `embarch-dev-bench`'s own build embeds and
    /// `embarch-umbrella`'s doctor check 13 compares against, so the default
    /// is the suite's existing convention rather than a new one.
    ///
    /// **Declared, not inferred, and the distinction is the point.** There is
    /// no readback path from a DUT — this string describes the *tree that was
    /// built*, and EmbArch has no way to confirm the image actually embeds
    /// it. If a project's build stamps something else (a `VERSION` file, a
    /// CI-supplied tag), declare the command that produces that instead;
    /// EmbArch is not going to guess at it, for the same reason
    /// `embarch-study-designer/design.md` §3 decision 35 keeps firmware
    /// semantics out of anything this suite derives on its own.
    ///
    /// Only ever consulted when a run actually reflashes the DUT. A project
    /// nobody reflashes through `run_study` never needs it.
    #[serde(default)]
    pub version_command: Option<Vec<String>>,
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

/// The dev-bench build target this machine's bench is wired to —
/// deliberately still not a `[[projects]]` entry (`design.md`'s dev-bench-
/// flashing-pipeline decision): a DUT project is something a firmware
/// engineer or an agent adds/discovers per repo, and dev-bench remains
/// EmbArch's own test rig, one at a time, addressed by no project name.
///
/// **What changed 2026-08-31: which board it is stopped being a constant.**
/// `board`/`chip`/`flash_format`/`artifact_path` used to live in
/// `dev_bench.rs` as hardcoded ESP32-C5 values, on the premise that there
/// was exactly one dev-bench board this suite would ever know about. That
/// premise has now been falsified twice by the same physical bench: the
/// nRF54L15DK was the original target, the ESP32-C5 replaced it when the DK
/// broke (`embarch-decision-reversals.md` row 13), and an nRF54L15DK is the
/// bench again as of this date. The two boards disagree about every one of
/// those four facts — different west board target, different probe-rs chip,
/// `hex` vs a `bin` needing a `base_address`, and (because NCS defaults to
/// sysbuild and vanilla Zephyr doesn't) even a different artifact path
/// under `build/`.
///
/// So they are declared here, and **none of them is optional or defaulted**.
/// A default would have to pick one of the two boards, and picking wrong
/// means flashing the wrong image through the wrong debug interface at the
/// wrong chip — the exact class of silent-wrong-answer this suite refuses
/// to guess at elsewhere (`embarch-core/design.md` §7's
/// `artifact_path_for_core` retrospective). A missing field is a startup
/// error naming it, which is cheap; a wrong default is not.
#[derive(Debug, Deserialize)]
pub struct DevBenchConfig {
    /// Absolute path to the `embarch-dev-bench` workspace this bench builds
    /// from — one of that repo's `workspaces/*` (per-vendor-family, see its
    /// own `design.md` §2), matching `board` below. Not auto-derived from a
    /// sibling-repo convention at runtime: an explicit, declared fact, same
    /// posture every DUT project's own `source_path` already has,
    /// deliberately not guessed the way an earlier `artifact_path_for_core`
    /// UNC-guessing scheme was (`embarch-core/design.md` §7's retrospective
    /// on exactly that class of mistake).
    pub source_path: PathBuf,
    /// The west board target to build (e.g.
    /// `"nrf54l15dk/nrf54l15/cpuapp"`, `"esp32c5_devkitc/esp32c5/hpcore"`).
    /// Must be one `app/boards/` in `embarch-dev-bench` carries a `.conf`
    /// fragment for — the shared `app/` builds for any board, but only a
    /// board with that fragment gets the BLE/logging Kconfig this firmware
    /// actually needs.
    pub board: String,
    /// The probe-rs chip target Core attaches as (e.g. `"nRF54L15"`,
    /// `"esp32c5"`). Distinct from `board`: one names a Zephyr build target,
    /// the other names silicon to a debug probe, and neither is derivable
    /// from the other by anything this crate should be inventing.
    pub chip: String,
    /// `"hex"` or `"bin"` — what `artifact_path` below points at, and what
    /// Core's `/flash` is told to expect.
    pub flash_format: String,
    /// Where the flashable artifact lands, **relative to `source_path`**.
    /// Declared rather than derived because it genuinely varies by more
    /// than `flash_format`: NCS turns sysbuild on by default, which moves
    /// the image down a level (`build/app/zephyr/zephyr.hex`), while the
    /// vanilla-Zephyr espressif workspace has no sysbuild and leaves it at
    /// `build/zephyr/zephyr.bin`.
    pub artifact_path: PathBuf,
    /// Flash offset for the image, written as a TOML hex literal
    /// (`base_address = 0x2000`). Only meaningful for `flash_format =
    /// "bin"` (`embarch-core/design.md` §3 decision 18) — and
    /// [`Config::validate`] *requires* it there, since a `bin` written at
    /// the wrong offset is a bricked bench, not an error message.
    #[serde(default)]
    pub base_address: Option<u64>,
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
            // Same pairing rule `[[projects]]` entries live under, applied
            // here for the first time now that dev-bench's format is
            // declared rather than a constant: a raw `bin` carries no
            // addresses of its own, so an absent offset is not "flash it at
            // 0", it is "nobody said where this goes". Refuse at startup
            // rather than let Core pick.
            match dev_bench.flash_format.as_str() {
                "bin" => {
                    if dev_bench.base_address.is_none() {
                        bail!(
                            "[dev_bench] has flash_format = \"bin\" but no base_address — a raw \
                             binary has no load address in it, so the flash offset has to be \
                             declared (e.g. base_address = 0x2000)"
                        );
                    }
                }
                "hex" => {
                    if dev_bench.base_address.is_some() {
                        bail!(
                            "[dev_bench] has flash_format = \"hex\" and a base_address — a hex \
                             image carries its own addresses, so an offset here would be \
                             ignored rather than honoured; remove it"
                        );
                    }
                }
                other => bail!(
                    "[dev_bench] has flash_format = \"{other}\", which is not one of \"hex\" or \"bin\""
                ),
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
                    // Decision 51: a static project refuses every selection
                    // field on a *call*, so a default one is refused here
                    // rather than accepted and then rejected at every use —
                    // a config that cannot ever be honoured fails at load,
                    // which is the cheapest place to learn it.
                    if project.default_target.is_some() {
                        bail!(
                            "project '{}' (discovery = \"static\") sets default_target, which only \
                             a discovery = \"zephyr-west\" project can honour — a static project \
                             builds its configured build_command verbatim and refuses \
                             board/variant/revision/app outright. Remove it, or set discovery = \
                             \"zephyr-west\"",
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
                    if project.default_target.as_ref().is_some_and(|d| d.is_empty()) {
                        bail!(
                            "project '{}' has an empty [projects.default_target] — it sets none of \
                             board/variant/revision/app, so it changes nothing. Fill in the \
                             field(s) that pin this repo's common target, or remove the table",
                            project.name
                        );
                    }
                    // Decision 21's sentinel is a *call-time* override of
                    // this list; as the list itself it would mean "default
                    // to no snippets", which is what omitting the field
                    // already means. Refuse rather than leave a config that
                    // reads as meaningful and is not.
                    if project
                        .default_snippets
                        .iter()
                        .any(|s| s == crate::resolve::NO_SNIPPETS)
                    {
                        bail!(
                            "project '{}' has default_snippets containing the reserved literal \
                             \"{}\" — that literal is a call-time override meaning \"build with no \
                             snippets despite the configured default\", so it says nothing as a \
                             default. Omit default_snippets entirely for that",
                            project.name,
                            crate::resolve::NO_SNIPPETS,
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
    fn base_address_defaults_to_none_and_reads_a_toml_hex_literal() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            &format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "hex-project"
source_path = "{dir}"
build_command = ["true"]
chip = "nRF54L15"
artifact_path = "out.hex"
flash_format = "hex"

[[projects]]
name = "bin-project"
source_path = "{dir}"
build_command = ["true"]
chip = "esp32c5"
artifact_path = "out.bin"
flash_format = "bin"
base_address = 0x2000
"#,
                dir = dir.path().display()
            ),
        );
        // Absent is the default — every project predating decision 42 keeps
        // loading unchanged.
        assert_eq!(config.project("hex-project").unwrap().base_address, None);
        // TOML's own hex-integer literal, so the config reads the way an
        // offset is actually written down.
        assert_eq!(
            config.project("bin-project").unwrap().base_address,
            Some(0x2000)
        );
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

    /// Decision 20. Per-field, and a `zephyr-west` project without the table
    /// keeps loading exactly as before it existed.
    #[test]
    fn zephyr_west_project_reads_a_default_target_and_defaults_it_absent() {
        let dir = tempdir();
        let config = write_config(
            dir.path(),
            &format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[[projects]]
name = "unpinned"
source_path = "{dir}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{dir}"
flash_format = "hex"

[[projects]]
name = "pinned"
source_path = "{dir}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{dir}"
flash_format = "hex"

[projects.default_target]
board = "my_board"
app = "my-app"
"#,
                dir = dir.path().display()
            ),
        );
        assert!(config.project("unpinned").unwrap().default_target.is_none());
        let pinned = config.project("pinned").unwrap();
        let default_target = pinned
            .default_target
            .as_ref()
            .expect("the configured table should have been read");
        assert_eq!(default_target.board.as_deref(), Some("my_board"));
        assert_eq!(default_target.app.as_deref(), Some("my-app"));
        // Every axis is independently optional — a repo pinning board+app
        // while leaving revision free is the case decision 20 is for.
        assert_eq!(default_target.variant, None);
        assert_eq!(default_target.revision, None);
    }

    /// Decision 51 refuses every selection field on a *call* to a static
    /// project, so a configured default one can never be honoured — refused
    /// at load rather than at every use.
    #[test]
    fn static_project_setting_default_target_fails_validation() {
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
source_path = "{dir}"
build_command = ["true"]
chip = "nRF54L15"
artifact_path = "out.hex"
flash_format = "hex"

[projects.default_target]
board = "my_board"
"#,
                dir = dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("default_target"), "{message}");
        assert!(message.contains("static"), "{message}");
    }

    #[test]
    fn an_empty_default_target_table_fails_validation_rather_than_doing_nothing() {
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
source_path = "{dir}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{dir}"
flash_format = "hex"

[projects.default_target]
"#,
                dir = dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// Decision 21's literal is a call-time override; as a configured
    /// default it would mean what omitting the field already means.
    #[test]
    fn default_snippets_containing_the_reserved_literal_fails_validation() {
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
source_path = "{dir}"
discovery = "zephyr-west"
west_binary = "west"
build_dir_root = "{dir}"
flash_format = "hex"
default_snippets = ["none"]
"#,
                dir = dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("default_snippets"), "{message}");
        assert!(message.contains("none"), "{message}");
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
board = "nrf54l15dk/nrf54l15/cpuapp"
chip = "nRF54L15"
flash_format = "hex"
artifact_path = "build/app/zephyr/zephyr.hex"
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
board = "nrf54l15dk/nrf54l15/cpuapp"
chip = "nRF54L15"
flash_format = "hex"
artifact_path = "build/app/zephyr/zephyr.hex"
"#,
                dir.path().display()
            ),
        );
        let dev_bench = config.dev_bench.expect("dev_bench should be present");
        assert_eq!(dev_bench.build_timeout_secs, default_build_timeout_secs());
        assert_eq!(dev_bench.board, "nrf54l15dk/nrf54l15/cpuapp");
        assert_eq!(dev_bench.chip, "nRF54L15");
        assert_eq!(dev_bench.base_address, None);
    }

    /// The four fields that used to be `dev_bench.rs` constants are
    /// required, not defaulted — see `DevBenchConfig`'s own doc comment for
    /// why a plausible default is worse here than a startup error. Pinned as
    /// a test because "we deliberately did not add a default" is invisible
    /// in the type otherwise, and the obvious "helpful" follow-up edit is to
    /// add one back.
    #[test]
    fn dev_bench_board_and_chip_have_no_default() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
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
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(format!("{err:#}").contains("board"), "{err:#}");
    }

    /// A raw `bin` has no load address in it, so an absent offset is not
    /// "flash it at 0" — it is nobody having said where the image goes.
    #[test]
    fn a_bin_dev_bench_without_a_base_address_is_rejected() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[dev_bench]
source_path = "{}"
west_binary = "/usr/bin/west"
board = "esp32c5_devkitc/esp32c5/hpcore"
chip = "esp32c5"
flash_format = "bin"
artifact_path = "build/zephyr/zephyr.bin"
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(format!("{err:#}").contains("base_address"), "{err:#}");
    }

    /// The mirror image: a `hex` carries its own addresses, so an offset
    /// beside one would be silently ignored. Saying so beats honouring
    /// nothing.
    #[test]
    fn a_hex_dev_bench_with_a_base_address_is_rejected() {
        let dir = tempdir();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[core]
base_url = "auto"
token_env = "EMBARCH_TOKEN"

[dev_bench]
source_path = "{}"
west_binary = "/usr/bin/west"
board = "nrf54l15dk/nrf54l15/cpuapp"
chip = "nRF54L15"
flash_format = "hex"
artifact_path = "build/app/zephyr/zephyr.hex"
base_address = 0x2000
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        assert!(format!("{err:#}").contains("base_address"), "{err:#}");
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
