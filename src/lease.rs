//! `lease_take`, `lease_renew`, `lease_release`, `lease_show`: the client half
//! of embarch-core's bench lease (`embarch-core` decision 93, `embarch-api`
//! decision 90). The MCP tools and the CLI subcommands are two front-ends over
//! the functions here, so the wording is written once.
//!
//! A refusal is Core's own text, verbatim, so a session reads who holds the
//! bench and until when; it is never paraphrased or wrapped in a guess.

use embarch_core_client::{CoreClient, LEASE_TOKEN_ENV};

/// A tool or subcommand result: `value` for `--json`/MCP, `human` for a terminal.
pub struct Report {
    pub success: bool,
    pub value: serde_json::Value,
    pub human: String,
}

/// What the MCP server asks for when the caller names none. The server
/// renews on a timer, so this is how long a *crashed* session keeps the bench.
pub const MCP_DEFAULT_TTL_S: u64 = 900;
/// The CLI has no process to renew for it, so it takes longer by default.
pub const CLI_DEFAULT_TTL_S: u64 = 3600;

fn failed(what: &str, e: anyhow::Error) -> Report {
    let message = format!("{what} failed: {e:#}");
    Report { success: false, value: serde_json::json!({ "success": false, "error": message }), human: message }
}

pub async fn take(core: &CoreClient, holder: &str, purpose: Option<&str>, ttl_s: u64) -> Report {
    match core.lease_take(holder, purpose, ttl_s).await {
        Ok(grant) => {
            let human = format!(
                "took the bench lease as '{}' until {} (ttl {} s)\n\
                 token: {}\n\
                 to send it from another process (an embarch-api CLI call, embarch-atlas): \
                 export {LEASE_TOKEN_ENV}={}",
                grant.lease.holder, grant.lease.expires_at, grant.lease.ttl_s, grant.token, grant.token
            );
            Report {
                success: true,
                value: serde_json::json!({ "success": true, "lease": grant.lease, "token": grant.token, "token_env": LEASE_TOKEN_ENV }),
                human,
            }
        }
        Err(e) => failed("lease_take", e),
    }
}

pub async fn renew(core: &CoreClient, ttl_s: Option<u64>) -> Report {
    match core.lease_renew(ttl_s).await {
        Ok(view) => {
            let human = format!("renewed the bench lease held by '{}' until {} (ttl {} s)", view.holder, view.expires_at, view.ttl_s);
            Report { success: true, value: serde_json::json!({ "success": true, "lease": view }), human }
        }
        Err(e) => failed("lease_renew", e),
    }
}

pub async fn release(core: &CoreClient) -> Report {
    match core.lease_release().await {
        Ok(()) => Report {
            success: true,
            value: serde_json::json!({ "success": true, "released": true }),
            human: "released the bench lease".to_string(),
        },
        Err(e) => failed("lease_release", e),
    }
}

pub async fn show(core: &CoreClient) -> Report {
    match core.lease_show().await {
        Ok(view) => {
            let holding = core.effective_lease_token().is_some();
            let human = match &view {
                Some(v) => format!(
                    "bench leased by '{}' for '{}' (started {}, expires {}); {}",
                    v.holder,
                    v.purpose,
                    v.started_at,
                    v.expires_at,
                    if holding { "this process sends a token" } else { "this process sends no token" }
                ),
                None => "no bench lease is live".to_string(),
            };
            Report {
                success: true,
                value: serde_json::json!({ "success": true, "lease": view, "this_process_sends_a_token": holding }),
                human,
            }
        }
        Err(e) => failed("lease_show", e),
    }
}
