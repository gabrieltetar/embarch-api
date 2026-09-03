//! Turns a `ProjectConfig` plus an optional (board, variant, revision, app)
//! selection into a concrete `build::BuildPlan` and chip, regardless of
//! whether the project is `discovery = "static"` (today's schema, read
//! straight from config) or `discovery = "zephyr-west"` (resolved live, per
//! call — `zephyr.rs`, `design.md` §3 decision 12). Every MCP tool and CLI
//! subcommand that runs a build or talks to Core about a chip goes through
//! here, so the branch on `discovery` happens in exactly one place.

use anyhow::{Context, Result};

use crate::build::BuildPlan;
use crate::config::{Discovery, ProjectConfig};
use embarch_core_client::CoreClient;
use crate::zephyr;

/// The four optional call-time params `design.md` §3 decision 12 adds to
/// `build`/`flash`/`build_and_flash` (and, as an extension beyond the
/// decision's original text — see `design.md`'s changelog — `reset`), plus
/// `snippets`: a same-shape extension, but not one of the four narrowing
/// axes — a snippet selection doesn't narrow which (board, soc, cpucluster,
/// variant, revision, app) tuple `zephyr::select` resolves, it's an
/// independent, purely additive `-S` build flag list, validated against
/// `zephyr::available_snippets` and folded into the build command/build dir
/// name after target selection is already settled.
///
/// **Every field here is rejected, not ignored, for a `discovery =
/// "static"` project** (`design.md` §3 decision 51): a hand-authored
/// `build_command` is an opaque argv this crate did not assemble, so there
/// is nowhere to put any of them, and an input that cannot be honoured
/// fails rather than being accepted and dropped.
#[derive(Debug, Default, Clone, Copy)]
pub struct Selection<'a> {
    pub board: Option<&'a str>,
    pub variant: Option<&'a str>,
    pub revision: Option<&'a str>,
    pub app: Option<&'a str>,
    /// Empty means "use the project's configured `default_snippets`", not
    /// "build with no snippets" — see `resolve_zephyr`. To force a build
    /// with genuinely no snippets despite a configured default, there is
    /// currently no override; add one if that need actually arises.
    pub snippets: &'a [String],
    /// Extra `west build` flags (e.g. `-p always`), opaque passthrough — see
    /// `zephyr::build_command`. Same "empty means use the configured
    /// default" semantics as `snippets`, via `default_extra_args`.
    pub extra_args: &'a [String],
}

/// A fully resolved project + target, ready to build/flash. `descriptor` is
/// what a tool response echoes back so a caller can see exactly which
/// target was picked (empty selection + a `static` project just echoes the
/// project name; a resolved `zephyr-west` target echoes the full tuple).
pub struct Resolved {
    pub plan: BuildPlan,
    pub chip: String,
    pub flash_format: String,
    /// Only meaningful for `flash_format = "bin"` (`embarch-core/design.md`
    /// §3 decision 18). Comes from the project's own `base_address` config
    /// field (`design.md` §3 decision 42) for a `[[projects]]` entry, and
    /// from `dev_bench.rs`'s fixed constant for dev-bench — which was the
    /// only source of it at all until decision 42, the gap that forced the
    /// ESP32-C5 validation to flash by hand-written `POST /flash` instead of
    /// through here.
    pub base_address: Option<String>,
    /// Disambiguates which attached debug probe to flash/reset through when
    /// more than one is present (`embarch-core/design.md` §3 decision 9).
    /// `None` for every DUT project resolved here today — a real gap only
    /// dev-bench's own resolution surfaced (see `dev_bench.rs`), since
    /// exercising two probes simultaneously (a DUT's own probe alongside
    /// dev-bench's) is new as of that pipeline.
    pub probe_serial: Option<String>,
    pub descriptor: serde_json::Value,
}

pub async fn resolve(project: &ProjectConfig, selection: Selection<'_>, core: &CoreClient) -> Result<Resolved> {
    match project.discovery {
        Discovery::Static => resolve_static(project, selection),
        Discovery::ZephyrWest => resolve_zephyr(project, selection, core).await,
    }
}

