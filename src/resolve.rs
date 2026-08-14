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
use crate::core_client::CoreClient;
use crate::zephyr;

/// The four optional call-time params `design.md` §3 decision 12 adds to
/// `build`/`flash`/`build_and_flash` (and, as an extension beyond the
/// decision's original text — see `design.md`'s changelog — `reset`), plus
/// `snippets`: a same-shape extension, but not one of the four narrowing
/// axes — a snippet selection doesn't narrow which (board, soc, cpucluster,
/// variant, revision, app) tuple `zephyr::select` resolves, it's an
/// independent, purely additive `-S` build flag list, validated against
/// `zephyr::available_snippets` and folded into the build command/build dir
/// name after target selection is already settled. Ignored entirely for a
/// `discovery = "static"` project (its `-S` flags, if any, are already
/// baked into its hand-authored `build_command`).
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
    pub artifact_path_for_core: Option<String>,
    pub descriptor: serde_json::Value,
}

pub async fn resolve(project: &ProjectConfig, selection: Selection<'_>, core: &CoreClient) -> Result<Resolved> {
    match project.discovery {
        Discovery::Static => resolve_static(project),
        Discovery::ZephyrWest => resolve_zephyr(project, selection, core).await,
    }
}

fn resolve_static(project: &ProjectConfig) -> Result<Resolved> {
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
        artifact_path_for_core: project.artifact_path_for_core.clone(),
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

    let artifact_path_for_core = if crate::env::under_wsl2() {
        crate::env::wsl_distro_name().map(|distro| crate::env::wsl_unc_path(&distro, &artifact_path))
    } else {
        None
    };

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
        artifact_path_for_core,
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
