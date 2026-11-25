//! The client sends Core's token only after a challenge (`embarch-api` decision 109):
//! against a mock Core that signs the nonce it is sent, with the machine-wide
//! and development stores in temp directories.

mod support;

use embarch_core_client::attest::TokenMode;
use embarch_core_client::{CoreClient, CoreConfig, TokenScope};
use serde_json::json;
use support::{challenge_fingerprint, Behavior, ChallengeFault, MockCore};

const MACHINE: &str = "machine-wide-token";
const DEV: &str = "development-token";
const EXPLICIT: &str = "explicit-token";

fn core(fault: ChallengeFault, seed: u8) -> Behavior {
    Behavior::Router {
        routes: vec![("/auth/challenge".into(), Behavior::Challenge { seed, fault })],
        otherwise: Box::new(Behavior::json_ok(json!({ "status": "ok", "probes": [] }))),
    }
}

/// A client over temp stores. `fingerprint` is written beside the machine token when given; `dev` is the development token.
fn client(mock: &MockCore, tag: &str, fingerprint: Option<String>, dev: Option<&str>, explicit: bool) -> (CoreClient, std::path::PathBuf) {
    let root = support::unique_dir(tag);
    let (machine, dev_dir) = (root.join("machine"), root.join("dev"));
    std::fs::create_dir_all(&machine).unwrap();
    std::fs::create_dir_all(&dev_dir).unwrap();
    std::fs::write(machine.join("token"), MACHINE).unwrap();
    if let Some(f) = fingerprint {
        std::fs::write(machine.join("core-fingerprint"), format!("{f}\n")).unwrap();
    }
    if let Some(d) = dev {
        std::fs::write(dev_dir.join("token"), d).unwrap();
    }
    let cfg: CoreConfig = serde_json::from_value(if explicit { json!({ "base_url": mock.base_url(), "token": EXPLICIT }) } else { json!({ "base_url": mock.base_url() }) }).unwrap();
    let c = CoreClient::new_with_stores(&cfg, TokenScope::Agent, machine, dev_dir).unwrap();
    (c, root)
}

fn auth_of_status(mock: &MockCore) -> Option<String> {
    mock.requests().iter().find(|r| r.path() == "/status").and_then(|r| r.header("authorization").map(str::to_string))
}

#[tokio::test]
async fn a_match_sends_the_machine_wide_token() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-match", Some(challenge_fingerprint(7)), Some(DEV), false);
    c.status().await.unwrap();
    assert_eq!(auth_of_status(&mock).unwrap(), format!("Bearer {MACHINE}"));
    assert!(mock.requests()[0].header("authorization").is_none(), "the challenge carries no token");
    assert_eq!(mock.requests()[0].path(), "/auth/challenge");
    assert_eq!(c.attestation().await.mode, TokenMode::Matched);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_mismatch_sends_the_development_token() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-dev", Some(challenge_fingerprint(9)), Some(DEV), false);
    c.status().await.unwrap();
    assert_eq!(auth_of_status(&mock).unwrap(), format!("Bearer {DEV}"));
    let a = c.attestation().await;
    assert_eq!((a.mode, a.matches), (TokenMode::Development, Some(false)));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_mismatch_with_no_development_store_sends_no_header_and_errors_naming_both() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-nodev", Some(challenge_fingerprint(9)), None, false);
    let e = format!("{:#}", c.status().await.unwrap_err());
    assert!(e.contains(&challenge_fingerprint(7)) && e.contains(&challenge_fingerprint(9)), "{e}");
    assert!(mock.requests().iter().all(|r| r.header("authorization").is_none()));
    assert_eq!(c.attestation().await.mode, TokenMode::Refused);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn no_fingerprint_file_is_todays_behaviour_with_no_challenge() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-none", None, Some(DEV), false);
    c.status().await.unwrap();
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(auth_of_status(&mock).unwrap(), format!("Bearer {MACHINE}"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_bad_signature_or_foreign_frame_sends_nothing() {
    for (fault, tag) in [(ChallengeFault::BadSignature, "att-sig"), (ChallengeFault::ForeignFrame, "att-frame")] {
        let mock = MockCore::start(core(fault, 7)).await;
        let (c, root) = client(&mock, tag, Some(challenge_fingerprint(7)), Some(DEV), false);
        assert!(c.status().await.is_err());
        assert!(mock.requests().iter().all(|r| r.header("authorization").is_none()), "{fault:?}");
        assert!(mock.requests().iter().all(|r| r.path() == "/auth/challenge"));
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn an_explicit_token_wins_but_only_after_the_challenge() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-explicit", Some(challenge_fingerprint(9)), Some(DEV), true);
    c.status().await.unwrap();
    assert_eq!(mock.requests()[0].path(), "/auth/challenge");
    assert_eq!(auth_of_status(&mock).unwrap(), format!("Bearer {EXPLICIT}"));
    assert_eq!(c.attestation().await.mode, TokenMode::Explicit);
    let _ = std::fs::remove_dir_all(root);

    let mock = MockCore::start(core(ChallengeFault::BadSignature, 7)).await;
    let (c, root) = client(&mock, "att-explicit-bad", Some(challenge_fingerprint(7)), None, true);
    assert!(c.status().await.is_err());
    assert!(mock.requests().iter().all(|r| r.header("authorization").is_none()));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn one_challenge_serves_many_requests() {
    let mock = MockCore::start(core(ChallengeFault::None, 7)).await;
    let (c, root) = client(&mock, "att-once", Some(challenge_fingerprint(7)), None, false);
    c.status().await.unwrap();
    c.status().await.unwrap();
    assert_eq!(mock.requests().iter().filter(|r| r.path() == "/auth/challenge").count(), 1);
    let _ = std::fs::remove_dir_all(root);
}
