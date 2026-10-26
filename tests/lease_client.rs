//! The client half of embarch-core's bench lease (`embarch-api` decision 90),
//! against a scripted mock Core: taking, sending, and renewing a token, the
//! refusal's text arriving verbatim, and a second process inheriting the token
//! from the environment. Nothing here reaches a live Core.

mod support;

use std::process::{Command, Output};
use std::time::Duration;

use embarch_core_client::{CoreClient, CoreConfig, LeaseHeldError};
use serde_json::json;
use support::{Behavior, MockCore, RecordedRequest};

const REFUSAL: &str = "bench leased by 'sess-a' for 'charger bring-up' (started 2026-10-26T12:00:00Z, \
    expires 2026-10-26T13:00:00Z); send that lease's token in the x-embarch-lease header, or wait for it to expire";

fn grant() -> serde_json::Value {
    json!({
        "token": "0123456789abcdef0123456789abcdef", "holder": "sess-a", "purpose": "charger bring-up",
        "started_at": "2026-10-26T12:00:00Z", "expires_at": "2026-10-26T13:00:00Z", "ttl_s": 3600,
    })
}

fn client(mock: &MockCore) -> CoreClient {
    let config: CoreConfig =
        serde_json::from_value(json!({ "base_url": mock.base_url(), "token": "t" })).unwrap();
    CoreClient::new(&config).unwrap()
}

fn lease_header(r: &RecordedRequest) -> Option<&str> {
    r.header("x-embarch-lease")
}

/// Runs the real binary against `mock`, optionally with the token in the
/// environment: the way `embarch-atlas` reaches Core.
fn cli(mock: &MockCore, env_token: Option<&str>, args: &[&str]) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "embarch-api-lease-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_embarch-api"));
    command
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .env_remove("EMBARCH_LEASE_TOKEN");
    if let Some(token) = env_token {
        command.env("EMBARCH_LEASE_TOKEN", token);
    }
    let out = command.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[tokio::test]
async fn taking_a_lease_returns_the_token_and_every_later_request_sends_it() {
    let mock = MockCore::start(Behavior::Router {
        routes: vec![("/lease".to_string(), Behavior::json_ok(grant()))],
        otherwise: Box::new(Behavior::json_ok(json!({ "status": "ok", "probes": [] }))),
    })
    .await;
    let c = client(&mock);
    assert!(c.effective_lease_token().is_none());

    let got = c.lease_take("sess-a", Some("charger bring-up"), 3600).await.unwrap();
    assert_eq!(got.token, "0123456789abcdef0123456789abcdef");
    assert_eq!(got.lease.holder, "sess-a");
    c.status().await.unwrap();

    let reqs = mock.requests();
    let take_body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(take_body, json!({ "holder": "sess-a", "purpose": "charger bring-up", "ttl_s": 3600 }));
    assert_eq!(lease_header(&reqs[1]), Some(got.token.as_str()), "{:?}", reqs[1]);

    c.lease_release().await.unwrap();
    assert!(c.effective_lease_token().is_none(), "release must forget the token");
    c.status().await.unwrap();
    let last = mock.requests().pop().unwrap();
    assert_eq!(lease_header(&last), None);
}

#[tokio::test]
async fn a_refused_take_names_the_holder_verbatim() {
    let mock = MockCore::start(Behavior::plain_text_error(409, "Conflict", REFUSAL)).await;
    let err = client(&mock).lease_take("sess-b", None, 60).await.unwrap_err();
    assert_eq!(err.downcast_ref::<LeaseHeldError>().map(|e| e.message.as_str()), Some(REFUSAL));
}

#[tokio::test]
async fn a_lease_409_is_not_misread_as_a_study_dev_bench_or_topology_conflict() {
    let mock = MockCore::start(Behavior::Router {
        routes: vec![(
            "/status".to_string(),
            Behavior::json_ok(json!({
                "status": "ok", "probes": [],
                "study_designer_schema_version": embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
            })),
        )],
        otherwise: Box::new(Behavior::plain_text_error(409, "Conflict", REFUSAL)),
    })
    .await;
    let c = client(&mock);
    let hello = c.dev_bench_hello().await.unwrap_err();
    assert!(hello.downcast_ref::<LeaseHeldError>().is_some(), "{hello:#}");
    let validate = c.validate("dut").await.unwrap_err();
    assert!(validate.downcast_ref::<LeaseHeldError>().is_some(), "{validate:#}");
    let flash = c.flash("nRF52840_xxAA", "/nonexistent/app.hex", "hex", None, None, false).await.unwrap_err();
    assert!(format!("{flash:#}").contains(REFUSAL), "{flash:#}");
    let study: embarch_study_designer::Study =
        serde_json::from_str(include_str!("fixtures/self_test_study.json")).unwrap();
    let post = c.post_study(&study, &Default::default()).await.unwrap_err();
    assert!(post.downcast_ref::<LeaseHeldError>().is_some(), "{post:#}");
}

#[test]
fn the_cli_refusal_text_reaches_stderr_verbatim() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(MockCore::start(Behavior::plain_text_error(409, "Conflict", REFUSAL)));
    let out = cli(&mock, None, &["dev-bench-hello"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(REFUSAL), "{err}");
    assert!(!err.contains("study is already in flight"), "{err}");
}

