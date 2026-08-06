//! Where is `embarch-core`, and how do we know?
//!
//! Design: ../embarch-doc/embarch-umbrella/design.md §3 decision 6.
//!
//! # MIRRORED MODULE — keep in sync with embarch-api
//!
//! `embarch-api` needs this exact logic for `base_url = "auto"`
//! (../embarch-doc/embarch-api/design.md §3.11, §7) and receives it as a
//! verbatim copy of this file plus its tests, rather than as a shared crate
//! (design.md §3 decision 15). Two rules follow, and breaking either one is
//! what turns "a copy" into "a fork":
//!
//! 1. **Nothing umbrella-specific crosses this module's boundary.** No CLI
//!    types, no config types, no HTTP client, no error type that carries
//!    umbrella's vocabulary. Inputs are plain data; outputs are plain data.
//! 2. **No I/O in here.** Reading `/proc/version`, running `ip route`, and
//!    making the HTTP request all live outside (`env.rs`, `probe.rs`), so the
//!    consumer supplies its own — `embarch-api` already has a `reqwest`
//!    client and shouldn't be handed a second one.
//!
//! Drift between the two copies is a real accepted risk. What makes it
//! visible rather than silent: this comment, the tests below (which port with
//! no adaptation), and `doctor` reporting *which* candidate won rather than
//! just pass/fail — so a divergence shows up as two different answers on one
//! machine instead of as a mystery.

use std::future::Future;

/// Core's default port. Overridable everywhere it's used; this is just the
/// value `embarch-core`'s own CLI defaults to.
pub const DEFAULT_CORE_PORT: u16 = 4884;

/// Which *kind* of place Core turned out to be — the only thing worth
/// persisting after detection. Deliberately not the resolved address: under
/// WSL2 that's a gateway IP that changes on every WSL restart, which is
/// exactly the staleness this whole mechanism exists to eliminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyClass {
    /// Reachable at loopback.
    Local,
    /// Reachable at the WSL2 default gateway, i.e. a Core running natively on
    /// the Windows host of this WSL2 guest.
    WslHost,
    /// A genuinely separate machine, named explicitly by the operator.
    Remote,
}

impl TopologyClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TopologyClass::Local => "local",
            TopologyClass::WslHost => "wsl-host",
            TopologyClass::Remote => "remote",
        }
    }
}

/// One place worth looking for Core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub class: TopologyClass,
    pub base_url: String,
}

/// What a single probe of one candidate found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Core is there. `authorized` distinguishes a `200` from a `401`, which
    /// is a distinction worth keeping all the way to the user: "Core isn't
    /// running" and "Core is running and rejected your token" have nothing to
    /// do with each other, and conflating them sends people to debug the
    /// wrong thing.
    Core { authorized: bool },
    /// Something is listening and speaking HTTP, but it isn't Core — a
    /// different service on the same port, most likely. Not a hit, but worth
    /// surfacing rather than reporting as "nothing there," since the fix
    /// (find out what's on port 4884) is completely different.
    NotCore { status: u16 },
    /// Nothing answered: connection refused, timed out, DNS failure.
    Unreachable,
}

/// One candidate and what probing it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub candidate: Candidate,
    pub outcome: ProbeOutcome,
}

/// Build the ordered list of places to look, cheapest and most likely first.
///
/// Order is load-bearing, not cosmetic. Loopback goes first because it covers
/// three topologies at once — Core native on this Mac/Linux/Windows box, *and*
/// WSL2 in mirrored-networking mode, where loopback already reaches the
/// Windows host. The gateway candidate is what covers WSL2's other (NAT)
/// networking mode. An explicitly configured host goes last: if the operator
/// named a machine, they still shouldn't be reached over the network for a
/// Core sitting on this one.
///
/// Duplicates are dropped, keeping the earliest occurrence — an operator who
/// sets `host = "127.0.0.1"` shouldn't cause the same URL to be probed twice.
pub fn candidates(
    under_wsl2: bool,
    gateway: Option<&str>,
    host: Option<&str>,
    port: u16,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |class, authority: String| {
        let base_url = format!("http://{authority}");
        if !out.iter().any(|c: &Candidate| c.base_url == base_url) {
            out.push(Candidate { class, base_url });
        }
    };

    push(TopologyClass::Local, format!("127.0.0.1:{port}"));

    if under_wsl2 {
        if let Some(gw) = gateway.map(str::trim).filter(|g| !g.is_empty()) {
            push(TopologyClass::WslHost, format!("{gw}:{port}"));
        }
    }

    if let Some(h) = host.map(str::trim).filter(|h| !h.is_empty()) {
        push(TopologyClass::Remote, format!("{h}:{port}"));
    }

    out
}

