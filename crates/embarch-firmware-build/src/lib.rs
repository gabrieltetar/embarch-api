//! The suite's firmware build machinery: config, discovery, resolution,
//! and the build itself.
//!
//! Extracted from `embarch-api` on 2026-09-18 so that `embarch-ui` can
//! build a study's firmware before running it (`embarch-ui` decision 11,
//! reversed). The move is the same one `embarch-core-client` already made,
//! for the same reason and between the same two crates: **`embarch-ui`
//! cannot depend on `embarch-api`** — no such dependency direction exists
//! in the suite — so anything both of them need has to live somewhere they
//! both can reach. `reflash.rs`'s own doc comment records the precedent.
//!
//! # What is here, and what is deliberately not
//!
//! - [`config`] — the TOML a bench declares its projects in. **Loading it
//!   from a path is here; deciding *which* path is not:** `embarch-api`
//!   resolves `--config`/`EMBARCH_API_CONFIG`/cwd in its own `main.rs`, and
//!   `embarch-ui` answers that question differently.
//! - [`zephyr`] — the live scan of a west workspace: boards, apps,
//!   snippets, revisions.
//! - [`resolve`] — a [`resolve::Selection`] narrowed against that scan into
//!   one buildable target.
//! - [`build`] — running the build command, draining its two pipes, and
//!   deciding whether what came out is genuinely fresh.
//! - [`json_out`] — the one serializer, and the one `schema_version`. It
//!   came along because [`build::write_target_manifest`] writes
//!   `target.json` through it (`embarch-api` decisions 19 and 50): the
//!   manifest's version stamp and the `--json` surface's are the same
//!   number by decision 50, so the stamper has to sit wherever the build
//!   machinery does. `embarch-api` re-exports it, and its
//!   `no_json_reaches_stdout_except_through_json_out` guard is unchanged.
//!
//! **No flashing, and no probe.** A build is `west` in a subprocess and
//! files on disk; putting an image on a board is Core's, over HTTP, for
//! both callers alike (`embarch-api` decisions 37/38). Nothing here links
//! `probe-rs` or `serialport`, and nothing here should start.
//!
//! **No `git checkout`, either.** The rule `reflash.rs` states — this path
//! builds the working tree *as it stands* and never moves it — is a
//! property of the machinery, not of the caller, and it survives the move
//! because nothing here spawns `git` at all.

pub mod build;
pub mod config;
pub mod json_out;
pub mod resolve;
pub mod zephyr;
