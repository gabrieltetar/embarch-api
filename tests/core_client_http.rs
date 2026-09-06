//! The mocked HTTP suite `embarch-api/open.md` specified and nothing wrote.
//!
//! Three of the six recorded acceptance criteria are about how this process
//! talks to embarch-core over HTTP, and all three are properties of
//! `embarch-core-client`'s `CoreClient`:
//!
//! 1. bearer token injection on **every** outbound call,
//! 2. per-endpoint timeout independence,
//! 3. a plain-text body surfaced on a non-2xx response.
//!
//! The other three are properties of `embarch_api::build` and live in
//! `tests/build_capture.rs`.
//!
//! Everything here runs against `support::MockCore` — a loopback socket, not
//! a live Core — so the suite is hardware-free and deterministic. See
//! `tests/support/mod.rs` for why the mock is hand-rolled rather than a
//! mock-HTTP dependency.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use embarch_core_client::{
    CoreClient, CoreConfig, SignalDirection, SignalLink, SignalRoute, StudyRunOptions,
};
use embarch_study_designer::Study;
use serde_json::json;
use support::{Behavior, MockCore};

/// Distinctive enough that finding it anywhere unexpected (a query string,
/// say) is unambiguous.
const TOKEN: &str = "mocked-core-token-3f9a1c";

/// `CoreConfig` is `Deserialize`-only by design, so tests build one the same
/// way the real config loader does. Going through `serde` rather than a
/// struct literal also means a newly added field with a `#[serde(default)]`
/// does not break this file, and a newly added *required* one does — which
/// is the right way round.
fn config(overrides: serde_json::Value) -> CoreConfig {
    let mut value = json!({ "token": TOKEN });
    let (serde_json::Value::Object(map), serde_json::Value::Object(extra)) =
        (&mut value, overrides)
    else {
        panic!("config overrides must be a JSON object");
    };
    map.extend(extra);
    serde_json::from_value(value).expect("test CoreConfig did not deserialize")
}

// ---------------------------------------------------------------------------
// Criterion 1 — bearer token injection on every outbound call
// ---------------------------------------------------------------------------

