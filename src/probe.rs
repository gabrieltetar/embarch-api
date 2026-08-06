//! The one piece of I/O behind topology detection: asking a candidate
//! address whether it's `embarch-core`.
//!
//! Deliberately *not* in `topology.rs` — that module is copied verbatim into
//! `embarch-api`, which already owns a configured `reqwest::Client` and
//! shouldn't be handed a second one (topology.rs's mirrored-module rules).

use std::time::Duration;

use crate::topology::{classify_status, ProbeOutcome};

/// Per-candidate budget.
///
/// Short on purpose. The common miss is "nothing is listening," which comes
/// back as an immediate connection refusal and never touches this timeout —
/// it only bites when packets are silently dropped (a firewall dropping
/// rather than rejecting), and in that case we'd rather move on to the next
/// candidate than stall a `status` call.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Unauthenticated `GET {base_url}/status`.
///
/// No token is sent, and none is needed: this asks "is Core there," not "may
/// I use it." A healthy Core answers `401`, which `classify_status` treats as
/// a hit (Core found, not authorized).
pub async fn probe_core(client: &reqwest::Client, base_url: &str) -> ProbeOutcome {
    let url = format!("{}/status", base_url.trim_end_matches('/'));
    match client.get(&url).timeout(PROBE_TIMEOUT).send().await {
        Ok(response) => classify_status(response.status().as_u16()),
        Err(_) => ProbeOutcome::Unreachable,
    }
}
