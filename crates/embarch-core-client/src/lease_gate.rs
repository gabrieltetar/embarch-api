//! What a session made to hold the bench lease is refused (`embarch-api`
//! decision 101): the switch, and the routes it covers.
//!
//! Core gates nothing while no lease is live (`embarch-core` decision 93), so
//! "this session must hold one" can only be enforced here. Core stays the
//! authority whenever a lease is live; this adds the one case it admits.

/// Set in a session's environment, makes `embarch-api` refuse a hardware route
/// while no bench lease is live. Empty or `0` is off.
pub const REQUIRE_LEASE_ENV: &str = "EMBARCH_REQUIRE_LEASE";

/// A copy of `embarch-core`'s `lease::LEASE_GATED`, compared with it by the
/// test below whenever the sibling checkout is there. A route missing here is
/// not refused locally, but Core still gates it while a lease is live.
pub const LEASE_GATED: &[(&str, &str)] = &[
    ("POST", "/flash"),
    ("POST", "/reset"),
    ("POST", "/bootload"),
    ("PUT", "/bootload/ports"),
    ("DELETE", "/bootload/ports"),
    ("GET", "/serial-log"),
    ("GET", "/dev-bench/hello"),
    ("POST", "/probes/enroll"),
    ("DELETE", "/probes/enrolled/{role}"),
    ("PUT", "/probes/enrolled/{role}/board"),
    ("DELETE", "/probes/by-serial/{serial}"),
    ("POST", "/dev-bench/link"),
    ("POST", "/signals"),
    ("DELETE", "/signals/{name}"),
    ("POST", "/signals/{name}/exchange"),
    ("POST", "/signals/{name}/capture"),
    ("DELETE", "/signals/{name}/capture"),
    ("PUT", "/instruments"),
    ("DELETE", "/instruments/{name}"),
    ("POST", "/instruments/{name}/exchange"),
    ("POST", "/live/mem-read"),
    ("POST", "/live/core-state"),
    ("POST", "/validate"),
    ("POST", "/study"),
    ("POST", "/host-studies"),
    ("POST", "/study/{study_id}/cancel"),
];

/// Whether [`REQUIRE_LEASE_ENV`] is on in this process's environment.
pub fn required_by_env() -> bool {
    std::env::var(REQUIRE_LEASE_ENV).is_ok_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0"
    })
}

/// Whether a request's method and URL path are a route in [`LEASE_GATED`].
pub fn is_gated(method: &str, path: &str) -> bool {
    LEASE_GATED.iter().any(|(m, pattern)| *m == method && path_matches(pattern, path))
}

/// `{name}` in a pattern matches one non-empty path segment.
fn path_matches(pattern: &str, path: &str) -> bool {
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) if (w.starts_with('{') && !g.is_empty()) || w == g => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_real_paths_of_its_own_method_only() {
        assert!(is_gated("POST", "/flash"));
        assert!(is_gated("POST", "/signals/ppg/exchange"));
        assert!(is_gated("POST", "/signals/a%2Fb/exchange"));
        assert!(is_gated("DELETE", "/signals/ppg/capture"));
        assert!(is_gated("PUT", "/probes/enrolled/dut/board"));
        assert!(is_gated("POST", "/study/abc/cancel"));

        assert!(!is_gated("GET", "/signals/ppg/capture"));
        assert!(!is_gated("GET", "/study/abc"));
        assert!(!is_gated("POST", "/study/abc/prompt"));
        assert!(!is_gated("GET", "/bootload/ports"));
        assert!(!is_gated("GET", "/status"));
        assert!(!is_gated("POST", "/lease"));
        assert!(!is_gated("POST", "/signals//exchange"));
        assert!(!is_gated("POST", "/signals/ppg/exchange/more"));
        assert!(!is_gated("POST", "/signals/ppg"));
    }

    /// Fails when Core's gated set moves and this copy did not. A checkout with
    /// no `embarch-core` beside it has nothing to compare with, so it passes.
    #[test]
    fn the_copy_matches_embarch_core_when_the_sibling_is_there() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../embarch-core/src/lease.rs");
        let Ok(source) = std::fs::read_to_string(path) else {
            eprintln!("no {path}: nothing to compare the gated routes with");
            return;
        };
        let start = source.find("pub const LEASE_GATED").expect("Core's lease.rs names LEASE_GATED");
        let rest = &source[start..];
        let table = &rest[..rest.find("\n];").expect("LEASE_GATED ends with a closing bracket")];
        let core: Vec<(&str, &str)> = table
            .lines()
            .filter(|line| line.trim_start().starts_with("(\""))
            .map(|line| {
                let parts: Vec<&str> = line.split('"').collect();
                (parts[1], parts[3])
            })
            .collect();
        assert_eq!(core, LEASE_GATED, "update this copy of embarch-core's lease::LEASE_GATED");
    }
}
