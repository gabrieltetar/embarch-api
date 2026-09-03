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

use std::time::{Duration, Instant};

use embarch_core_client::{CoreClient, CoreConfig, SignalDirection, SignalLink, SignalRoute};
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
/// The mock answers `503` with a plain-text body to *everything*, so no call
/// gets far enough to need a well-formed response body and none of them can
/// take a status-specific shortcut (a `404`, notably, is a meaningful
/// non-error for `dev_bench_port`/`study_streams`/`study_steps` and would
/// have exercised a different branch). Every call therefore fails, and
/// failing is fine: what is under test is the request that went out, not the
/// answer that came back.
///
/// The route list is the point of the test. A new endpoint added to
/// `CoreClient` without `.bearer_auth(…)` is only caught here if the sweep
/// calls it, so this list is meant to stay exhaustive over the client's
/// networked surface.
#[tokio::test]
async fn every_outbound_call_carries_the_bearer_token() {
    let mock = MockCore::start(Behavior::plain_text_error(
        503,
        "Service Unavailable",
        "the mock is refusing everything on purpose",
    ))
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

    // Every listed call must actually have hit the wire. Without this a
    // method that silently stopped issuing a request would still pass the
    // header assertion above, because there would be nothing to check.
    let observed: Vec<(String, String)> = requests.iter().map(|r| r.route()).collect();
    for (method, path) in [
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
    ] {
        let route = (method.to_string(), path.to_string());
        assert!(
            observed.contains(&route),
            "{method} {path} never reached the mock; observed routes were {observed:?}"
        );
    }
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