/// Which of `Selection`'s fields the caller actually gave, in the order a
/// caller reads them on the CLI. A `None` option and an empty slice are both
/// "not given": for a `zephyr-west` project an empty `snippets`/`extra_args`
/// means "fall back to the configured default" (see `resolve_zephyr`), so
/// neither can mean "an explicit empty list" here either, and a call that
/// passes nothing must keep resolving exactly as it did before decision 51.
fn fields_given(selection: &Selection<'_>) -> Vec<&'static str> {
    let mut given = Vec::new();
    if selection.board.is_some() {
        given.push("board");
    }
    if selection.variant.is_some() {
        given.push("variant");
    }
    if selection.revision.is_some() {
        given.push("revision");
    }
    if selection.app.is_some() {
        given.push("app");
    }
    if !selection.snippets.is_empty() {
        given.push("snippets");
    }
    if !selection.extra_args.is_empty() {
        given.push("extra_args");
    }
    given
}

fn resolve_static(project: &ProjectConfig, selection: Selection<'_>) -> Result<Resolved> {
    // `design.md` §3 decision 51. A static project builds by running its
    // configured `build_command` verbatim, so there is no scan to narrow and
    // no `-S` to append to an argv this crate did not assemble — the only
    // honest answers were "reject" and "splice", and splicing into an opaque
    // command is not something this crate can do correctly.
    let given = fields_given(&selection);
    if !given.is_empty() {
        anyhow::bail!(
            "project '{}' is discovery = \"static\", so it builds by running its configured \
             build_command verbatim — there is no target scan to narrow and no `-S` to add to a \
             command this crate did not assemble, so {} refused rather than silently dropped. \
             Given: {}. Either re-run without {}, or put the equivalent into the project's \
             build_command in config; a Zephyr/west repo can instead set discovery = \
             \"zephyr-west\", which resolves these per call.",
            project.name,
            if given.len() == 1 {
                "it cannot be honoured and is"
            } else {
                "they cannot be honoured and are"
            },
            given.join(", "),
            if given.len() == 1 { "it" } else { "them" },
        );
    }

    Ok(Resolved {
        plan: BuildPlan {
            lock_key: project.name.clone(),
            cwd: project.build_dir(),
            command: project
                .build_command
                .clone()
                .expect("validate() enforces build_command for a static project"),
            artifact_path: project.resolved_artifact_path(),
            timeout_secs: project.build_timeout_secs,
            env: project.env.clone(),
        },
        chip: project
            .chip
            .clone()
            .expect("validate() enforces chip for a static project"),
        flash_format: project.flash_format.clone(),
        base_address: format_base_address(project.base_address),
        probe_serial: project.probe_serial.clone(),
        descriptor: serde_json::json!({ "project": project.name }),
    })
}

async fn resolve_zephyr(project: &ProjectConfig, selection: Selection<'_>, core: &CoreClient) -> Result<Resolved> {
    let targets = zephyr::scan_or_err(&project.source_path)?;

    let target = zephyr::select(
        &targets,
        selection.board,
        selection.variant,
        selection.revision,
        selection.app,
    )
    .map_err(|e| match e {
        zephyr::Selection::NoMatch => anyhow::anyhow!(
            "no target matches the given board/variant/revision/app for project '{}'. Available targets:\n{}",
            project.name,
            describe_targets(&targets)
        ),
        zephyr::Selection::Ambiguous(remaining) => anyhow::anyhow!(
            "ambiguous target for project '{}' — narrow with board/variant/revision/app. Matching targets:\n{}",
            project.name,
            describe_targets(&remaining)
        ),
    })?;

    let build_dir_root = project
        .build_dir_root
        .clone()
        .expect("validate() enforces build_dir_root for a zephyr-west project");
    let west_binary = project
        .west_binary
        .clone()
        .expect("validate() enforces west_binary for a zephyr-west project");

    // Empty selection.snippets means "use the project's configured
    // default_snippets" (e.g. a repo that always wants `-S ble-shell` for a
    // normal dev build), not "build with no snippets at all" — an explicit,
    // even if empty in effect, call-time selection isn't distinguishable
    // from "nothing was passed" through a plain slice, so the fallback
    // always applies when the caller passes none. Sorted + deduped so the
    // build dir name and assembled `-S` order are stable regardless of
    // caller-supplied order.
    let mut snippets: Vec<String> = if selection.snippets.is_empty() {
        project.default_snippets.clone()
    } else {
        selection.snippets.to_vec()
    };
    snippets.sort();
    snippets.dedup();

    let available = zephyr::available_snippets(&project.source_path, &target.app);
    let unknown: Vec<&String> = snippets.iter().filter(|s| !available.contains(s)).collect();
    if !unknown.is_empty() {
        anyhow::bail!(
            "unknown snippet(s) {unknown:?} for project '{}' app '{}'. Available snippets: {}",
            project.name,
            target.app,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    }

    // Same "empty means use the configured default" fallback as snippets,
    // but no validation — see `Selection::extra_args`. Caller-given order is
    // preserved (not sorted): unlike a snippet list, west build flag order
    // can be meaningful.
    let extra_args: Vec<String> = if selection.extra_args.is_empty() {
        project.default_extra_args.clone()
    } else {
        selection.extra_args.to_vec()
    };

    let build_dir = build_dir_root.join(target.build_dir_name(&snippets, &extra_args));
    let app_path = project.source_path.join("app").join(&target.app);
    let command = zephyr::build_command(&west_binary, &target, &snippets, &extra_args, &build_dir, &app_path);
    let artifact_path = zephyr::artifact_path(&build_dir, &project.flash_format);

    let chip = core
        .resolve_chip(&target.soc)
        .await
        .with_context(|| format!("failed to resolve a probe-rs chip for SoC '{}'", target.soc))?;

    Ok(Resolved {
        plan: BuildPlan {
            lock_key: format!("{}::{}", project.name, target.build_dir_name(&snippets, &extra_args)),
            // west build's -d/app-path args are absolute, so the actual cwd
            // doesn't affect what gets built — source_path just needs to
            // exist, which config validation already guarantees.
            cwd: project.source_path.clone(),
            command,
            artifact_path,
            timeout_secs: project.build_timeout_secs,
            env: project.env.clone(),
        },
        chip,
        flash_format: project.flash_format.clone(),
        base_address: format_base_address(project.base_address),
        probe_serial: project.probe_serial.clone(),
        descriptor: serde_json::json!({
            "project": project.name,
            "board": target.board,
            "soc": target.soc,
            "cpucluster": target.cpucluster,
            "variant": target.variant,
            "revision": target.revision,
            "app": target.app,
            "snippets": snippets,
            "extra_args": extra_args,
        }),
    })
}

/// `list_targets` (MCP tool + CLI subcommand), independent of `resolve`
/// above: this never needs Core (no chip resolution), just a live scan (for
/// `zephyr-west`) or the hand-authored menu (for `static`).
pub fn list_targets(project: &ProjectConfig) -> Result<serde_json::Value> {
    match project.discovery {
        Discovery::ZephyrWest => {
            let targets = zephyr::scan_or_err(&project.source_path)?;

            // Snippets aren't part of the (board, soc, cpucluster, variant,
            // revision, app) tuple itself (see `Selection::snippets`), so
            // they're reported separately here, keyed by app — the only
            // axis a snippet selection is actually scoped to.
            let mut apps: Vec<&str> = targets.iter().map(|t| t.app.as_str()).collect();
            apps.sort();
            apps.dedup();
            let snippets_by_app: serde_json::Map<String, serde_json::Value> = apps
                .iter()
                .map(|app| {
                    (
                        app.to_string(),
                        serde_json::json!(zephyr::available_snippets(&project.source_path, app)),
                    )
                })
                .collect();

            Ok(serde_json::json!({
                "targets": targets,
                "snippets_by_app": snippets_by_app,
                "default_snippets": project.default_snippets,
                "default_extra_args": project.default_extra_args,
            }))
        }
        Discovery::Static => {
            if project.static_targets.is_empty() {
                anyhow::bail!(
                    "project '{}' (discovery = \"static\") has no [[projects.targets]] declared. \
                     Add one per selectable target, e.g.:\n\n\
                     [[projects.targets]]\n\
                     name = \"target-a\"\n\
                     build_command = [\"make\", \"TARGET=a\"]\n\
                     chip = \"...\"\n\
                     artifact_path = \"...\"",
                    project.name
                );
            }
            let rows: Vec<_> = project
                .static_targets
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "build_command": t.build_command,
                        "chip": t.chip,
                        "artifact_path": t.artifact_path.as_ref().map(|p| p.display().to_string()),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "targets": rows }))
        }
    }
}