/// Every method on `CoreClient` that reaches the network must send
/// `Authorization: Bearer <token>`.
///
/// The mock answers `503` with a plain-text body to everything but
/// `GET /status`, so almost no call gets far enough to need a well-formed
/// response body and none of them can take a status-specific shortcut (a
/// `404`, notably, is a meaningful non-error for
/// `dev_bench_port`/`study_streams`/`study_steps` and would have exercised a
/// different branch). Nearly every call therefore fails, and failing is fine:
/// what is under test is the request that went out, not the answer that came
/// back.
///
/// The route list is the point of the test, and its exhaustiveness is
/// **enforced, not intended**: `the_sweep_calls_every_networked_method` reads
/// the client's source, derives the set of methods that build an outbound
/// request, and fails if this function does not call each of them. Add a
/// networked method to `CoreClient` and that test goes red naming it; add the
/// call here and this one proves it sends `Authorization: Bearer …`.
#[tokio::test]
async fn every_outbound_call_carries_the_bearer_token() {
    // Everything is refused, with one exception: `post_study` reads Core's
    // schema version off `GET /status` before it will submit, so a 503 there
    // would stop it short of the request this sweep exists to inspect. No
    // other call reads a 200 body it did not ask for, and no other route is
    // answered — a 404, notably, stays meaningful-and-unreachable for
    // `dev_bench_port`/`study_streams`/`study_steps`.
    let mock = MockCore::start(Behavior::Router {
        routes: vec![(
            "/status".to_string(),
            Behavior::json_ok(json!({
                "status": "ok",
                "probes": [],
                "study_designer_schema_version":
                    embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
            })),
        )],
        otherwise: Box::new(Behavior::plain_text_error(
            503,
            "Service Unavailable",
            "the mock is refusing everything on purpose",
        )),
    })
    .await;
    let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
        .expect("client did not build");

    let signal = SignalLink {
        name: "outpost".to_string(),
        origin_role: "dut".to_string(),
        direction: SignalDirection::DutToHost,
        route: SignalRoute::Direct {
            port_serial: "MOCK-BRIDGE-0001".to_string(),
        },
    };

    // Each of these is exactly one HTTP request; none retries.
    let _ = client.status().await;
    let _ = client.alerts(3).await;
    let _ = client.list_enrolled().await;
    let _ = client.list_signals().await;
    let _ = client.list_serial_ports().await;
    let _ = client.logs_recent(5).await;
    let _ = client.dev_bench_port().await;
    let _ = client.dev_bench_hello().await;
    let _ = client.validate("dut").await;
    let _ = client.reset("nRF52840_xxAA", None).await;
    let _ = client.enroll_probe("dut", "nRF52840_xxAA", None).await;
    let _ = client.resolve_chip("nrf52840").await;
    let _ = client.serial_log("COM7", 115_200, 250).await;
    let _ = client.declare_signal(&signal).await;
    let _ = client.remove_signal("outpost").await;
    let _ = client.get_study_status("study-1").await;
    let _ = client.study_streams("study-1").await;
    let _ = client.study_steps("study-1").await;
    let _ = client.get_study_power_data("study-1").await;
    let _ = client.get_study_waveform_data("study-1").await;
    let _ = client.get_study_gatt_data("study-1").await;
    let _ = client.get_study_stream("study-1", "ppg", true).await;
    // A declared `base_url` resolves as `TopologyClass::Local`, so `flash`
    // takes its send-a-path branch and needs no artifact on disk.
    let _ = client
        .flash("nRF52840_xxAA", "/nonexistent/app.hex", "hex", None, None, false)
        .await;
    // Two requests, not one: `post_study` reads `GET /status` for Core's
    // schema version first, and refuses to submit across a mismatch — which
    // is why the mock answers that one route.
    let _ = client.post_study(&self_test_study(), &StudyRunOptions::default()).await;
    // The SSE stream is the one route that does not go through the client's
    // `send` helper at all (it streams a body and sets no request timeout),
    // so it is exactly the shape that escapes a hand-kept list — and did.
    let _ = client
        .open_study_events("study-1", Duration::from_millis(250))
        .await;

    let requests = mock.requests();
    assert!(
        !requests.is_empty(),
        "the mock recorded no requests at all — the sweep never reached the network"
    );

    let expected = format!("Bearer {TOKEN}");
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some(expected.as_str()),
            "{} {} went out without the bearer token",
            request.method,
            request.target,
        );
        assert!(
            !request.target.contains(TOKEN),
            "{} {} leaked the token into the request target",
            request.method,
            request.target,
        );
    }

    // Every listed call must actually have hit the wire, and nothing may hit
    // it that this list does not name. The first direction catches a method
    // that silently stopped issuing a request — it would still pass the
    // header assertion above, because there would be nothing to check. The
    // second catches a call added to the sweep whose route was never added
    // here, which would otherwise leave this list quietly incomplete again.
    let observed: BTreeSet<(String, String)> = requests.iter().map(|r| r.route()).collect();
    let expected: BTreeSet<(String, String)> = [
        ("GET", "/status"),
        ("GET", "/alerts"),
        ("GET", "/probes/enrolled"),
        ("GET", "/signals"),
        ("GET", "/serial-ports"),
        ("GET", "/logs/recent"),
        ("GET", "/dev-bench/port"),
        ("GET", "/dev-bench/hello"),
        ("POST", "/validate"),
        ("POST", "/reset"),
        ("POST", "/probes/enroll"),
        ("POST", "/resolve-chip"),
        ("GET", "/serial-log"),
        ("POST", "/signals"),
        ("DELETE", "/signals/outpost"),
        ("GET", "/study/study-1"),
        ("GET", "/study/study-1/streams"),
        ("GET", "/study/study-1/steps"),
        ("GET", "/study/study-1/power-data"),
        ("GET", "/study/study-1/waveform-data"),
        ("GET", "/study/study-1/gatt-data"),
        ("GET", "/study/study-1/stream/ppg"),
        ("POST", "/flash"),
        ("POST", "/study"),
        ("GET", "/study/study-1/events"),
    ]
    .into_iter()
    .map(|(method, path)| (method.to_string(), path.to_string()))
    .collect();

    let missing: Vec<_> = expected.difference(&observed).collect();
    assert!(
        missing.is_empty(),
        "these routes never reached the mock: {missing:?}"
    );
    let unlisted: Vec<_> = observed.difference(&expected).collect();
    assert!(
        unlisted.is_empty(),
        "the mock saw routes this list does not name: {unlisted:?} — a call was added to \
         the sweep without adding the route it emits here"
    );
}