/// Map an HTTP status from `GET /status` onto what it says about Core.
///
/// `401` counts as finding Core, not as a miss: every one of Core's routes is
/// behind bearer-token auth, so an unauthenticated probe reaching a healthy
/// Core gets exactly this. Anything else that answers is some other service.
pub fn classify_status(status: u16) -> ProbeOutcome {
    match status {
        200 => ProbeOutcome::Core { authorized: true },
        401 => ProbeOutcome::Core { authorized: false },
        other => ProbeOutcome::NotCore { status: other },
    }
}

/// Probe candidates in order, stopping at the first one that is Core.
///
/// Ordered and sequential rather than concurrent, despite "race": ordering is
/// the point (§`candidates`), and the common miss — nothing listening — is a
/// connection refusal that returns immediately rather than burning the
/// timeout. A timeout is only paid when packets are silently dropped, which
/// is the uncommon case and at most one or two candidates deep.
///
/// Returns every attempt made, in order, so a caller can report what it tried
/// and not just what it found. `probe` supplies the actual I/O.
///
/// `probe` takes an owned `String` rather than a `&str` deliberately: a
/// borrowed argument would need the returned future to carry that borrow's
/// lifetime, which a plain `Fn(&str) -> Fut` bound can't express (the future
/// type is fixed, the borrow isn't). Cloning at most three short URLs is not
/// a cost worth a higher-ranked-trait-bound workaround.
pub async fn resolve<F, Fut>(candidates: &[Candidate], probe: F) -> Vec<Attempt>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = ProbeOutcome>,
{
    let mut attempts = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let outcome = probe(candidate.base_url.clone()).await;
        let found_core = matches!(outcome, ProbeOutcome::Core { .. });
        attempts.push(Attempt {
            candidate: candidate.clone(),
            outcome,
        });
        if found_core {
            break;
        }
    }
    attempts
}

/// The attempt that found Core, if any.
pub fn winner(attempts: &[Attempt]) -> Option<&Attempt> {
    attempts
        .iter()
        .find(|a| matches!(a.outcome, ProbeOutcome::Core { .. }))
}

/// Are we running inside a WSL2 guest?
///
/// Two independent signals, either of which is enough: the kernel release
/// string (WSL2's kernel is Microsoft-built and says so) and the environment
/// variable WSL itself sets. Neither alone is airtight — `WSL_DISTRO_NAME`
/// can be inherited into a context that isn't really WSL, and a custom kernel
/// might not carry the vendor string — so this takes either.
pub fn detect_wsl2(proc_version: Option<&str>, wsl_distro_env: Option<&str>) -> bool {
    let kernel_says_so = proc_version
        .map(|v| v.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false);
    let env_says_so = wsl_distro_env.map(|v| !v.is_empty()).unwrap_or(false);
    kernel_says_so || env_says_so
}