/// Core's `/flash` takes the offset as a hex-or-decimal *string*
/// (`embarch-core/design.md` §3 decision 18's `parse_base_address`), while
/// the config field is a TOML integer (`design.md` §3 decision 42) so a
/// value written `0x2000` reads as one. `{:#x}` is the round trip: it is the
/// form Core's own error message names, and the form a `bin` bench's
/// `[dev_bench] base_address` is written in too — which is why this is
/// `pub(crate)` rather than private: `dev_bench.rs` needs the identical
/// round trip now that its offset is config rather than a constant string.
pub(crate) fn format_base_address(configured: Option<u64>) -> Option<String> {
    configured.map(|address| format!("{address:#x}"))
}

fn describe_targets(targets: &[zephyr::Target]) -> String {
    if targets.is_empty() {
        return "  (none)".to_string();
    }
    targets
        .iter()
        .map(|t| format!("  {}", t.board_qualifier_with_app()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_project(extra: &str) -> ProjectConfig {
        // Deserialized directly rather than through `Config::load_from_path`
        // — `resolve_static` never touches the filesystem, so there's no
        // need for a real `source_path` on disk here.
        toml::from_str(&format!(
            r#"
name = "p"
source_path = "/nonexistent"
build_command = ["true"]
chip = "esp32c5"
artifact_path = "out.bin"
flash_format = "bin"
{extra}
"#
        ))
        .expect("test project config should parse")
    }

    #[test]
    fn base_address_is_formatted_as_the_hex_string_core_takes() {
        // Core's `parse_base_address` accepts hex-or-decimal; `{:#x}` is the
        // form its own error message names and the form `dev_bench.rs`'s
        // constant already uses, so a config-driven flash and a dev-bench
        // flash put the identical bytes on the wire.
        assert_eq!(format_base_address(Some(0x2000)).as_deref(), Some("0x2000"));
        assert_eq!(format_base_address(Some(0)).as_deref(), Some("0x0"));
        assert_eq!(format_base_address(None), None);
    }

    #[test]
    fn resolve_static_passes_the_configured_base_address_through() {
        let resolved =
            resolve_static(&static_project("base_address = 0x2000"), Selection::default()).unwrap();
        assert_eq!(resolved.base_address.as_deref(), Some("0x2000"));
    }

    #[test]
    fn resolve_static_leaves_base_address_unset_when_the_project_omits_it() {
        let resolved = resolve_static(&static_project(""), Selection::default()).unwrap();
        assert_eq!(resolved.base_address, None);
    }

    /// Decision 51's other half: rejecting a selection must not disturb the
    /// call that gives none. Asserted over the whole `Resolved`, not just the
    /// error path, because "unchanged" is the claim the new check makes.
    #[test]
    fn resolve_static_with_no_selection_resolves_exactly_as_before() {
        let resolved = resolve_static(&static_project(""), Selection::default()).unwrap();
        assert_eq!(resolved.plan.lock_key, "p");
        assert_eq!(resolved.plan.command, vec!["true".to_string()]);
        assert_eq!(resolved.chip, "esp32c5");
        assert_eq!(resolved.flash_format, "bin");
        assert_eq!(resolved.descriptor, serde_json::json!({ "project": "p" }));
        // An explicitly-constructed empty selection is indistinguishable from
        // a default one, which is what makes "omitted" safe to treat as
        // "nothing given" rather than as "an empty list".
        let explicit = Selection {
            board: None,
            variant: None,
            revision: None,
            app: None,
            snippets: &[],
            extra_args: &[],
        };
        assert!(resolve_static(&static_project(""), explicit).is_ok());
    }

    #[test]
    fn resolve_static_rejects_snippets_it_cannot_honour() {
        let snippets = ["ble-shell".to_string(), "cdc-acm-console".to_string()];
        let err = resolve_static(
            &static_project(""),
            Selection {
                snippets: &snippets,
                ..Selection::default()
            },
        )
        .map(|_| ())
        .expect_err("a static project cannot honour snippets and must say so");
        let message = format!("{err:#}");
        assert!(message.contains("snippets"), "{message}");
        assert!(message.contains("project 'p'"), "{message}");
        assert!(message.contains("static"), "{message}");
        // One `bail!`, so a caller's `{e:#}` render is the message itself
        // rather than a chain of contexts it has to read backwards.
        assert_eq!(message, err.to_string());
    }

    #[test]
    fn resolve_static_names_every_field_the_caller_gave() {
        let extra_args = ["-p".to_string(), "always".to_string()];
        let err = resolve_static(
            &static_project(""),
            Selection {
                board: Some("nrf54l15dk"),
                variant: Some("some-variant"),
                revision: Some("0.9.0"),
                app: Some("reference-dut"),
                snippets: &[],
                extra_args: &extra_args,
            },
        )
        .map(|_| ())
        .expect_err("every unhonourable field must be refused, not just snippets");
        let message = format!("{err:#}");
        for field in ["board", "variant", "revision", "app", "extra_args"] {
            assert!(message.contains(field), "{field} missing from: {message}");
        }
    }

    #[test]
    fn fields_given_reports_nothing_for_an_empty_selection() {
        assert!(fields_given(&Selection::default()).is_empty());
    }
}