/// The committed dev-bench self-test study, which `src/study.rs` already
/// round-trips through this same schema. Read from disk rather than built
/// inline: `Study` is a large, deeply nested type and the sweep needs a
/// valid one only so `post_study` has something to send.
fn self_test_study() -> Study {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/self_test_study.json"
    ))
    .expect("the self-test study fixture should be readable");
    serde_json::from_str(&raw).expect("the self-test study fixture should match Study's schema")
}

/// The token comes from `[core].token_env` in preference to an inline
/// `token`, and whichever wins is the one that ends up in the header —
/// resolution and injection are one property from a caller's point of view,
/// and a client that resolved correctly then sent the other value would pass
/// the sweep above.
#[tokio::test]
async fn the_resolved_token_is_the_one_that_is_sent() {
    const VAR: &str = "EMBARCH_TEST_TOKEN_FOR_INJECTION";
    // Scoped to this process; no other test in this binary reads this var.
    std::env::set_var(VAR, "token-from-the-environment");

    let mock = MockCore::start(Behavior::plain_text_error(503, "Service Unavailable", "nope")).await;
    let client = CoreClient::new(&config(json!({
        "base_url": mock.base_url(),
        "token_env": VAR,
    })))
    .expect("client did not build");

    let _ = client.status().await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer token-from-the-environment"),
        "the inline `token` was sent even though `token_env` resolved"
    );
}

// ---------------------------------------------------------------------------
// The sweep's own exhaustiveness
// ---------------------------------------------------------------------------

/// Where `CoreClient` is implemented. Two files: `client.rs` and the SSE half
/// in `study_events.rs`.
const CLIENT_SRC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/crates/embarch-core-client/src"
);

/// This file. The set of methods the sweep covers is read off its real call
/// sites, so there is no second list to keep in step with the first.
const SWEEP_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/core_client_http.rs");

const SWEEP_FN: &str = "every_outbound_call_carries_the_bearer_token";

/// The `reqwest::Client` verbs an outbound request can be born from.
const VERBS: [&str; 7] = ["get", "post", "put", "patch", "delete", "head", "request"];

/// `every_outbound_call_carries_the_bearer_token` must call every method on
/// `CoreClient` that reaches the network.
///
/// Until 2026-09-06 that was a doc comment asking the next author to
/// remember, and it had already failed twice over: `post_study` and
/// `open_study_events` both build outbound requests and neither was swept.
/// A comment describing a gap does not close one.
///
/// So the set is derived from the source instead. Every request in this
/// client is born from `self.client.<verb>(…)` or from the one `http()`
/// escape hatch `study_events` uses, both of which are greppable; the method
/// enclosing each such call site is the networked surface, expanded through
/// private helpers (`get_study_csv`) to the public methods that reach them.
/// The sweep's own `client.<method>(…)` calls are read the same way. The two
/// sets must match exactly.
///
/// **What this does not cover.** It is a lexical scan, not a Rust parse: a
/// request built through some third route — a second `reqwest::Client`, or
/// `http()` bound to a local first — would be invisible to it, so both of
/// those are asserted against directly below. It also says nothing about
/// whether a swept call carries the token; that is the sweep's job, and this
/// test only guarantees the sweep is asked the question.
#[test]
fn the_sweep_calls_every_networked_method() {
    let sources = client_sources();

    // One `reqwest::Client` in the whole crate, so `self.client` and the
    // `http()` accessor over it are the only two roots a request grows from.
    let clients_built = sources
        .iter()
        .flat_map(|(_, lines)| lines.iter())
        .filter(|(_, line)| {
            line.contains("reqwest::Client::builder(") || line.contains("reqwest::Client::new(")
        })
        .count();
    assert_eq!(
        clients_built, 1,
        "this scan assumes `CoreClient` owns exactly one `reqwest::Client`; it found \
         {clients_built}, so there is a networked surface it cannot see"
    );

    // And the escape hatch is only ever used to build a request on the spot,
    // never bound to a local this scan would then lose track of.
    for (file, lines) in &sources {
        for (number, line) in lines {
            assert!(
                !line.contains("self.http()") || builds_a_request(line),
                "{file}:{number} takes `self.http()` without building a request in the same \
                 expression; this scan can no longer tell which method reaches the network"
            );
        }
    }

    let networked = networked_methods(&sources);
    let swept = swept_methods();

    let unswept: Vec<&String> = networked.difference(&swept).collect();
    assert!(
        unswept.is_empty(),
        "these `CoreClient` methods build an outbound request and `{SWEEP_FN}` never calls \
         them: {unswept:?}. Add a call for each — the sweep is the only thing that proves a \
         route sends `Authorization: Bearer …`."
    );

    let stale: Vec<&String> = swept.difference(&networked).collect();
    assert!(
        stale.is_empty(),
        "`{SWEEP_FN}` calls these, but no outbound request was found anywhere in their \
         source: {stale:?}. Either they stopped reaching the network, or this scan's idea \
         of how a request is built has gone stale."
    );
}

