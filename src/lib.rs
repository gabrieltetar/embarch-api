//! `embarch-api`'s library face — deliberately two modules wide.
//!
//! This crate is a binary: an MCP server plus the mirroring CLI, and
//! everything in `main.rs` stays in `main.rs`. The modules lifted behind a
//! `lib` target are [`build`] and [`json_out`], and in both cases the
//! reason is testability rather than reuse.
//!
//! A Rust binary crate has no importable surface at all — each file under
//! `tests/` compiles as its own crate and can reach a package's `lib` and
//! nothing else. Three of the six acceptance criteria `embarch-api/open.md`
//! has carried unwritten since the MCP surface landed (the two-pipe drain
//! invariant, truncation on a UTF-8 character boundary, and an untouched
//! pre-existing artifact not counting as fresh) all live in [`build`], so
//! until this file existed they were not merely untested but *untestable*
//! from an integration test. See `embarch-doc/embarch-api/decisions.md`
//! decision 47.
//!
//! `main.rs` imports [`build`] from here rather than declaring it a second
//! time, so there is exactly one compiled copy and the bin and the tests
//! exercise the same code.
//!
//! **Both modules now live in `embarch-firmware-build`** (2026-09-18) and
//! are re-exported here. Nothing above changes: a file under `tests/` still
//! reaches them as `embarch_api::build` / `embarch_api::json_out`, and the
//! acceptance criteria named below are still pinned from there. What the
//! move buys is `embarch-ui` reaching the same implementation, which it
//! could never do through this crate's `lib` face — it cannot depend on
//! this package at all.

pub use embarch_firmware_build::build;

/// The `--json` surface's single serializer, lifted here for the same
/// reason [`build`] was: `tests/` cannot reach a binary crate's modules, and
/// `tests/json_surface.rs` has to compare what the binary printed against
/// [`json_out::SCHEMA_VERSION`] rather than against a second copy of the
/// number. See `embarch-doc/embarch-api/decisions.md` decision 50.
pub use embarch_firmware_build::json_out;
