//! `embarch-api`'s own rolling logfile (`design.md` §3 decision 43), so
//! [embarch-ui](../../embarch-doc/embarch-ui/design.md)'s Debug tab can show
//! this process's logs at all.
//!
//! Core solves the same problem with `/logs/recent` + `/logs/stream`, which
//! works because Core is a long-running service there is something to ask.
//! `embarch-api` is the opposite shape — spawned per Claude Code session as
//! an MCP server, or run once as a CLI and gone — so the record has to
//! outlive the process that wrote it. That is the property a file has and an
//! endpoint does not, and it is the whole argument.
//!
//! Two things every line carries, because both modes append to the *same*
//! file and interleaved sessions would otherwise be unreadable: the pid, and
//! which mode wrote it. Where the file lives is
//! `embarch_core_client::api_log`'s to say — `embarch-ui` reads the same
//! path from the same function, and a path resolved twice is a path that
//! eventually disagrees with itself.

use anyhow::Context;
use tracing_subscriber::fmt::format::{Format, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// Which of `embarch-api`'s two entry points is running (`design.md` §3
/// decisions 4 and 3.10) — the tag that makes one shared logfile legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Spawned by an MCP client, speaking JSON-RPC over stdio.
    Mcp,
    /// A one-shot subcommand run by a human at a terminal.
    Cli,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Mcp => "mcp",
            Mode::Cli => "cli",
        }
    }
}

/// Prefixes every formatted event with `pid=<n> mode=<mcp|cli>`, then
/// delegates to `tracing-subscriber`'s own default format.
///
/// Done as an event formatter rather than by entering a long-lived span
/// carrying the two fields: a span guard doesn't follow an `await` into
/// another task, and the MCP server path is full of those — half the lines
/// would come out untagged, which is worse than no tag at all in a file
/// whose entire purpose is separating interleaved sessions.
struct PidModePrefixed {
    pid: u32,
    mode: &'static str,
    inner: Format,
}

impl<S, N> FormatEvent<S, N> for PidModePrefixed
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        write!(writer, "pid={} mode={} ", self.pid, self.mode)?;
        self.inner.format_event(ctx, writer, event)
    }
}

/// Installs the global subscriber: stderr (unchanged — it is what an MCP
/// client surfaces and what a terminal shows) **plus** the rolling file.
///
/// **A logfile that cannot be opened must never stop `embarch-api` from
/// running.** A failure here degrades to stderr-only and says so once, the
/// same posture `embarch-core`'s own `init_tracing` takes — this is
/// debug tooling, and refusing to flash a board because a log directory is
/// read-only would be a strictly worse outcome than losing the log.
/// `tracing_subscriber::fmt()`'s builder defaults to this; a bare
/// `registry()` defaults to no filter at all, which is how the first
/// two-layer build of this module started writing `hyper`/`reqwest` `TRACE`
/// into the file. Stated explicitly rather than inherited, and deliberately
/// the same level `embarch-core`'s own `init_tracing` runs at — neither
/// honors `RUST_LOG` today.
const LEVEL: tracing_subscriber::filter::LevelFilter = tracing_subscriber::filter::LevelFilter::INFO;

pub fn init(mode: Mode) {
    let format = || PidModePrefixed {
        pid: std::process::id(),
        mode: mode.as_str(),
        inner: Format::default(),
    };

    // stdout is the MCP JSON-RPC transport — logging must go to stderr, or
    // it corrupts the stream the moment anything logs.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .event_format(format())
        .with_writer(std::io::stderr);

    match build_file_appender() {
        Ok(file) => {
            // Two layers rather than one writer teed across both
            // destinations, for exactly one reason: `with_ansi(false)` on
            // the file half. `embarch-core` tees, and the consequence is
            // visible in its shipped logfile — every line carries SGR escape
            // sequences, which `embarch-ui`'s Debug tab renders as literal
            // garbage. A file whose whole purpose is being read by something
            // that is not a terminal should not be colored for one.
            tracing_subscriber::registry()
                .with(LEVEL)
                .with(stderr_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(format())
                        .with_ansi(false)
                        .with_writer(file),
                )
                .init();
        }
        Err(e) => {
            tracing_subscriber::registry().with(LEVEL).with(stderr_layer).init();
            tracing::warn!("failed to set up the rolling log file, continuing with stderr only: {e:#}");
        }
    }
}

/// Daily rotation with 7-file retention, named `api.log.<yyyy-MM-dd>` —
/// deliberately the same scheme and the same retention as `embarch-core`'s
/// own logfile, so `embarch-ui` tails the two with one reader shape rather
/// than two.
fn build_file_appender() -> anyhow::Result<tracing_appender::rolling::RollingFileAppender> {
    let dir = embarch_core_client::api_log::log_dir()?;
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(embarch_core_client::api_log::LOG_FILE_PREFIX)
        .max_log_files(7)
        .build(&dir)
        .with_context(|| format!("failed to initialize the rolling log file in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_tags_are_the_two_stable_strings_the_ui_and_a_human_grep_for() {
        assert_eq!(Mode::Mcp.as_str(), "mcp");
        assert_eq!(Mode::Cli.as_str(), "cli");
    }

    #[test]
    fn the_appender_targets_the_shared_path_not_a_second_opinion_about_it() {
        // The one thing this module must not do is resolve the log location
        // itself — `embarch-ui` reads it from the same function.
        let dir = embarch_core_client::api_log::log_dir().unwrap();
        assert!(dir.ends_with("api/logs") || dir.ends_with(r"api\logs"), "{dir:?}");
    }
}