/// Every `.rs` under the client's `src/`, as logical lines.
fn client_sources() -> Vec<(String, Vec<(usize, String)>)> {
    let entries = std::fs::read_dir(CLIENT_SRC)
        .unwrap_or_else(|error| panic!("could not read {CLIENT_SRC}: {error}"));
    let mut sources: Vec<(String, Vec<(usize, String)>)> = entries
        .map(|entry| entry.expect("could not read a directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
            (path.display().to_string(), logical_lines(&text))
        })
        .collect();
    assert!(!sources.is_empty(), "found no client source to scan under {CLIENT_SRC}");
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

/// Drops blank and comment lines, and glues a method-chain continuation onto
/// the line it continues, so a call split across five lines reads as one.
fn logical_lines(source: &str) -> Vec<(usize, String)> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        match lines.last_mut() {
            Some((_, previous)) if trimmed.starts_with('.') => previous.push_str(trimmed),
            _ => lines.push((index + 1, trimmed.to_string())),
        }
    }
    lines
}

/// `(name, is_public)` if this line declares a function. Only `pub` counts as
/// public — `pub(crate)` is not callable from an integration test and so is
/// not part of the surface the sweep can reach.
fn declared_fn(line: &str) -> Option<(String, bool)> {
    let at = line.find("fn ")?;
    let prefix = &line[..at];
    if !prefix.split_whitespace().all(|word| {
        matches!(word, "pub" | "pub(crate)" | "pub(super)" | "async" | "const" | "unsafe")
    }) {
        return None;
    }
    let rest = &line[at + 3..];
    let end = rest
        .find(|character: char| !character.is_alphanumeric() && character != '_')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), prefix.split_whitespace().any(|word| word == "pub")))
}

fn builds_a_request(line: &str) -> bool {
    VERBS.iter().any(|verb| {
        line.contains(&format!("self.client.{verb}(")) || line.contains(&format!("self.http().{verb}("))
    })
}

/// Names in `<receiver>.<name>(` on this line.
fn calls_on(line: &str, receiver: &str) -> Vec<String> {
    let needle = format!("{receiver}.");
    let mut names = Vec::new();
    for (at, _) in line.match_indices(&needle) {
        if at > 0 {
            let before = line[..at].chars().next_back().unwrap_or(' ');
            if before.is_alphanumeric() || before == '_' || before == '.' || before == ':' {
                continue;
            }
        }
        let rest = &line[at + needle.len()..];
        let end = rest
            .find(|character: char| !character.is_alphanumeric() && character != '_')
            .unwrap_or(rest.len());
        if end > 0 && rest[end..].starts_with('(') {
            names.push(rest[..end].to_string());
        }
    }
    names
}

/// The public methods that reach the network, derived from the source.
fn networked_methods(sources: &[(String, Vec<(usize, String)>)]) -> BTreeSet<String> {
    let mut public: BTreeMap<String, bool> = BTreeMap::new();
    let mut builders: BTreeSet<String> = BTreeSet::new();
    let mut callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (_, lines) in sources {
        let mut enclosing: Option<String> = None;
        for (_, line) in lines {
            if let Some((name, is_public)) = declared_fn(line) {
                public.insert(name.clone(), is_public);
                enclosing = Some(name);
            }
            let Some(enclosing) = enclosing.as_ref() else {
                continue;
            };
            if builds_a_request(line) {
                builders.insert(enclosing.clone());
            }
            for callee in calls_on(line, "self") {
                callers.entry(callee).or_default().insert(enclosing.clone());
            }
        }
    }

    // A private request builder is not itself reachable from a test, so it
    // stands in for whatever public methods call it.
    let mut networked: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<String> = builders.into_iter().collect();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if public.get(&name).copied().unwrap_or(false) {
            networked.insert(name);
            continue;
        }
        let reached: Vec<String> = callers.get(&name).cloned().unwrap_or_default().into_iter().collect();
        assert!(
            !reached.is_empty(),
            "`{name}` builds an outbound request, is not public, and nothing calls it — this \
             scan cannot say which swept method reaches it"
        );
        pending.extend(reached);
    }
    networked
}

