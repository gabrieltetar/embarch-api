//! What this binary says about the commit it was built from (decision 93).
//!
//! The values are set by `build.rs`, the stamp `embarch-umbrella` decision 57
//! shares across the suite's three binaries. An MCP server keeps the binary it
//! was spawned with while its checkout moves on, and until 2026-10-28 every
//! build printed `0.1.0`, so nothing could tell a session which one answered.

/// The crate version, `CARGO_PKG_VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The full commit sha this binary was built from, or `unknown` when it was
/// built outside the root of a git checkout.
pub const COMMIT: &str = env!("EMBARCH_BUILD_COMMIT");

/// The directory this crate was built in — the checkout `embarch doctor`
/// check 18 compares [`COMMIT`] against.
pub const SOURCE_DIR: &str = env!("EMBARCH_BUILD_SOURCE");

/// What `--version` prints after the binary name: `0.1.0 (commit 744b94b0)`,
/// with `, dirty` inside the parentheses for a build with uncommitted changes.
pub const VERSION_LINE: &str = env!("EMBARCH_BUILD_VERSION");

/// Whether tracked files differed from [`COMMIT`] when this was built.
pub fn dirty() -> bool {
    env!("EMBARCH_BUILD_DIRTY") == "1"
}

/// The `api_build` object `status` carries, CLI and MCP alike.
pub fn json() -> serde_json::Value {
    serde_json::json!({
        "version": VERSION,
        "commit": COMMIT,
        "dirty": dirty(),
    })
}
