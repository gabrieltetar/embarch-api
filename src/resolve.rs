//! Turns a `ProjectConfig` plus an optional (board, variant, revision, app)
//! selection into a concrete `build::BuildPlan` and chip, regardless of
//! whether the project is `discovery = "static"` (today's schema, read
//! straight from config) or `discovery = "zephyr-west"` (resolved live, per
//! call — `zephyr.rs`, `design.md` §3 decision 12). Every MCP tool and CLI
//! subcommand that runs a build or talks to Core about a chip goes through
//! here, so the branch on `discovery` happens in exactly one place.

use anyhow::{Context, Result};

use crate::build::BuildPlan;
use crate::config::{DefaultTarget, Discovery, ProjectConfig};
use embarch_core_client::CoreClient;
use crate::zephyr;

/// The reserved snippet literal that forces a build with **no** snippets
/// over a project's configured `default_snippets` (`design.md` §3
/// decision 21). Reserved rather than escaped because there was no third
/// state between "omit `snippets` and take the default" and "pass an
/// explicit list": an empty list is indistinguishable from an omitted one
/// through the plain slice `Selection` carries, and decision 51 depends on
/// that staying true.
///
/// **It shadows a real snippet of the same name**, and that is checked
/// rather than assumed: `resolve_zephyr` refuses `["none"]` outright for an
/// app that really declares a `none` snippet, naming the collision, instead
/// of silently building the wrong one of the two things the caller could
/// have meant.
pub const NO_SNIPPETS: &str = "none";

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
    /// with genuinely no snippets despite a configured default, pass the
    /// reserved literal `["none"]` (`NO_SNIPPETS`, decision 21); a list
    /// mixing it with real names is refused rather than guessed at.
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

/// The (board, variant, revision, app) `zephyr::select` actually narrows
/// with, once the project's configured `default_target` has filled in
/// whatever the call left out (`design.md` §3 decision 20).
///
/// **Per field, not all-or-nothing.** A call naming `board` overrides the
/// default's `board` and nothing else, which is what "a base selection a
/// call narrows further" means — and which is also why the error path below
/// has to say a default contributed: a caller who passed one field can
/// otherwise read "no target matches the given board/variant/revision/app"
/// about three values it never supplied.
#[derive(Debug, Default, PartialEq, Eq)]
struct EffectiveSelection<'a> {
    board: Option<&'a str>,
    variant: Option<&'a str>,
    revision: Option<&'a str>,
    app: Option<&'a str>,
    /// Which axes came from `default_target` rather than from the call, in
    /// the order a caller reads them. Empty when the project configures no
    /// default, or when the call named every axis the default did.
    from_default: Vec<&'static str>,
}

fn effective_selection<'a>(
    default_target: Option<&'a DefaultTarget>,
    selection: &Selection<'a>,
) -> EffectiveSelection<'a> {
    fn axis<'a>(
        call: Option<&'a str>,
        configured: Option<&'a String>,
        name: &'static str,
        from_default: &mut Vec<&'static str>,
    ) -> Option<&'a str> {
        match (call, configured) {
            (Some(given), _) => Some(given),
            (None, Some(base)) => {
                from_default.push(name);
                Some(base.as_str())
            }
            (None, None) => None,
        }
    }

    let mut from_default = Vec::new();
    EffectiveSelection {
        board: axis(
            selection.board,
            default_target.and_then(|d| d.board.as_ref()),
            "board",
            &mut from_default,
        ),
        variant: axis(
            selection.variant,
            default_target.and_then(|d| d.variant.as_ref()),
            "variant",
            &mut from_default,
        ),
        revision: axis(
            selection.revision,
            default_target.and_then(|d| d.revision.as_ref()),
            "revision",
            &mut from_default,
        ),
        app: axis(
            selection.app,
            default_target.and_then(|d| d.app.as_ref()),
            "app",
            &mut from_default,
        ),
        from_default,
    }
}

/// A sentence naming what `default_target` contributed, appended to a
/// selection error so the values in it are attributable. Empty when it
/// contributed nothing.
fn default_target_note(effective: &EffectiveSelection<'_>) -> String {
    if effective.from_default.is_empty() {
        return String::new();
    }
    format!(
        "\n(the project's configured default_target supplied {}; a call-time param overrides it \
         per field)",
        effective.from_default.join(", ")
    )
}

