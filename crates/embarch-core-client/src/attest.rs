//! Which Core is answering, checked before a token goes to it (`embarch-api`
//! decisions 109 and 129; `embarch-core` decision 110, `interfaces/signing.md`).
//!
//! The frame Core signs is `embarch-core-auth-v1\n<core_nonce>\n<nonce>`, with
//! `nonce` the one **this** process drew. Nothing in the answer is trusted
//! until `frame` equals that string rebuilt locally and the signature over it
//! verifies under the key the answer carries. The fingerprint is the SHA-256 of
//! the raw key, the one `embarch-api verify` uses.

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::ChallengeAnswer;

const FRAME_TAG: &str = "embarch-core-auth-v1";
/// 16 bytes, 32 hex characters: the shortest nonce Core accepts.
const NONCE_BYTES: usize = 16;

/// Core's key, once its signature over our nonce has checked.
#[derive(Debug, PartialEq, Eq)]
pub struct CoreKey {
    /// The raw Ed25519 public key, 64 lowercase hex characters (`verify --public-key`).
    pub public_key: String,
    /// SHA-256 of the raw key, hex (the DSSE `keyid`).
    pub fingerprint: String,
}

/// A fresh random nonce in the shape Core accepts.
pub fn fresh_nonce() -> Result<String> {
    let mut b = [0u8; NONCE_BYTES];
    getrandom::fill(&mut b).map_err(|e| anyhow!("cannot draw a random nonce: {e}"))?;
    Ok(hex(&b))
}

/// Checks `answer` against the `nonce` that was sent. Any mismatch is an error naming it.
pub fn check(nonce: &str, answer: &ChallengeAnswer) -> Result<CoreKey> {
    let expected = format!("{FRAME_TAG}\n{}\n{nonce}", answer.core_nonce);
    if answer.frame != expected {
        bail!("Core's answer signs a frame that is not the challenge frame over the nonce sent (its core_nonce or the tag differs), so it is not an answer to this challenge");
    }
    let key_bytes = unhex(&answer.public_key)
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| anyhow!("Core's public_key is not 64 hex characters"))?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|e| anyhow!("Core's public_key is not an Ed25519 key: {e}"))?;
    let sig = unhex(&answer.signature)
        .and_then(|b| <[u8; 64]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| anyhow!("Core's signature is not 128 hex characters"))?;
    key.verify_strict(expected.as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| anyhow!("Core's signature over the challenge frame does not verify under the public key it sent; do not use this key"))?;
    let fingerprint = hex(&Sha256::digest(key_bytes));
    if !answer.keyid.eq_ignore_ascii_case(&fingerprint) {
        bail!("Core's keyid {} is not the SHA-256 of the public key it sent ({fingerprint})", answer.keyid);
    }
    Ok(CoreKey { public_key: hex(&key_bytes), fingerprint })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Which token a client sends, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenMode {
    /// No `core-fingerprint` beside the token file: no Core here is attested, the token goes as it always did.
    Unattested,
    /// Core's fingerprint is the installed one: the machine-wide token.
    Matched,
    /// Core's fingerprint is not the installed one: the development token.
    Development,
    /// A token the caller named, sent after a verified challenge.
    Explicit,
    /// Nothing is sent: the challenge failed, or the Core is foreign and there is no development token.
    Refused,
}

/// What `status` shows about the Core the client reaches.
#[derive(Debug, Clone, Serialize)]
pub struct Attestation {
    pub mode: TokenMode,
    /// SHA-256 of the key Core proved it holds; `None` when no challenge was answered.
    pub fingerprint: Option<String>,
    /// The fingerprint recorded beside the token file; `None` when there is none.
    pub installed_fingerprint: Option<String>,
    /// Whether the two agree; `None` when either is missing.
    pub matches: Option<bool>,
    /// Why nothing is sent, when `mode` is `refused`.
    pub error: Option<String>,
}

impl Attestation {
    /// Three human lines: mode, fingerprint and the match.
    pub fn human(&self) -> String {
        let fp = self.fingerprint.as_deref().unwrap_or("none (no challenge answered)");
        let m = match self.matches {
            Some(true) => "matches the installed Core".to_string(),
            Some(false) => format!("does NOT match the installed Core ({})", self.installed_fingerprint.as_deref().unwrap_or("?")),
            None => "no installed fingerprint to compare".to_string(),
        };
        let mut s = format!("token mode: {}\ncore fingerprint: {fp}\n{m}", serde_json::to_value(self.mode).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default());
        if let Some(e) = &self.error {
            s.push_str(&format!("\nno token sent: {e}"));
        }
        s
    }
}

/// A foreign Core with no development token: carried as an error so a caller can show both fingerprints.
#[derive(Debug)]
pub struct AttestationRefused {
    pub core_fingerprint: Option<String>,
    pub installed_fingerprint: String,
    pub message: String,
}

impl std::fmt::Display for AttestationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AttestationRefused {}