/// Pull the gateway address out of `ip route show default` output.
///
/// Parses the `via <addr>` form specifically. A default route with no `via`
/// (a point-to-point link) yields nothing, which is correct — there's no
/// host address to talk to in that case.
pub fn parse_default_gateway(ip_route_output: &str) -> Option<String> {
    ip_route_output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != "default" {
            return None;
        }
        let mut fields = fields.skip_while(|f| *f != "via");
        fields.next()?; // the "via" itself
        fields.next().map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.base_url.as_str()).collect()
    }

    #[test]
    fn plain_machine_gets_loopback_only() {
        let c = candidates(false, None, None, 4884);
        assert_eq!(urls(&c), ["http://127.0.0.1:4884"]);
        assert_eq!(c[0].class, TopologyClass::Local);
    }

    #[test]
    fn gateway_is_ignored_when_not_under_wsl2() {
        // A Linux box with a default route is not a WSL2 guest; probing its
        // gateway for a Core would be probing the router.
        let c = candidates(false, Some("192.168.1.1"), None, 4884);
        assert_eq!(urls(&c), ["http://127.0.0.1:4884"]);
    }

    #[test]
    fn wsl2_adds_the_gateway_after_loopback() {
        let c = candidates(true, Some("172.22.128.1"), None, 4884);
        assert_eq!(
            urls(&c),
            ["http://127.0.0.1:4884", "http://172.22.128.1:4884"]
        );
        assert_eq!(c[1].class, TopologyClass::WslHost);
    }

    #[test]
    fn wsl2_without_a_gateway_still_tries_loopback() {
        // Mirrored networking, or a guest with no default route at all.
        assert_eq!(urls(&candidates(true, None, None, 4884)).len(), 1);
        assert_eq!(urls(&candidates(true, Some("   "), None, 4884)).len(), 1);
    }

    #[test]
    fn explicit_host_goes_last() {
        let c = candidates(true, Some("172.22.128.1"), Some("bench.local"), 4884);
        assert_eq!(
            urls(&c),
            [
                "http://127.0.0.1:4884",
                "http://172.22.128.1:4884",
                "http://bench.local:4884"
            ]
        );
        assert_eq!(c[2].class, TopologyClass::Remote);
    }

    #[test]
    fn duplicate_urls_are_dropped_keeping_the_earliest() {
        let c = candidates(true, Some("127.0.0.1"), Some("127.0.0.1"), 4884);
        assert_eq!(urls(&c), ["http://127.0.0.1:4884"]);
        assert_eq!(c[0].class, TopologyClass::Local);
    }

    #[test]
    fn port_is_honored() {
        let c = candidates(false, None, None, 9999);
        assert_eq!(urls(&c), ["http://127.0.0.1:9999"]);
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify_status(200), ProbeOutcome::Core { authorized: true });
        // The one that matters: Core is there, the token isn't right.
        assert_eq!(
            classify_status(401),
            ProbeOutcome::Core { authorized: false }
        );
        assert_eq!(classify_status(404), ProbeOutcome::NotCore { status: 404 });
        assert_eq!(classify_status(500), ProbeOutcome::NotCore { status: 500 });
    }

    #[tokio::test]
    async fn resolve_stops_at_the_first_core() {
        let c = candidates(true, Some("172.22.128.1"), Some("bench.local"), 4884);
        let attempts = resolve(&c, |url| async move {
            match url.as_str() {
                "http://127.0.0.1:4884" => ProbeOutcome::Unreachable,
                _ => ProbeOutcome::Core { authorized: false },
            }
        })
        .await;

        assert_eq!(attempts.len(), 2, "must not probe past the first hit");
        let w = winner(&attempts).expect("gateway should have won");
        assert_eq!(w.candidate.class, TopologyClass::WslHost);
        assert_eq!(w.outcome, ProbeOutcome::Core { authorized: false });
    }

    #[tokio::test]
    async fn a_non_core_service_does_not_win_but_is_recorded() {
        let c = candidates(true, Some("172.22.128.1"), None, 4884);
        let attempts = resolve(&c, |url| async move {
            match url.as_str() {
                "http://127.0.0.1:4884" => ProbeOutcome::NotCore { status: 404 },
                _ => ProbeOutcome::Core { authorized: true },
            }
        })
        .await;

        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].outcome, ProbeOutcome::NotCore { status: 404 });
        assert_eq!(winner(&attempts).unwrap().candidate.class, TopologyClass::WslHost);
    }

    #[tokio::test]
    async fn nothing_anywhere_reports_every_attempt() {
        let c = candidates(true, Some("172.22.128.1"), Some("bench.local"), 4884);
        let attempts = resolve(&c, |_| async { ProbeOutcome::Unreachable }).await;
        assert_eq!(attempts.len(), 3);
        assert!(winner(&attempts).is_none());
    }

    #[test]
    fn wsl2_detection() {
        let wsl_kernel = "Linux version 6.6.87.2-microsoft-standard-WSL2 (gcc ...)";
        assert!(detect_wsl2(Some(wsl_kernel), None));
        assert!(detect_wsl2(Some("Linux version 6.6.87.2-MICROSOFT"), None));
        assert!(detect_wsl2(None, Some("Ubuntu-24.04")));
        assert!(!detect_wsl2(Some("Linux version 6.8.0-45-generic"), None));
        assert!(!detect_wsl2(None, None));
        assert!(!detect_wsl2(None, Some("")));
    }

    #[test]
    fn gateway_parsing() {
        assert_eq!(
            parse_default_gateway("default via 172.22.128.1 dev eth0 proto kernel"),
            Some("172.22.128.1".to_string())
        );
        // A non-default route ahead of the default one must not match.
        assert_eq!(
            parse_default_gateway(
                "10.0.0.0/8 via 10.1.2.3 dev eth1\ndefault via 192.168.0.1 dev eth0\n"
            ),
            Some("192.168.0.1".to_string())
        );
        assert_eq!(parse_default_gateway(""), None);
        assert_eq!(parse_default_gateway("default dev ppp0 scope link"), None);
        assert_eq!(parse_default_gateway("172.22.128.0/20 dev eth0"), None);
    }
}
