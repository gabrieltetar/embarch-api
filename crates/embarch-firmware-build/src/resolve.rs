//! Turns a `ProjectConfig` plus an optional (board, variant, revision, app)
//! selection into a concrete `build::BuildPlan` and chip, regardless of
//! whether the project is `discovery = "static"` (today's schema, read
//! straight from config) or `discovery = "zephyr-west"` (resolved live, per
//! call — `zephyr.rs`, decision 12). Every MCP tool and CLI
//! subcommand that runs a build or talks to Core about a chip goes through
//! here, so the branch on `discovery` happens in exactly one place.

use anyhow::{Context, Result};

use crate::build::{BuildPlan, TargetManifest, TARGET_MANIFEST_NAME};
use crate::config::{BuildVariant, DefaultTarget, Discovery, ProjectConfig, DEFAULT_BUILD_VARIANT};
use embarch_core_client::CoreClient;
use crate::zephyr;

/// The reserved snippet literal that forces a build with **no** snippets
/// over a project's configured `default_snippets` (decision
/// 21). Reserved rather than escaped because there was no third
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

/// The four optional call-time params decision 12 adds to
/// `build`/`flash`/`build_and_flash` (and, as an extension beyond the
/// decision's original text — see decision 12's changelog — `reset`), plus
/// `snippets`: a same-shape extension, but not one of the four narrowing
/// axes — a snippet selection doesn't narrow which (board, soc, cpucluster,
/// variant, revision, app) tuple `zephyr::select` resolves, it's an
/// independent, purely additive `-S` build flag list, validated against
/// `zephyr::available_snippets` and folded into the build command/build dir
/// name after target selection is already settled.
///
/// **Every field here is rejected, not ignored, for a `discovery =
/// "static"` project** (decision 51): a hand-authored
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
    /// Only meaningful for `flash_format = "bin"` (`embarch-core`
    /// decision 18). Comes from the project's own `base_address` config
    /// field (decision 42) for a `[[projects]]` entry, and
    /// from `dev_bench.rs`'s fixed constant for dev-bench — which was the
    /// only source of it at all until decision 42, the gap that forced the
    /// ESP32-C5 validation to flash by hand-written `POST /flash` instead of
    /// through here.
    pub base_address: Option<String>,
    /// Disambiguates which attached debug probe to flash/reset through when
    /// more than one is present (`embarch-core` decision 9).
    /// `None` for every DUT project resolved here today — a real gap only
    /// dev-bench's own resolution surfaced (see `dev_bench.rs`), since
    /// exercising two probes simultaneously (a DUT's own probe alongside
    /// dev-bench's) is new as of that pipeline.
    pub probe_serial: Option<String>,
    pub descriptor: serde_json::Value,
    /// Why this build starts from an empty directory (`-p always`), when it
    /// does: the caller asked, or a build variant's directory last built
    /// other settings (decision 103). `None` is an incremental build.
    pub pristine: Option<String>,
}

/// Which named build variant to build, and whether to start it pristine
/// (decision 103). Separate from [`Selection`], which narrows *which
/// target* and is built by callers outside this crate field by field.
#[derive(Debug, Default, Clone, Copy)]
pub struct BuildChoice<'a> {
    /// A `[projects.build_variants.<name>]` key; `None` or
    /// [`DEFAULT_BUILD_VARIANT`] is the build with no variant.
    pub build_variant: Option<&'a str>,
    pub pristine: bool,
}

pub async fn resolve(project: &ProjectConfig, selection: Selection<'_>, core: &CoreClient) -> Result<Resolved> {
    resolve_with(project, selection, BuildChoice::default(), core).await
}

/// [`resolve`], building a named variant or starting pristine.
pub async fn resolve_with(
    project: &ProjectConfig,
    selection: Selection<'_>,
    choice: BuildChoice<'_>,
    core: &CoreClient,
) -> Result<Resolved> {
    match project.discovery {
        Discovery::Static => resolve_static(project, selection, choice),
        Discovery::ZephyrWest => {
            let (build, soc) = resolve_zephyr_build(project, selection, choice)?;
            let chip = core
                .resolve_chip(&soc)
                .await
                .with_context(|| format!("failed to resolve a probe-rs chip for SoC '{soc}'"))?;
            Ok(Resolved {
                plan: build.plan,
                chip,
                flash_format: project.flash_format.clone(),
                base_address: format_base_address(project.base_address),
                probe_serial: project.probe_serial.clone(),
                descriptor: build.descriptor,
                pristine: build.pristine,
            })
        }
    }
}

