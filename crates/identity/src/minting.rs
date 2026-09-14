//! Minting tokens for the tests, including ones no well-behaved client would
//! present.
//!
//! Both the verifier's tests and the resolvers' tests need the same forged
//! credentials, so the helpers live once here rather than once per module.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::hmac::hmac_sha256;
use crate::jwt::{Hs256Verifier, TenantAuth};

pub(crate) const NOW_SECS: u64 = 1_700_000_000;
pub(crate) const KEY: &[u8] = b"current-signing-key";
pub(crate) const OLD_KEY: &[u8] = b"previous-signing-key";

pub(crate) fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A compact JWS over any header and payload, however implausible: the
/// tests need to mint tokens a well-behaved client never would.
pub(crate) fn mint(key: &[u8], header: &str, payload: &str) -> String {
    let signed = format!("{}.{}", b64u(header.as_bytes()), b64u(payload.as_bytes()));
    let sig = hmac_sha256(key, signed.as_bytes());
    format!("{signed}.{}", b64u(&sig))
}

pub(crate) fn claims_json(iss: &str, sub: &str, iat: u64, exp: u64) -> String {
    format!(r#"{{"iss":"{iss}","sub":"{sub}","iat":{iat},"exp":{exp}}}"#)
}

pub(crate) fn good_token(key: &[u8]) -> String {
    mint(
        key,
        r#"{"alg":"HS256","typ":"JWT"}"#,
        &claims_json("acme", "u1", NOW_SECS - 60, NOW_SECS + 600),
    )
}

pub(crate) fn verifier(auth: TenantAuth) -> Hs256Verifier {
    Hs256Verifier::with_clock(
        HashMap::from([("acme".to_string(), auth)]),
        Box::new(|| NOW_SECS),
    )
}

pub(crate) fn acme(previous: Option<Vec<u8>>, iat_floor: u64) -> TenantAuth {
    TenantAuth {
        current: KEY.to_vec(),
        previous,
        iat_floor,
    }
}
