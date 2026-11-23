//! `core-key` (`embarch-api` decision 129): the challenge's answer is checked
//! against the nonce this process drew, with no live Core.
//!
//! `check` is driven with answers a real Ed25519 key signs. The mock Core
//! answers a fixed body, so it cannot sign the random nonce a run draws: the
//! end-to-end test is the replay, an answer valid for some other nonce, which
//! the command must refuse.

mod support;

use ed25519_dalek::{Signer, SigningKey};
use embarch_api::core_key::{check, fresh_nonce};
use embarch_core_client::{ChallengeAnswer, CoreClient, CoreConfig};
use serde_json::json;
use sha2::{Digest, Sha256};
use support::{Behavior, MockCore};

const NONCE: &str = "00112233445566778899aabbccddeeff";
const CORE_NONCE: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn answer(k: &SigningKey, nonce: &str) -> ChallengeAnswer {
    let frame = format!("embarch-core-auth-v1\n{CORE_NONCE}\n{nonce}");
    let pk = k.verifying_key().to_bytes();
    ChallengeAnswer {
        signature: hex(&k.sign(frame.as_bytes()).to_bytes()),
        frame,
        core_nonce: CORE_NONCE.into(),
        keyid: hex(&Sha256::digest(pk)),
        public_key: hex(&pk),
    }
}

#[test]
fn a_good_answer_gives_the_key_and_its_fingerprint() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let got = check(NONCE, &answer(&k, NONCE)).unwrap();
    assert_eq!(got.public_key, hex(&k.verifying_key().to_bytes()));
    assert_eq!(got.public_key.len(), 64);
    assert_eq!(got.fingerprint, hex(&Sha256::digest(k.verifying_key().to_bytes())));
}

#[test]
fn a_bad_signature_is_refused() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let mut a = answer(&k, NONCE);
    let other = SigningKey::from_bytes(&[8; 32]);
    a.signature = answer(&other, NONCE).signature;
    let e = check(NONCE, &a).unwrap_err().to_string();
    assert!(e.contains("does not verify"), "{e}");
}

#[test]
fn an_answer_to_another_nonce_is_refused() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let e = check(NONCE, &answer(&k, "ffffffffffffffffffffffffffffffff")).unwrap_err().to_string();
    assert!(e.contains("not the challenge frame"), "{e}");
}

#[test]
fn a_keyid_that_is_not_the_keys_hash_is_refused() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let mut a = answer(&k, NONCE);
    a.keyid = "0".repeat(64);
    assert!(check(NONCE, &a).unwrap_err().to_string().contains("keyid"));
}

#[test]
fn nonces_are_fresh_and_in_the_shape_core_accepts() {
    let (a, b) = (fresh_nonce().unwrap(), fresh_nonce().unwrap());
    assert_ne!(a, b);
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
}

#[tokio::test]
async fn the_challenge_is_sent_without_the_token() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let a = answer(&k, NONCE);
    let body = json!({ "frame": a.frame, "core_nonce": a.core_nonce, "keyid": a.keyid,
                       "public_key": a.public_key, "signature": a.signature });
    let mock = MockCore::start(Behavior::json_ok(body)).await;
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "secret-token", "base_url": mock.base_url() })).unwrap();
    let got = CoreClient::new(&cfg).unwrap().auth_challenge(NONCE).await.unwrap();
    assert_eq!(got.public_key, a.public_key);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].route(), ("POST".to_string(), "/auth/challenge".to_string()));
    assert!(reqs[0].header("authorization").is_none(), "the token must not go to a Core not yet checked");
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&reqs[0].body).unwrap(), json!({ "nonce": NONCE }));
}

#[tokio::test]
async fn the_command_refuses_a_replayed_answer_and_exits_1() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let a = answer(&k, NONCE);
    let body = json!({ "frame": a.frame, "core_nonce": a.core_nonce, "keyid": a.keyid,
                       "public_key": a.public_key, "signature": a.signature });
    let mock = MockCore::start(Behavior::json_ok(body)).await;
    let dir = support::unique_dir("core-key");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .args(["--config", cfg.to_str().unwrap(), "--json", "core-key"])
        .env("XDG_DATA_HOME", &dir)
        .env("LOCALAPPDATA", &dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(v["success"], false);
    assert!(v["error"].as_str().unwrap().contains("core-key failed"), "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}