/// What to build and what was picked, with no chip: a bootload talks to the
/// DUT's bootloader, never a probe, so it has no use for Core's SoC-to-chip
/// table and should not fail on a SoC that table lacks (decision 81).
pub struct ResolvedBuild {
    pub plan: BuildPlan,
    pub descriptor: serde_json::Value,
    pub pristine: Option<String>,
}

pub fn resolve_build(project: &ProjectConfig, selection: Selection<'_>) -> Result<ResolvedBuild> {
    resolve_build_with(project, selection, BuildChoice::default())
}

/// [`resolve_build`], building a named variant or starting pristine. Also
/// how a `vs` comparison finds the other variant's directory without
/// building it.
pub fn resolve_build_with(
    project: &ProjectConfig,
    selection: Selection<'_>,
    choice: BuildChoice<'_>,
) -> Result<ResolvedBuild> {
    match project.discovery {
        Discovery::Static => resolve_static(project, selection, choice).map(|r| ResolvedBuild {
            plan: r.plan,
            descriptor: r.descriptor,
            pristine: r.pristine,
        }),
        Discovery::ZephyrWest => resolve_zephyr_build(project, selection, choice).map(|(build, _soc)| build),
    }
}

/// The variant a choice names, or `None` for the default build. An unknown
/// name is an error listing the configured ones.
fn chosen_variant<'p>(project: &'p ProjectConfig, choice: BuildChoice<'_>) -> Result<Option<(&'p str, &'p BuildVariant)>> {
    let Some(name) = choice.build_variant.filter(|n| *n != DEFAULT_BUILD_VARIANT) else {
        return Ok(None);
    };
    match project.build_variants.get_key_value(name) {
        Some((key, variant)) => Ok(Some((key.as_str(), variant))),
        None => {
            let known: Vec<&str> = project.build_variants.keys().map(String::as_str).collect();
            anyhow::bail!(
                "project '{}' has no build variant '{name}' (configured: {}; \"{DEFAULT_BUILD_VARIANT}\" is the \
                 build with none)",
                project.name,
                if known.is_empty() { "none".to_string() } else { known.join(", ") },
            )
        }
    }
}

/// What a variant contributes to the descriptor and `target.json`.
fn variant_descriptor(name: &str, variant: &BuildVariant) -> serde_json::Value {
    serde_json::json!({ "name": name, "cmake_args": variant.cmake_args() })
}