#[test]
fn a_second_process_sends_the_token_from_the_environment_with_no_argument() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(MockCore::start(Behavior::json_ok(json!({ "status": "ok", "probes": [] }))));
    let with = cli(&mock, Some("tok-from-env"), &["status"]);
    assert!(with.status.success(), "{}", String::from_utf8_lossy(&with.stderr));
    let without = cli(&mock, None, &["status"]);
    assert!(without.status.success());
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(lease_header(&reqs[0]), Some("tok-from-env"));
    assert_eq!(lease_header(&reqs[1]), None);
}

#[test]
fn cli_lease_take_prints_the_token_once_in_json_and_the_export_line_in_text() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(MockCore::start(Behavior::json_ok(grant())));
    let out = cli(&mock, None, &["--json", "lease-take", "--holder", "sess-a", "--purpose", "charger bring-up"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["token"], "0123456789abcdef0123456789abcdef");
    assert_eq!(v["token_env"], "EMBARCH_LEASE_TOKEN");
    let body: serde_json::Value = serde_json::from_slice(&mock.requests()[0].body).unwrap();
    assert_eq!(body["ttl_s"], 3600, "the CLI default is an hour: nothing renews for it");

    let text = cli(&mock, None, &["lease-take"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("export EMBARCH_LEASE_TOKEN=0123456789abcdef0123456789abcdef"));
}

#[test]
fn cli_lease_show_reports_none_and_a_holder() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let none = rt.block_on(MockCore::start(Behavior::plain_text_error(404, "Not Found", "no lease is live")));
    let out = cli(&none, None, &["lease-show"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no bench lease is live"));

    let held = rt.block_on(MockCore::start(Behavior::json_ok(json!({
        "holder": "sess-a", "purpose": "charger bring-up",
        "started_at": "2026-10-26T12:00:00Z", "expires_at": "2026-10-26T13:00:00Z", "ttl_s": 3600,
    }))));
    let v: serde_json::Value =
        serde_json::from_slice(&cli(&held, None, &["--json", "lease-show"]).stdout).unwrap();
    assert_eq!(v["lease"]["holder"], "sess-a");
    assert_eq!(v["this_process_sends_a_token"], false);
}

#[test]
fn status_exposes_the_lease_and_last_images_so_a_session_can_compare_sha256() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(MockCore::start(Behavior::json_ok(json!({
        "status": "ok", "probes": [],
        "lease": { "holder": "sess-a", "purpose": "p", "started_at": "s", "expires_at": "e", "ttl_s": 60 },
        "last_images": [{
            "chip": "nRF54L15", "probe_serial": "123", "path": "/x/app.hex",
            "sha256": "ab12", "flashed_at": "2026-10-26T12:00:00Z", "lease_holder": "sess-a",
        }],
    }))));
    let v: serde_json::Value = serde_json::from_slice(&cli(&mock, None, &["--json", "status"]).stdout).unwrap();
    assert_eq!(v["lease"]["holder"], "sess-a");
    assert_eq!(v["last_images"][0]["sha256"], "ab12");
    assert_eq!(v["last_images"][0]["chip"], "nRF54L15");
    let old = rt.block_on(MockCore::start(Behavior::json_ok(json!({ "status": "ok", "probes": [] }))));
    let v: serde_json::Value = serde_json::from_slice(&cli(&old, None, &["--json", "status"]).stdout).unwrap();
    assert!(v["last_images"].is_null());
    let none = rt.block_on(MockCore::start(Behavior::json_ok(json!({ "status": "ok", "probes": [], "last_images": [] }))));
    let v: serde_json::Value = serde_json::from_slice(&cli(&none, None, &["--json", "status"]).stdout).unwrap();
    assert_eq!(v["last_images"], json!([]));
}

#[tokio::test]
async fn an_older_core_status_without_the_new_fields_still_parses() {
    let mock = MockCore::start(Behavior::json_ok(json!({ "status": "ok", "probes": [] }))).await;
    let s = client(&mock).status().await.unwrap();
    assert!(s.lease.is_none());
    assert!(s.last_images.is_none());
    let mock = MockCore::start(Behavior::json_ok(json!({ "status": "ok", "probes": [], "last_images": [] }))).await;
    assert_eq!(client(&mock).status().await.unwrap().last_images, Some(vec![]));
}

#[tokio::test]
async fn the_keepalive_renews_a_taken_lease_on_its_own_with_the_token() {
    let mut short = grant();
    short["ttl_s"] = json!(3);
    let mock = MockCore::start(Behavior::json_ok(short)).await;
    let c = client(&mock);
    c.lease_take("sess-a", None, 3).await.unwrap();
    let keepalive = c.clone();
    let task = tokio::spawn(async move { keepalive.lease_keepalive().await });
    tokio::time::sleep(Duration::from_millis(3500)).await;
    task.abort();
    let renews: Vec<_> = mock.requests().into_iter().filter(|r| r.method == "PUT").collect();
    assert!(!renews.is_empty(), "no renew in 3.5 s of a 3 s lease");
    assert_eq!(lease_header(&renews[0]), Some("0123456789abcdef0123456789abcdef"));
}

#[tokio::test]
async fn the_keepalive_adopts_and_renews_a_token_it_was_handed() {
    let mock = MockCore::start(Behavior::json_ok(grant())).await;
    let c = client(&mock);
    c.set_lease_token("handed-token".to_string());
    let keepalive = c.clone();
    let task = tokio::spawn(async move { keepalive.lease_keepalive().await });
    tokio::time::sleep(Duration::from_millis(2500)).await;
    task.abort();
    let renews: Vec<_> = mock.requests().into_iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(renews.len(), 1, "unknown ttl renews once at once, then learns 3600 s: {renews:?}");
    assert_eq!(lease_header(&renews[0]), Some("handed-token"));
}
