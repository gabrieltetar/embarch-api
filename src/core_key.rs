//! `embarch-api core-key`: Core's public key, from its challenge, with the
//! signature checked first (`embarch-api` decision 129). The check itself lives
//! in `embarch-core-client`, which also runs it before sending any token
//! (decision 109); this re-exports it for the command and its tests.

pub use embarch_core_client::attest::{check, fresh_nonce, CoreKey};
