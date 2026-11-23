//! `embarch-api core-key`: Core's public key, from its challenge, with the
//! signature checked first (`embarch-api` decision 129; `embarch-core` decision
//! 110, `interfaces/signing.md`).
//!
//! The frame Core signs is `embarch-core-auth-v1\n<core_nonce>\n<nonce>`, with
//! `nonce` the one **this** process drew. Nothing in the answer is trusted
//! until `frame` equals that string rebuilt locally and the signature over it
//! verifies under the key the answer carries. The fingerprint is the one
//! `verify` uses (SHA-256 of the raw key).

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use embarch_core_client::ChallengeAnswer;

use crate::verify::{sha, unhex};

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
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
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
    let fingerprint = sha(&key_bytes);
    if !answer.keyid.eq_ignore_ascii_case(&fingerprint) {
        bail!("Core's keyid {} is not the SHA-256 of the public key it sent ({fingerprint})", answer.keyid);
    }
    Ok(CoreKey { public_key: unhex_to_lower(&key_bytes), fingerprint })
}

fn unhex_to_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