/// The methods `SWEEP_FN` actually calls, read off its body.
fn swept_methods() -> BTreeSet<String> {
    let source = std::fs::read_to_string(SWEEP_FILE).unwrap_or_else(|error| {
        panic!(
            "could not read {SWEEP_FILE}: {error} — this test reads the sweep's own call \
             sites, so it has to be able to find this file"
        )
    });
    let mut inside = false;
    let mut swept: BTreeSet<String> = BTreeSet::new();
    for (_, line) in logical_lines(&source) {
        if let Some((name, _)) = declared_fn(&line) {
            inside = name == SWEEP_FN;
            continue;
        }
        if inside {
            swept.extend(calls_on(&line, "client"));
        }
    }
    assert!(
        !swept.is_empty(),
        "found no calls at all inside `{SWEEP_FN}` — this scan has lost track of the sweep"
    );
    swept
}

// ---------------------------------------------------------------------------
// Criterion 2 — per-endpoint timeout independence
// ---------------------------------------------------------------------------

/// `[core]` carries five separate timeout knobs, and each endpoint family
/// must be governed by its own.
///
/// The mock accepts the connection and then goes silent, so the only thing
/// that can end any of these calls is the client's own per-request timeout.
/// `status` (1s) and `reset` (2s) are given short knobs and are expected to
/// return at roughly those times; `serial_log` and the study CSV endpoints
/// are given a 30s knob and are expected to be *still waiting* when the test
/// stops caring at 4s.
///
/// Both directions matter, which is why the test asserts on both ends:
/// collapse every endpoint onto `status_timeout` and `reset`/`serial_log`
/// finish far too early; collapse them onto `serial_timeout` and `status`
/// never returns inside the window.
#[tokio::test]
async fn each_endpoint_family_waits_on_its_own_timeout() {
    let mock = MockCore::start(Behavior::BlackHole).await;
    let client = CoreClient::new(&config(json!({
        "base_url": mock.base_url(),
        "status_timeout_secs": 1,
        "reset_timeout_secs": 2,
        "serial_timeout_secs": 30,
        "study_timeout_secs": 30,
        "flash_timeout_secs": 30,
    })))
    .expect("client did not build");

    /// How long the two long-knob calls are given to prove they are *not*
    /// governed by the short ones. Comfortably past `reset`'s 2s and
    /// comfortably short of the 30s they were configured with.
    const PATIENCE: Duration = Duration::from_secs(4);

    let status = async {
        let started = Instant::now();
        let outcome = client.status().await;
        (outcome.is_err(), started.elapsed())
    };
    let reset = async {
        let started = Instant::now();
        let outcome = client.reset("nRF52840_xxAA", None).await;
        (outcome.is_err(), started.elapsed())
    };
    let serial = tokio::time::timeout(PATIENCE, client.serial_log("COM7", 115_200, 250));
    let study = tokio::time::timeout(PATIENCE, client.get_study_power_data("study-1"));

    let ((status_failed, status_took), (reset_failed, reset_took), serial, study) =
        tokio::join!(status, reset, serial, study);

    assert!(status_failed, "the black-hole mock somehow answered /status");
    assert!(reset_failed, "the black-hole mock somehow answered /reset");

    // Lower bounds are what catch a shorter knob leaking in; upper bounds are
    // what catch a longer one. Both are loose enough for a loaded machine and
    // far tighter than the gap between any two configured values.
    assert!(
        (Duration::from_millis(700)..Duration::from_millis(1_900)).contains(&status_took),
        "/status has a 1s timeout but gave up after {status_took:?}"
    );
    assert!(
        (Duration::from_millis(1_700)..Duration::from_millis(3_600)).contains(&reset_took),
        "/reset has a 2s timeout but gave up after {reset_took:?}"
    );
    assert!(
        serial.is_err(),
        "/serial-log has a 30s timeout but gave up inside {PATIENCE:?} — it is sharing a shorter knob"
    );
    assert!(
        study.is_err(),
        "/study/*/power-data has a 30s timeout but gave up inside {PATIENCE:?} — it is sharing a shorter knob"
    );
}

// ---------------------------------------------------------------------------
// Criterion 3 — plain-text body surfaced on a non-2xx response
// ---------------------------------------------------------------------------

