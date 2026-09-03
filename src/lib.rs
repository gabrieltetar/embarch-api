//! `embarch-api`'s library face — deliberately one module wide.
//!
//! This crate is a binary: an MCP server plus the mirroring CLI, and
//! everything in `main.rs` stays in `main.rs`. The single module lifted
//! behind a `lib` target is [`build`], and the reason is testability rather
//! than reuse.
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

pub mod build;