/// Which snippets a build actually gets, given what the call passed and what
/// the project configures (`design.md` §3 decision 21).
///
/// Three states, which is the whole point: an omitted list takes
/// `default_snippets`, an explicit list replaces it, and the reserved
/// literal `["none"]` forces zero snippets over a configured default. A list
/// mixing the literal with real names is **refused naming the ambiguity**
/// rather than resolved one way — both readings ("no snippets" and "these
/// snippets plus one called none") are things a caller could plausibly have
/// meant, and picking silently is how decision 44c's build reported success
/// having produced the wrong image.
///
/// `available` is the app's real snippet list, consulted only to catch the
/// one case the literal cannot cover: a repo that really does declare a
/// snippet named `none`.
fn resolve_snippets(
    project_name: &str,
    app: &str,
    call: &[String],
    default_snippets: &[String],
    available: &[String],
) -> Result<Vec<String>> {
    let mut snippets: Vec<String> = if call.iter().any(|s| s == NO_SNIPPETS) {
        let real: Vec<&str> = call
            .iter()
            .filter(|s| *s != NO_SNIPPETS)
            .map(String::as_str)
            .collect();
        if !real.is_empty() {
            anyhow::bail!(
                "snippets for project '{project_name}' mixes the reserved literal \"{NO_SNIPPETS}\" \
                 with real snippet name(s) {real:?}, which could mean either \"build with no \
                 snippets\" or \"build with those\" — refused rather than guessed at. Pass \
                 [\"{NO_SNIPPETS}\"] alone to force no snippets over the project's configured \
                 default_snippets, or pass just the names you want."
            );
        }
        if available.iter().any(|s| s == NO_SNIPPETS) {
            anyhow::bail!(
                "project '{project_name}' app '{app}' declares a real snippet named \
                 \"{NO_SNIPPETS}\", which collides with the reserved literal meaning \"build with \
                 no snippets\" — refused rather than guessed at. Rename that snippet, or omit \
                 `snippets` to take the project's configured default_snippets."
            );
        }
        Vec::new()
    } else if call.is_empty() {
        // An explicit, even if empty in effect, call-time selection isn't
        // distinguishable from "nothing was passed" through a plain slice
        // (decision 51 depends on that), so the fallback always applies when
        // the caller passes none — and the literal above is what gives a
        // caller the third state that costs.
        default_snippets.to_vec()
    } else {
        call.to_vec()
    };
    // Sorted + deduped so the build dir name and assembled `-S` order are
    // stable regardless of caller-supplied order.
    snippets.sort();
    snippets.dedup();

    let unknown: Vec<&String> = snippets.iter().filter(|s| !available.contains(s)).collect();
    if !unknown.is_empty() {
        anyhow::bail!(
            "unknown snippet(s) {unknown:?} for project '{project_name}' app '{app}'. Available snippets: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    }

    Ok(snippets)
}

async fn resolve_zephyr(project: &ProjectConfig, selection: Selection<'_>, core: &CoreClient) -> Result<Resolved> {
    let targets = zephyr::scan_or_err(&project.source_path)?;

    let effective = effective_selection(project.default_target.as_ref(), &selection);

    let target = zephyr::select(
        &targets,
        effective.board,
        effective.variant,
        effective.revision,
        effective.app,
    )
    .map_err(|e| match e {
        zephyr::Selection::NoMatch => anyhow::anyhow!(
            "no target matches the given board/variant/revision/app for project '{}'.{} Available targets:\n{}",
            project.name,
            default_target_note(&effective),
            describe_targets(&targets)
        ),
        zephyr::Selection::Ambiguous(remaining) => anyhow::anyhow!(
            "ambiguous target for project '{}' — narrow with board/variant/revision/app.{} Matching targets:\n{}",
            project.name,
            default_target_note(&effective),
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

    // Three states, not two: omitted takes `default_snippets`, an explicit
    // list replaces it, and the reserved `["none"]` literal forces zero
    // snippets over a configured default (decision 21). See
    // `resolve_snippets`, which also validates against the app's real list.
    let available = zephyr::available_snippets(&project.source_path, &target.app);
    let snippets = resolve_snippets(
        &project.name,
        &target.app,
        selection.snippets,
        &project.default_snippets,
        &available,
    )?;

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
/// `zephyr-west`) or the project's own single configured target (for
/// `static` — decision 53).
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

            // `default_target` is reported alongside the menu for the same
            // reason `default_snippets` is: a caller reading this list has
            // to know which of these rows a bare call already resolves to,
            // or the pinning decision 20 adds is invisible from the surface
            // that exists to answer "what can I build?".
            let default_target = project.default_target.as_ref().map(|d| {
                serde_json::json!({
                    "board": d.board,
                    "variant": d.variant,
                    "revision": d.revision,
                    "app": d.app,
                })
            });

            Ok(serde_json::json!({
                "targets": targets,
                "snippets_by_app": snippets_by_app,
                "default_snippets": project.default_snippets,
                "default_extra_args": project.default_extra_args,
                "default_target": default_target,
            }))
        }
        Discovery::Static => {
            // **A static project has exactly one target: itself**
            // (`design.md` §3 decision 53). It used to return the
            // hand-authored `[[projects.targets]]` menu, and error demanding
            // one when a project had none — a menu nothing selected from,
            // since a build runs the project-level `build_command` and
            // decision 51 refuses every selection param outright. Reporting
            // the one real target instead is what makes retiring the menu
            // lossless: this tool now answers "what can I build?" for every
            // project kind rather than erroring for half of them, and the
            // row it returns *is* the build a bare `build` runs. The name is
            // the project's own, because there is nothing else to call it
            // and nothing to pass it to.
            Ok(serde_json::json!({
                "targets": [{
                    "name": project.name,
                    "build_command": project.build_command,
                    "chip": project.chip,
                    "artifact_path": project.resolved_artifact_path().display().to_string(),
                }],
            }))
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

    /// Decision 53: `list_targets` for a `static` project reports the one
    /// target it has — itself — instead of a hand-authored menu nothing
    /// could pick from, and instead of erroring when there was none. The
    /// row has to *be* the build a bare `build` runs, or the tool that
    /// answers "what can I build?" is describing something else.
    #[test]
    fn list_targets_reports_a_static_project_as_its_own_single_target() {
        let project = static_project("");
        let value = list_targets(&project).expect("a static project always has one target");
        let targets = value["targets"].as_array().expect("targets is an array");
        assert_eq!(targets.len(), 1, "{value:#}");
        assert_eq!(targets[0]["name"], serde_json::json!("p"));
        assert_eq!(targets[0]["build_command"], serde_json::json!(["true"]));
        assert_eq!(targets[0]["chip"], serde_json::json!("esp32c5"));
        // The resolved path, not the configured relative one — the same
        // string a flash of this project would send.
        let resolved = resolve_static(&project, Selection::default()).unwrap();
        assert_eq!(
            targets[0]["artifact_path"],
            serde_json::json!(resolved.plan.artifact_path.display().to_string())
        );
        // Nothing else is advertised: no menu, and no selection axes a
        // `static` project would only refuse (decision 51).
        assert_eq!(value.as_object().unwrap().len(), 1, "{value:#}");
    }

    #[test]
    fn fields_given_reports_nothing_for_an_empty_selection() {
        assert!(fields_given(&Selection::default()).is_empty());
    }

    // ---- decision 20: `default_target` as a base selection ----

    fn default_target(board: Option<&str>, variant: Option<&str>, revision: Option<&str>, app: Option<&str>) -> DefaultTarget {
        DefaultTarget {
            board: board.map(str::to_string),
            variant: variant.map(str::to_string),
            revision: revision.map(str::to_string),
            app: app.map(str::to_string),
        }
    }

    /// The property that keeps every project predating decision 20 resolving
    /// byte-for-byte as before: no configured default means the effective
    /// selection *is* the call's.
    #[test]
    fn no_default_target_leaves_the_selection_exactly_as_the_caller_gave_it() {
        let effective = effective_selection(
            None,
            &Selection {
                board: Some("board_a"),
                app: Some("widget"),
                ..Selection::default()
            },
        );
        assert_eq!(effective.board, Some("board_a"));
        assert_eq!(effective.app, Some("widget"));
        assert_eq!(effective.variant, None);
        assert_eq!(effective.revision, None);
        assert!(effective.from_default.is_empty());
    }

    #[test]
    fn a_default_target_fills_in_only_the_axes_the_call_omitted() {
        let configured = default_target(Some("board_a"), None, Some("evt1"), Some("widget"));
        let effective = effective_selection(
            Some(&configured),
            &Selection {
                app: Some("gadget"),
                ..Selection::default()
            },
        );
        // The call wins for `app`, and only for `app`.
        assert_eq!(effective.app, Some("gadget"));
        assert_eq!(effective.board, Some("board_a"));
        assert_eq!(effective.revision, Some("evt1"));
        assert_eq!(effective.variant, None);
        assert_eq!(effective.from_default, vec!["board", "revision"]);
    }

    #[test]
    fn a_call_naming_every_configured_axis_takes_nothing_from_the_default() {
        let configured = default_target(Some("board_a"), None, None, Some("widget"));
        let effective = effective_selection(
            Some(&configured),
            &Selection {
                board: Some("board_b"),
                app: Some("gadget"),
                ..Selection::default()
            },
        );
        assert_eq!(effective.board, Some("board_b"));
        assert_eq!(effective.app, Some("gadget"));
        assert!(effective.from_default.is_empty());
    }

    /// A selection error has to be attributable, or a caller who passed one
    /// field reads a complaint about three values it never supplied — the
    /// surprise decision 20 exists to remove, reintroduced in the error text.
    #[test]
    fn a_selection_error_names_what_the_default_target_contributed() {
        let configured = default_target(Some("board_a"), None, None, Some("widget"));
        let effective = effective_selection(Some(&configured), &Selection::default());
        let note = default_target_note(&effective);
        assert!(note.contains("default_target"), "{note}");
        assert!(note.contains("board"), "{note}");
        assert!(note.contains("app"), "{note}");
        // And says nothing at all when it contributed nothing.
        assert_eq!(
            default_target_note(&effective_selection(None, &Selection::default())),
            ""
        );
    }

    // ---- decision 21: the `["none"]` snippet sentinel ----

    fn available() -> Vec<String> {
        vec!["ble-shell".to_string(), "wdt31".to_string()]
    }

    #[test]
    fn omitted_snippets_still_take_the_configured_default() {
        let configured = vec!["ble-shell".to_string()];
        let resolved = resolve_snippets("p", "widget", &[], &configured, &available()).unwrap();
        assert_eq!(resolved, vec!["ble-shell".to_string()]);
    }

    #[test]
    fn the_reserved_literal_forces_no_snippets_over_a_configured_default() {
        let configured = vec!["ble-shell".to_string()];
        let resolved = resolve_snippets(
            "p",
            "widget",
            &[NO_SNIPPETS.to_string()],
            &configured,
            &available(),
        )
        .unwrap();
        assert!(
            resolved.is_empty(),
            "[\"none\"] must override the default, not be looked up as a snippet: {resolved:?}"
        );
    }

    /// The literal is not itself a snippet name, so it must not be validated
    /// against the app's real list — a repo with no snippets at all can still
    /// use it, and does not get "unknown snippet(s)".
    #[test]
    fn the_reserved_literal_works_for_an_app_that_declares_no_snippets() {
        let resolved =
            resolve_snippets("p", "widget", &[NO_SNIPPETS.to_string()], &[], &[]).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn mixing_the_reserved_literal_with_real_names_is_refused_naming_both_readings() {
        let call = vec![NO_SNIPPETS.to_string(), "ble-shell".to_string()];
        let err = resolve_snippets("p", "widget", &call, &[], &available())
            .map(|_| ())
            .expect_err("a mixed list is ambiguous and must not be resolved one way");
        let message = format!("{err:#}");
        assert!(message.contains("ble-shell"), "{message}");
        assert!(message.contains("reserved"), "{message}");
        // One `bail!`, so a caller's `{e:#}` render is the message itself.
        assert_eq!(message, err.to_string());
    }

    /// The one case the reserved literal genuinely cannot cover, checked
    /// rather than assumed away: a repo that really declares a `none`
    /// snippet. Refused naming the collision instead of silently picking one
    /// of the two things the caller could have meant.
    #[test]
    fn a_real_snippet_named_none_collides_with_the_literal_and_is_refused() {
        let real = vec!["none".to_string(), "ble-shell".to_string()];
        let err = resolve_snippets("p", "widget", &[NO_SNIPPETS.to_string()], &[], &real)
            .map(|_| ())
            .expect_err("a real snippet named none makes the literal ambiguous");
        let message = format!("{err:#}");
        assert!(message.contains("collides"), "{message}");
        assert!(message.contains("widget"), "{message}");
    }

    #[test]
    fn an_explicit_list_still_replaces_the_default_and_is_sorted_and_deduped() {
        let configured = vec!["ble-shell".to_string()];
        let call = vec![
            "wdt31".to_string(),
            "ble-shell".to_string(),
            "wdt31".to_string(),
        ];
        let resolved = resolve_snippets("p", "widget", &call, &configured, &available()).unwrap();
        assert_eq!(resolved, vec!["ble-shell".to_string(), "wdt31".to_string()]);
    }

    #[test]
    fn an_unknown_snippet_is_still_rejected_against_the_apps_real_list() {
        let call = vec!["nope".to_string()];
        let err = resolve_snippets("p", "widget", &call, &[], &available())
            .map(|_| ())
            .expect_err("an unknown snippet must still fail");
        let message = format!("{err:#}");
        assert!(message.contains("unknown snippet"), "{message}");
        assert!(message.contains("ble-shell"), "{message}");
    }
}
