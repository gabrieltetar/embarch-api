//! `interfaces/tools.md:5`'s whole premise is "one table, because these are
//! two front-ends over one implementation — not two surfaces to keep in
//! sync." Nothing checked that until this test: it derives the MCP tool
//! list straight from `src/tools.rs`'s `#[tool(description = ...)]` +
//! `async fn <name>` pairs, derives the CLI subcommand list straight from
//! `src/main.rs`'s `Commands` enum variants, and asserts every tool has a
//! matching kebab-case subcommand and vice versa — outside the two
//! documented asymmetries below.
//!
//! `include_str!`, not a build-time enumeration: this crate's `#[tool]`s
//! live on a type behind `rmcp`'s macros, and `Commands` is a `clap`
//! `Subcommand` enum with no `strum`-style `VARIANTS` const — neither is
//! reflectable at runtime without adding a dependency for it. Parsing the
//! source text is deliberately the same trick `cli.rs`'s own
//! `every_subcommand_is_covered_by_the_json_surface_test` and
//! `no_json_reaches_stdout_except_through_json_out` already use for the
//! same reason.

use std::collections::HashSet;

const TOOLS_SRC: &str = include_str!("../src/tools.rs");
const MAIN_SRC: &str = include_str!("../src/main.rs");

/// The suite's two documented CLI↔MCP naming asymmetries (`spec.md` §1:
/// "a superset, `versions` having no tool"; the `study_watch`/
/// `study-status --follow` split cited from the same section in
/// `interfaces/tools.md` and spelled out in `interfaces/studies.md`).
/// Every other MCP tool must have a matching kebab-case CLI subcommand, and
/// every other CLI subcommand must have a matching MCP tool — these two are
/// named, cited exceptions to that rule, never an inline skip.
enum Asymmetry {
    /// A CLI subcommand with no MCP tool of the same name at all.
    CliOnly(&'static str),
    /// An MCP tool reached on the CLI under a different subcommand name,
    /// rather than one of its own.
    ToolReachedAs(&'static str, &'static str),
}

const DOCUMENTED_ASYMMETRIES: &[Asymmetry] = &[
    Asymmetry::CliOnly("versions"),
    Asymmetry::ToolReachedAs("study_watch", "study-status"),
];

/// Every `async fn` immediately preceded by a `#[tool(description = ...)]`
/// attribute inside `src/tools.rs`, in source order. Deliberately tied to
/// that exact adjacency (the real macro's own requirement) rather than a
/// looser "any fn in this file" scan.
fn tool_fn_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if !line.trim_start().starts_with("#[tool(description") {
            continue;
        }
        for next in lines.by_ref() {
            let trimmed = next.trim_start();
            if let Some(rest) = trimmed.strip_prefix("async fn ") {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                assert!(
                    !name.is_empty(),
                    "found `#[tool(description = ...)]` with no fn name after it"
                );
                names.push(name);
                break;
            }
        }
    }
    names
}

/// Every variant name of `src/main.rs`'s `pub enum Commands`, in source
/// order. Variant lines sit at exactly one indent level (4 spaces) inside
/// the enum body; doc comments, attributes and nested field lines (8+
/// spaces) are skipped.
fn command_variant_names(src: &str) -> Vec<String> {
    let marker = "pub enum Commands {";
    let start = src
        .find(marker)
        .expect("src/main.rs no longer declares `pub enum Commands` verbatim")
        + marker.len();
    let rest = &src[start..];
    let end = rest
        .find("\n}\n")
        .expect("could not find the end of `enum Commands` (expected a `}` at column 0)");
    let body = &rest[..end];

    let mut names = Vec::new();
    for line in body.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if indent != 4
            || trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with('}')
        {
            // Comments, attributes, and a struct-variant's own closing `},`
            // (which sits back at the variant indent) are not new variants.
            continue;
        }
        let name: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        assert!(
            name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
            "unexpected line at variant indent inside `enum Commands`: {line:?}"
        );
        names.push(name);
    }
    names
}

/// `ResetDevBench` -> `reset-dev-bench`: what `clap`'s `Subcommand` derive
/// does to a variant name by default (no `#[command(rename_all = ...)]`
/// overrides it in `src/main.rs`, confirmed by reading the derive).
fn pascal_to_kebab(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn every_tool_has_a_matching_subcommand_and_vice_versa() {
    let tools = tool_fn_names(TOOLS_SRC);
    assert!(tools.len() > 20, "tool extraction found suspiciously few tools: {tools:?}");

    let variants = command_variant_names(MAIN_SRC);
    assert!(
        variants.len() > 20,
        "subcommand extraction found suspiciously few variants: {variants:?}"
    );

    let subcommand_kebabs: HashSet<String> = variants.iter().map(|v| pascal_to_kebab(v)).collect();
    let tool_kebabs: HashSet<String> = tools.iter().map(|t| t.replace('_', "-")).collect();

    for tool in &tools {
        let kebab = tool.replace('_', "-");
        if subcommand_kebabs.contains(&kebab) {
            continue;
        }
        let excepted = DOCUMENTED_ASYMMETRIES.iter().any(|a| {
            matches!(a, Asymmetry::ToolReachedAs(t, reached_as) if *t == tool
                && subcommand_kebabs.contains(*reached_as))
        });
        assert!(
            excepted,
            "MCP tool `{tool}` has no matching `{kebab}` CLI subcommand and is not \
             one of `DOCUMENTED_ASYMMETRIES` (`tests/tool_subcommand_parity.rs`). \
             `interfaces/tools.md:5`'s premise is one table for both front-ends: \
             either add the `{kebab}` subcommand, or — if this is a real, new, \
             documented asymmetry — add it to `DOCUMENTED_ASYMMETRIES` citing where \
             it's documented, never as a silent skip."
        );
    }

    for variant in &variants {
        let kebab = pascal_to_kebab(variant);
        if tool_kebabs.contains(&kebab) {
            continue;
        }
        let excepted = DOCUMENTED_ASYMMETRIES
            .iter()
            .any(|a| matches!(a, Asymmetry::CliOnly(name) if *name == kebab));
        assert!(
            excepted,
            "CLI subcommand `{kebab}` has no matching MCP tool and is not one of \
             `DOCUMENTED_ASYMMETRIES` (`tests/tool_subcommand_parity.rs`). \
             `interfaces/tools.md:5`'s premise is one table for both front-ends: \
             either add the `{}` MCP tool, or — if this is a real, new, documented \
             asymmetry — add it to `DOCUMENTED_ASYMMETRIES` citing where it's \
             documented, never as a silent skip.",
            kebab.replace('-', "_")
        );
    }
}