/// Core's error responses are `text/plain` (axum's `IntoResponse` for
/// `(StatusCode, String)`), while every success body this client reads is
/// JSON. A client that parsed the body as JSON regardless would replace
/// Core's actual message — the only useful part — with a parse error.
#[tokio::test]
async fn a_plain_text_error_body_reaches_the_caller() {
    const MESSAGE: &str =
        "probe MOCK-BRIDGE-0001 is busy: another study holds the hardware lock";

    let mock = MockCore::start(Behavior::plain_text_error(409, "Conflict", MESSAGE)).await;
    let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
        .expect("client did not build");

    let error = client
        .status()
        .await
        .expect_err("a 409 was reported as success")
        .to_string();

    assert!(error.contains(MESSAGE), "Core's own message was lost: {error}");
    assert!(error.contains("409"), "the status code was lost: {error}");
    assert!(
        !error.contains("failed to parse"),
        "the body was run through the JSON parser instead of being read as text: {error}"
    );
}

/// The 204-expecting path (`declare_signal`) reads its non-2xx body the same
/// way. It has its own send helper, so it can regress independently.
#[tokio::test]
async fn a_no_content_endpoint_surfaces_its_plain_text_error_too() {
    const MESSAGE: &str = "signal 'outpost' names a port Core cannot see";

    let mock = MockCore::start(Behavior::plain_text_error(400, "Bad Request", MESSAGE)).await;
    let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
        .expect("client did not build");

    let error = client
        .declare_signal(&SignalLink {
            name: "outpost".to_string(),
            origin_role: "dut".to_string(),
            direction: SignalDirection::DutToHost,
            route: SignalRoute::ViaDevBench {
                rx_pin: "P1.04".to_string(),
                tx_pin: "P1.05".to_string(),
            },
        })
        .await
        .expect_err("a 400 was reported as success")
        .to_string();

    assert!(error.contains(MESSAGE), "Core's own message was lost: {error}");
    assert!(error.contains("400"), "the status code was lost: {error}");
}

/// The `/study/*` endpoints try Core's structured `{code, message, cause}`
/// error shape first and fall back to raw text. Both halves of that fallback
/// are pinned here: a body that is not JSON at all, and a body that *is*
/// JSON but not that shape — the second is the one a careless
/// `serde_json::from_str(..).unwrap_or_default()` would silently blank out.
#[tokio::test]
async fn a_study_endpoint_falls_back_to_the_raw_body() {
    for body in [
        "study-1 was aborted: dev-bench reset mid-frame",
        r#"{"unexpected":true,"detail":"not Core's error shape"}"#,
    ] {
        let mock =
            MockCore::start(Behavior::plain_text_error(500, "Internal Server Error", body)).await;
        let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
            .expect("client did not build");

        let error = client
            .get_study_power_data("study-1")
            .await
            .expect_err("a 500 was reported as success")
            .to_string();

        assert!(
            error.contains(body),
            "the response body was dropped rather than relayed: {error}"
        );
        assert!(error.contains("500"), "the status code was lost: {error}");
    }
}

/// Not a criterion of its own, but the assumption the three above rest on:
/// the body is read as text, so a non-2xx that happens to carry *no* body is
/// still reported as the status it was rather than as a parse failure.
#[tokio::test]
async fn an_empty_non_2xx_body_still_reports_the_status() {
    let mock = MockCore::start(Behavior::plain_text_error(502, "Bad Gateway", "")).await;
    let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
        .expect("client did not build");

    let error = client
        .status()
        .await
        .expect_err("a 502 was reported as success")
        .to_string();

    assert!(error.contains("502"), "the status code was lost: {error}");
}

// ---------------------------------------------------------------------------
// The mock itself
// ---------------------------------------------------------------------------

/// A JSON request body must arrive intact, or the two POST-shaped
/// assertions above would be pinning a request the client never really sent.
#[tokio::test]
async fn the_mock_sees_the_json_body_a_post_sent() {
    let mock = MockCore::start(Behavior::plain_text_error(503, "Service Unavailable", "nope")).await;
    let client = CoreClient::new(&config(json!({ "base_url": mock.base_url() })))
        .expect("client did not build");

    let _ = client.resolve_chip("nrf52840").await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    let body = requests[0].body_text();
    assert!(
        body.contains("nrf52840"),
        "POST /resolve-chip arrived without its body: {body:?}"
    );
}