/// Whether a build starts pristine, and why. A variant's directory is
/// reused across builds so switching variant is incremental; it is wiped
/// only when the caller asks or when its `target.json` shows it last built
/// other variant settings — CMake keeps a `-D` value in its cache after the
/// argument is dropped, so a stale directory would build the old settings.
fn pristine_reason(dir: &std::path::Path, variant: Option<&serde_json::Value>, asked: bool) -> Option<String> {
    if asked {
        return Some("asked".to_string());
    }
    let variant = variant?;
    if !dir.is_dir() {
        return None;
    }
    let previous = std::fs::read_to_string(dir.join(TARGET_MANIFEST_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    match previous {
        Some(prev) if prev.get("build_variant") == Some(variant) => None,
        Some(_) => Some("this directory last built other variant settings".to_string()),
        None => Some(format!("this directory has no readable {TARGET_MANIFEST_NAME}, so its CMake cache is of unknown origin")),
    }
}

/// `west build`'s pristine flag, inserted right after `build` the way a
/// hand-typed `west build -p always …` has it.
const PRISTINE_ARGS: [&str; 2] = ["-p", "always"];

/// Whether a variant's CMake arguments go on the command line. `west build`
/// re-runs CMake whenever it is given any, which costs most of a pristine
/// build [measured 2026-11-10: 4.7 s of 7.6 s for a small app]; a directory
/// that is not being wiped and already has a cache whose `target.json`
/// matches the variant ([`pristine_reason`] returned `None`) holds them, so
/// they are left off and the build is ninja alone.
fn needs_configure(dir: &std::path::Path, pristine: Option<&str>) -> bool {
    pristine.is_some() || !dir.join("CMakeCache.txt").is_file()
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

fn resolve_static(project: &ProjectConfig, selection: Selection<'_>, choice: BuildChoice<'_>) -> Result<Resolved> {
    // decision 51. A static project builds by running its
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

    let mut command = project
        .build_command
        .clone()
        .expect("validate() enforces build_command for a static project");
    let (lock_key, artifact_path, manifest, descriptor, pristine) = match chosen_variant(project, choice)? {
        None => {
            if choice.pristine {
                anyhow::bail!(
                    "project '{}' is discovery = \"static\": its default build runs build_command verbatim, \
                     so there is no `-p always` to add to it. Put the flag in build_command, or ask for \
                     pristine on a build variant, whose argv this crate assembles",
                    project.name
                );
            }
            // A `static` project's default build resolves no selection
            // (decision 51) and builds wherever its own `build_command`
            // says, so there is nothing a `target.json` beside it could
            // record.
            (
                project.name.clone(),
                project.resolved_artifact_path(),
                None,
                serde_json::json!({ "project": project.name }),
                None,
            )
        }
        Some((name, variant)) => {
            // Load validation (`static_variants_honourable`) guarantees a
            // plain `west build` argv and an artifact under `build/`.
            let dir = project.build_dir().join(format!("build-{name}"));
            let configured = project.artifact_path.as_deref().expect("validate() enforces artifact_path");
            let artifact_path = dir.join(configured.strip_prefix("build").unwrap_or(configured));
            let variant_json = variant_descriptor(name, variant);
            let pristine = pristine_reason(&dir, Some(&variant_json), choice.pristine);
            if pristine.is_some() {
                command.extend(PRISTINE_ARGS.iter().map(|a| a.to_string()));
            }
            command.extend(["-d".to_string(), dir.display().to_string()]);
            if needs_configure(&dir, pristine.as_deref()) {
                command.push("--".to_string());
                command.extend(variant.cmake_args());
            }
            let descriptor = serde_json::json!({ "project": project.name, "build_variant": variant_json });
            (
                format!("{}::build-{name}", project.name),
                artifact_path,
                Some(TargetManifest { dir, target: descriptor.clone() }),
                descriptor,
                pristine,
            )
        }
    };

    Ok(Resolved {
        plan: BuildPlan {
            lock_key,
            cwd: project.build_dir(),
            command,
            artifact_path,
            timeout_secs: project.build_timeout_secs,
            env: project.env.clone(),
            manifest,
        },
        chip: project
            .chip
            .clone()
            .expect("validate() enforces chip for a static project"),
        flash_format: project.flash_format.clone(),
        base_address: format_base_address(project.base_address),
        probe_serial: project.probe_serial.clone(),
        descriptor,
        pristine,
    })
}

/// The (board, variant, revision, app) `zephyr::select` actually narrows
/// with, once the project's configured `default_target` has filled in
/// whatever the call left out (decision 20).
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
/// the project configures (decision 21).
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
            // The remedy is conditional and this function holds what decides
            // it. "Omit `snippets` to take the project's configured
            // default_snippets" was stated unconditionally and is only a
            // remedy when that default is empty: with a non-empty one,
            // omitting builds *those* rather than none, and the config edit
            // a reader reaches for next — the literal in `default_snippets`
            // — is itself refused at load (decision 21). Naming a remedy
            // that is a second refusal is worse than naming one (decision
            // 51's surface-text rule, task `api/015`).
            let remedy = if default_snippets.is_empty() {
                "Rename that snippet, or omit `snippets` — this project configures no \
                 default_snippets, so omitting builds with none either way."
                    .to_string()
            } else {
                format!(
                    "Rename that snippet: with a configured default it is the only remedy. \
                     Omitting `snippets` would build the project's configured default_snippets \
                     {default_snippets:?}, not none, and \"{NO_SNIPPETS}\" cannot be written into \
                     that list instead — it is refused at config load."
                )
            };
            anyhow::bail!(
                "project '{project_name}' app '{app}' declares a real snippet named \
                 \"{NO_SNIPPETS}\", which collides with the reserved literal meaning \"build with \
                 no snippets\" — refused rather than guessed at. {remedy}"
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
    // **Deduped in place, never sorted.** A snippet list is an *ordered
    // composition*: west applies `-S` in the order given, and the suite has
    // one documented case where the order is the whole difference —
    // `embarch-decision-reversals.md` row 109, where the BLE-shell snippet
    // re-points the shell backend and switches the traced UART off, so the
    // outpost overlay has to be applied *second* or the tracer's own UART
    // goes with it. Sorting was silently authoritative over that: it made
    // two genuinely different images indistinguishable to this function and
    // gave them the same build directory.
    //
    // Order-preserving dedup keeps the property the sort was actually for —
    // one name appearing twice must not produce two `-S` flags or a doubled
    // directory-name segment — without claiming the list is a set.
    let mut seen = std::collections::HashSet::new();
    snippets.retain(|s| seen.insert(s.clone()));

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

/// A `zephyr-west` target's build, and the SoC [`resolve`] maps to a chip.
fn resolve_zephyr_build(
    project: &ProjectConfig,
    selection: Selection<'_>,
    choice: BuildChoice<'_>,
) -> Result<(ResolvedBuild, String)> {
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

    let variant = chosen_variant(project, choice)?;
    if variant.is_some() && extra_args.iter().any(|a| a == "--") {
        anyhow::bail!(
            "project '{}': a build variant appends its own `--` and CMake arguments, and these extra_args \
             already contain `--`; move those CMake arguments into a build variant's defines",
            project.name
        );
    }
    let dir_name = match variant {
        // `.` cannot appear in a board, app or snippet name, so the
        // variant's directory never collides with another target's.
        Some((name, _)) => format!("{}.{name}", target.build_dir_name(&snippets, &extra_args)),
        None => target.build_dir_name(&snippets, &extra_args),
    };
    let build_dir = build_dir_root.join(&dir_name);
    let app_path = zephyr::app_path(&project.source_path, &target.app);
    let mut command = zephyr::build_command(&west_binary, &target, &snippets, &extra_args, &build_dir, &app_path);
    let artifact_path = zephyr::artifact_path(&build_dir, &project.flash_format);

    // Built once and used twice on purpose: this is both what the tool
    // response echoes back and what `build.rs` writes into the build
    // directory as `target.json` (decision 19), so the directory's
    // provenance is the caller's answer rather than a restatement of it.
    let mut descriptor = serde_json::json!({
        "project": project.name,
        "board": target.board,
        "soc": target.soc,
        "cpucluster": target.cpucluster,
        "variant": target.variant,
        "revision": target.revision,
        "app": target.app,
        "snippets": snippets,
        "extra_args": extra_args,
    });
    let variant_json = variant.map(|(name, v)| variant_descriptor(name, v));
    let pristine = pristine_reason(&build_dir, variant_json.as_ref(), choice.pristine);
    if pristine.is_some() {
        // Right after `build`, where `zephyr::build_command` puts extra_args.
        command.splice(2..2, PRISTINE_ARGS.iter().map(|a| a.to_string()));
    }
    if let (Some((_, v)), Some(json)) = (variant, variant_json) {
        if needs_configure(&build_dir, pristine.as_deref()) {
            command.push("--".to_string());
            command.extend(v.cmake_args());
        }
        descriptor["build_variant"] = json;
    }

    let build = ResolvedBuild {
        plan: BuildPlan {
            lock_key: format!("{}::{dir_name}", project.name),
            // west build's -d/app-path args are absolute, so the actual cwd
            // doesn't affect what gets built — source_path just needs to
            // exist, which config validation already guarantees.
            cwd: project.source_path.clone(),
            command,
            artifact_path,
            timeout_secs: project.build_timeout_secs,
            env: project.env.clone(),
            manifest: Some(TargetManifest {
                dir: build_dir,
                target: descriptor.clone(),
            }),
        },
        descriptor,
        pristine,
    };
    Ok((build, target.soc))
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
            let available_by_app: std::collections::HashMap<&str, Vec<String>> = apps
                .iter()
                .map(|app| (*app, zephyr::available_snippets(&project.source_path, app)))
                .collect();
            let snippets_by_app: serde_json::Map<String, serde_json::Value> = available_by_app
                .iter()
                .map(|(app, available)| (app.to_string(), serde_json::json!(available)))
                .collect();

            // `build_dir_name` (decision 77): the identity `embarch-umbrella`
            // decision 26 needs to prune a build directory whose target
            // combination is no longer valid, folded here rather than left
            // for a second, lossier attempt to derive it from a directory
            // name alone (every segment can contain `-`). Resolved with this
            // project's own configured defaults — the identity a bare
            // `build` for this row writes to today — not the caller's, since
            // a listing has no call-time override to fold in.
            //
            // `null` when this app's available snippets don't cover
            // `default_snippets`: exactly the case a bare `build` for this
            // target already refuses at build time (`resolve_snippets`'s
            // "unknown snippet(s)" error), surfaced here instead of only
            // there. Never an error for the whole listing — one target's
            // snippet mismatch must not hide every other row's identity.
            let targets_json: Vec<serde_json::Value> = targets
                .iter()
                .map(|t| {
                    let available = available_by_app.get(t.app.as_str()).cloned().unwrap_or_default();
                    let build_dir_name = resolve_snippets(
                        &project.name,
                        &t.app,
                        &[],
                        &project.default_snippets,
                        &available,
                    )
                    .ok()
                    .map(|snippets| t.build_dir_name(&snippets, &project.default_extra_args));
                    let mut value = serde_json::to_value(t).expect("Target always serializes");
                    value["build_dir_name"] = serde_json::json!(build_dir_name);
                    value
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
                "targets": targets_json,
                "snippets_by_app": snippets_by_app,
                "default_snippets": project.default_snippets,
                "default_extra_args": project.default_extra_args,
                "default_target": default_target,
            }))
        }
        Discovery::Static => {
            // **A static project has exactly one target: itself**
            // (decision 53). It used to return the
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
/// (`embarch-core` decision 18's `parse_base_address`), while
/// the config field is a TOML integer (decision 42) so a
/// value written `0x2000` reads as one. `{:#x}` is the round trip: it is the
/// form Core's own error message names, and the form a `bin` bench's
/// `[dev_bench] base_address` is written in too — which is why this is
/// `pub` rather than private: `embarch-api`'s `dev_bench.rs` needs the
/// identical round trip now that its offset is config rather than a
/// constant string, and since this module moved out of that crate
/// (2026-09-18) `pub(crate)` no longer reaches it.
pub fn format_base_address(configured: Option<u64>) -> Option<String> {
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
            resolve_static(&static_project("base_address = 0x2000"), Selection::default(), BuildChoice::default()).unwrap();
        assert_eq!(resolved.base_address.as_deref(), Some("0x2000"));
    }

    #[test]
    fn resolve_static_leaves_base_address_unset_when_the_project_omits_it() {
        let resolved = resolve_static(&static_project(""), Selection::default(), BuildChoice::default()).unwrap();
        assert_eq!(resolved.base_address, None);
    }

    /// Decision 51's other half: rejecting a selection must not disturb the
    /// call that gives none. Asserted over the whole `Resolved`, not just the
    /// error path, because "unchanged" is the claim the new check makes.
    #[test]
    fn resolve_static_with_no_selection_resolves_exactly_as_before() {
        let resolved = resolve_static(&static_project(""), Selection::default(), BuildChoice::default()).unwrap();
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
        assert!(resolve_static(&static_project(""), explicit, BuildChoice::default()).is_ok());
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
            BuildChoice::default(),
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
            BuildChoice::default(),
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
        let resolved = resolve_static(&project, Selection::default(), BuildChoice::default()).unwrap();
        assert_eq!(
            targets[0]["artifact_path"],
            serde_json::json!(resolved.plan.artifact_path.display().to_string())
        );
        // Nothing else is advertised: no menu, and no selection axes a
        // `static` project would only refuse (decision 51).
        assert_eq!(value.as_object().unwrap().len(), 1, "{value:#}");
    }

    // Minimal on-disk zephyr-west fixture, scoped to this module rather than
    // reusing `zephyr.rs`'s own private test helpers: one board with no
    // variant/revision axes, one app, so `scan_or_err` returns exactly one
    // `Target` — enough to exercise `list_targets`'s `build_dir_name`
    // without pulling in the fuller scan matrix `zephyr.rs`'s own tests
    // already cover.
    struct ZephyrFixture(std::path::PathBuf);
    impl ZephyrFixture {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for ZephyrFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_zephyr_fixture(with_snippet: bool) -> ZephyrFixture {
        let mut base = std::env::temp_dir();
        let unique = format!(
            "embarch-api-resolve-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        base.push(unique);
        let root = base.clone();

        let board_dir = root.join("boards/acme/single");
        std::fs::create_dir_all(&board_dir).unwrap();
        std::fs::write(
            board_dir.join("single.yml"),
            "board:\n  name: single\n  socs:\n    - name: nrf54l15\n",
        )
        .unwrap();
        std::fs::write(board_dir.join("single_nrf54l15.dts"), "").unwrap();

        let app_dir = root.join("app/widget");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("CMakeLists.txt"), "").unwrap();

        if with_snippet {
            let snippet_dir = app_dir.join("snippets/ble-shell");
            std::fs::create_dir_all(&snippet_dir).unwrap();
            std::fs::write(snippet_dir.join("snippet.yml"), "name: ble-shell\n").unwrap();
        }

        ZephyrFixture(root)
    }

    fn zephyr_project(source_path: &std::path::Path, extra: &str) -> ProjectConfig {
        toml::from_str(&format!(
            r#"
name = "p"
source_path = "{source}"
discovery = "zephyr-west"
flash_format = "hex"
{extra}
"#,
            source = source_path.display().to_string().replace('\\', "\\\\"),
        ))
        .expect("test project config should parse")
    }

    /// The identity `embarch-umbrella` decision 26 asked this crate to
    /// publish (decision 77): a bare `build` for this row, with no snippets
    /// and no extra_args configured, writes to `build_dir_name(&[], &[])`
    /// exactly.
    #[test]
    fn list_targets_reports_build_dir_name_for_the_default_combination() {
        let fixture = write_zephyr_fixture(false);
        let project = zephyr_project(fixture.path(), "");
        let value = list_targets(&project).expect("a single board/app scans to one target");
        let targets = value["targets"].as_array().expect("targets is an array");
        assert_eq!(targets.len(), 1, "{value:#}");
        assert_eq!(
            targets[0]["build_dir_name"],
            serde_json::json!("single-default-none-widget"),
            "{value:#}"
        );
    }

    /// Same fixture, but the project configures a `default_snippets` entry
    /// its one app never declares. A bare `build` for this target already
    /// refuses with "unknown snippet(s)" (`resolve_snippets`) — this listing
    /// must report that same target as `build_dir_name: null` rather than
    /// failing the whole call or fabricating a name nothing would actually
    /// build to.
    #[test]
    fn list_targets_nulls_build_dir_name_when_the_default_snippet_is_unavailable() {
        let fixture = write_zephyr_fixture(false);
        let project = zephyr_project(fixture.path(), r#"default_snippets = ["ble-shell"]"#);
        let value = list_targets(&project).expect("an unavailable default snippet must not fail the listing");
        let targets = value["targets"].as_array().expect("targets is an array");
        assert_eq!(targets.len(), 1, "{value:#}");
        assert!(targets[0]["build_dir_name"].is_null(), "{value:#}");
    }

    /// The same `default_snippets` value, but the fixture's app really does
    /// declare it: `build_dir_name` folds it in exactly the way
    /// `Target::build_dir_name` does for a real build.
    #[test]
    fn list_targets_folds_a_real_default_snippet_into_build_dir_name() {
        let fixture = write_zephyr_fixture(true);
        let project = zephyr_project(fixture.path(), r#"default_snippets = ["ble-shell"]"#);
        let value = list_targets(&project).expect("a declared default snippet must resolve");
        let targets = value["targets"].as_array().expect("targets is an array");
        assert_eq!(targets.len(), 1, "{value:#}");
        assert_eq!(
            targets[0]["build_dir_name"],
            serde_json::json!("single-default-none-widget-ble-shell"),
            "{value:#}"
        );
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
        // With no configured default, omitting `snippets` really does give
        // the caller the empty set they asked for, so it is offered.
        assert!(message.contains("omit `snippets`"), "{message}");
    }

    /// **The advice is conditional and the message says which case it is
    /// in.** It used to offer "omit `snippets` to take the project's
    /// configured default_snippets" unconditionally: with a non-empty
    /// default that builds those snippets rather than none, and the config
    /// edit it invites — the literal in `default_snippets` — is itself a
    /// load error, so the reader is routed into a second refusal. A message
    /// naming an unreachable remedy is worse than one naming a single
    /// reachable one (decision 51, task `api/015`).
    #[test]
    fn the_collision_message_does_not_offer_omitting_when_a_default_is_configured() {
        let real = vec!["none".to_string(), "ble-shell".to_string()];
        let configured = vec!["ble-shell".to_string()];
        let err = resolve_snippets("p", "widget", &[NO_SNIPPETS.to_string()], &configured, &real)
            .map(|_| ())
            .expect_err("a real snippet named none makes the literal ambiguous");
        let message = format!("{err:#}");
        assert!(message.contains("collides"), "{message}");
        // Renaming is offered as the only remedy...
        assert!(message.contains("the only remedy"), "{message}");
        // ...and the other two paths a reader would try are named as the
        // dead ends they are, rather than left to be discovered by running
        // into them.
        assert!(message.contains("[\"ble-shell\"]"), "{message}");
        assert!(message.contains("refused at config load"), "{message}");
        // Asserted as an absence too, so the unconditional phrasing cannot
        // come back without this test failing.
        assert!(
            !message.contains("or omit `snippets`"),
            "the unreachable remedy is offered anyway: {message}"
        );
    }

    #[test]
    fn an_explicit_list_still_replaces_the_default_and_is_deduped_in_place() {
        let configured = vec!["ble-shell".to_string()];
        let call = vec![
            "wdt31".to_string(),
            "ble-shell".to_string(),
            "wdt31".to_string(),
        ];
        let resolved = resolve_snippets("p", "widget", &call, &configured, &available()).unwrap();
        // The caller's order, with the repeat dropped at its *second*
        // occurrence — not sorted.
        assert_eq!(resolved, vec!["wdt31".to_string(), "ble-shell".to_string()]);
    }

    /// Reversals row 109: `-S a -S b` and `-S b -S a` are different images
    /// whenever one snippet disables what the other needs, which is exactly
    /// the outpost-over-BLE-shell case. This function used to sort, making
    /// the two indistinguishable — and, through `build_dir_name`, giving
    /// them the same build directory to overwrite each other in.
    #[test]
    fn two_orderings_of_the_same_snippets_stay_two_different_selections() {
        let forwards = vec!["ble-shell".to_string(), "wdt31".to_string()];
        let backwards = vec!["wdt31".to_string(), "ble-shell".to_string()];
        let a = resolve_snippets("p", "widget", &forwards, &[], &available()).unwrap();
        let b = resolve_snippets("p", "widget", &backwards, &[], &available()).unwrap();
        assert_eq!(a, forwards);
        assert_eq!(b, backwards);
        assert_ne!(a, b, "order was flattened away");
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

    // --- build variants (decision 103) ---

    const VARIANTS: &str = r#"
[build_variants.bench]
extra_conf_files = ["bench.conf"]
overlays = ["bench.overlay"]
defines = ["CONFIG_FOO=y"]
"#;

    fn west_static(extra: &str) -> ProjectConfig {
        static_project(&format!("{extra}\n")).with_command(&["west", "build", "-b", "b", "app"], "build/zephyr/zephyr.hex")
    }

    impl ProjectConfig {
        fn with_command(mut self, argv: &[&str], artifact: &str) -> ProjectConfig {
            self.build_command = Some(argv.iter().map(|s| s.to_string()).collect());
            self.artifact_path = Some(artifact.into());
            self
        }
    }

    fn bench() -> BuildChoice<'static> {
        BuildChoice { build_variant: Some("bench"), pristine: false }
    }

    #[test]
    fn a_static_variant_builds_west_into_its_own_directory_with_its_cmake_args() {
        let r = resolve_static(&west_static(VARIANTS), Selection::default(), bench()).unwrap();
        let dir = std::path::Path::new("/nonexistent/build-bench");
        assert_eq!(
            r.plan.command,
            [
                "west", "build", "-b", "b", "app", "-d", "/nonexistent/build-bench", "--",
                "-DEXTRA_CONF_FILE=bench.conf", "-DEXTRA_DTC_OVERLAY_FILE=bench.overlay", "-DCONFIG_FOO=y",
            ]
        );
        assert_eq!(r.plan.artifact_path, dir.join("zephyr/zephyr.hex"));
        assert_eq!(r.plan.lock_key, "p::build-bench");
        assert_eq!(r.plan.manifest.as_ref().unwrap().dir, dir);
        assert_eq!(r.descriptor["build_variant"]["name"], "bench");
        // A directory that does not exist yet needs no wiping.
        assert_eq!(r.pristine, None);
    }

    #[test]
    fn the_default_variant_name_is_the_build_with_none() {
        let choice = BuildChoice { build_variant: Some(DEFAULT_BUILD_VARIANT), pristine: false };
        let r = resolve_static(&west_static(VARIANTS), Selection::default(), choice).unwrap();
        assert_eq!(r.plan.command, ["west", "build", "-b", "b", "app"]);
        assert!(r.plan.manifest.is_none());
    }

    #[test]
    fn an_unknown_variant_is_refused_naming_the_configured_ones() {
        let choice = BuildChoice { build_variant: Some("nope"), pristine: false };
        let err = resolve_static(&west_static(VARIANTS), Selection::default(), choice).err().unwrap();
        let message = format!("{err:#}");
        assert!(message.contains("no build variant 'nope'") && message.contains("bench"), "{message}");
    }

    #[test]
    fn pristine_on_a_static_default_build_is_refused_and_on_a_variant_is_added() {
        let asked = BuildChoice { build_variant: None, pristine: true };
        let err = resolve_static(&west_static(VARIANTS), Selection::default(), asked).err().unwrap();
        assert!(format!("{err:#}").contains("no `-p always` to add"), "{err:#}");

        let asked = BuildChoice { build_variant: Some("bench"), pristine: true };
        let r = resolve_static(&west_static(VARIANTS), Selection::default(), asked).unwrap();
        assert_eq!(r.pristine.as_deref(), Some("asked"));
        assert_eq!(&r.plan.command[5..9], ["-p", "always", "-d", "/nonexistent/build-bench"]);
    }

    #[test]
    fn a_variant_directory_that_last_built_other_settings_is_rebuilt_pristine() {
        let fixture = write_zephyr_fixture(false);
        let project = zephyr_project(
            fixture.path(),
            &format!("west_binary = \"west\"\nbuild_dir_root = \"{}\"\n{VARIANTS}", fixture.path().join("out").display()),
        );
        let first = resolve_build_with(&project, Selection::default(), bench()).unwrap();
        let dir = first.plan.manifest.as_ref().unwrap().dir.clone();
        assert!(dir.ends_with("single-default-none-widget.bench"), "{}", dir.display());
        assert_eq!(first.pristine, None, "no directory yet");
        assert_eq!(first.plan.command[1..3], ["build".to_string(), "-b".to_string()]);
        assert_eq!(first.plan.command.last().unwrap(), "-DCONFIG_FOO=y");

        // The same settings recorded: incremental.
        std::fs::create_dir_all(&dir).unwrap();
        crate::build::write_target_manifest(first.plan.manifest.as_ref().unwrap()).unwrap();
        let again = resolve_build_with(&project, Selection::default(), bench()).unwrap();
        assert_eq!(again.pristine, None);
        assert_eq!(again.plan.command.last().unwrap(), "-DCONFIG_FOO=y", "no cache yet: configure");
        // …and once CMake has a cache there, the arguments it holds are left
        // off so west does not reconfigure.
        std::fs::write(dir.join("CMakeCache.txt"), "").unwrap();
        let cached = resolve_build_with(&project, Selection::default(), bench()).unwrap();
        assert!(!cached.plan.command.iter().any(|a| a == "--"), "{:?}", cached.plan.command);

        // A define dropped from config: pristine, because CMake would keep it.
        let changed = zephyr_project(
            fixture.path(),
            &format!(
                "west_binary = \"west\"\nbuild_dir_root = \"{}\"\n[build_variants.bench]\nextra_conf_files = [\"bench.conf\"]\n",
                fixture.path().join("out").display()
            ),
        );
        let after = resolve_build_with(&changed, Selection::default(), bench()).unwrap();
        assert_eq!(after.pristine.as_deref(), Some("this directory last built other variant settings"));
        assert_eq!(after.plan.command[2..4], ["-p".to_string(), "always".to_string()]);
    }
}
