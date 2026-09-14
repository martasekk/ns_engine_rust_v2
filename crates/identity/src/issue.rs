//! Minting a token a company's own back end would mint.
//!
//! The counterpart of [`Hs256Verifier`](crate::Hs256Verifier), and the only
//! supported way to make a credential outside the tests. In production this
//! is not the interesting half — a company mints its own tokens, in its own
//! service, at the moment it knows which of *its* users is on the page, and
//! that service is the one thing that must hold the signing key. What this
//! function is for is everything before that exists: a test company, a
//! staging environment, a demo, an integration test in another language.
//!
//! It refuses what the verifier would refuse. A subject the session id
//! cannot be made of, or a life longer than a token may claim, is a mistake
//! worth hearing about while minting rather than as a flat refusal at the
//! far end, where the log says only that something was denied.

use crate::base64url::b64url_encode;
use crate::hmac::hmac_sha256;
use crate::jwt::MAX_LIFETIME_SECS;
use crate::vocabulary::valid_claim;

/// Why a token could not be minted. Unlike `Denied`, these are for the
/// person holding the key: they say exactly what to change.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CannotMint {
    #[error(
        "{what} {value:?} is not usable in a session id: 1-64 characters of \
         [A-Za-z0-9_.:-], and never a '/'"
    )]
    BadClaim { what: &'static str, value: String },
    #[error("a life of {lifetime}s is longer than the {max}s a token may claim")]
    LifetimeTooLong { lifetime: u64, max: u64 },
    #[error("refusing to sign with an empty key")]
    EmptyKey,
}

/// An HS256 token for one user of one company.
///
/// `tenant` becomes `iss` and the first segment of the session id; `subject`
/// becomes `sub` and the last. The subject must be opaque — never an email
/// address or a phone number — because it is what the session id is built
/// from, and the session id is the fact scope key and goes in the log.
pub fn mint_hs256(
    key: &[u8],
    tenant: &str,
    subject: &str,
    issued_at: u64,
    lifetime_secs: u64,
) -> Result<String, CannotMint> {
    if key.is_empty() {
        return Err(CannotMint::EmptyKey);
    }
    // The same charset the verifier insists on, refused here so the mistake
    // is named where it was made. A '/' is the one that matters: the session
    // id is `<iss>/<channel>/<sub>`, and a subject holding a separator could
    // name another user's conversation.
    for (what, value) in [("tenant", tenant), ("subject", subject)] {
        if !valid_claim(value) {
            return Err(CannotMint::BadClaim {
                what,
                value: value.to_string(),
            });
        }
    }
    if lifetime_secs > MAX_LIFETIME_SECS {
        return Err(CannotMint::LifetimeTooLong {
            lifetime: lifetime_secs,
            max: MAX_LIFETIME_SECS,
        });
    }
    let header = br#"{"alg":"HS256","typ":"JWT"}"#;
    let claims = format!(
        r#"{{"iss":"{tenant}","sub":"{subject}","iat":{issued_at},"exp":{}}}"#,
        issued_at.saturating_add(lifetime_secs)
    );
    let signed = format!(
        "{}.{}",
        b64url_encode(header),
        b64url_encode(claims.as_bytes())
    );
    let signature = hmac_sha256(key, signed.as_bytes());
    Ok(format!("{signed}.{}", b64url_encode(&signature)))
}

/// Unix seconds now. Minting is the one place in this crate that reads a
/// real clock — the verifier's is injected so its tests can hold time still.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{Hs256Verifier, TenantAuth};
    use crate::vocabulary::TokenVerifier;
    use std::collections::HashMap;

    const NOW: u64 = 1_700_000_000;
    const KEY: &[u8] = b"acme-signing-key";

    fn verifier() -> Hs256Verifier {
        Hs256Verifier::with_clock(
            HashMap::from([(
                "acme".to_string(),
                TenantAuth {
                    current: KEY.to_vec(),
                    previous: None,
                    iat_floor: 0,
                },
            )]),
            Box::new(|| NOW),
        )
    }

    /// The point of the pair: what this mints, that verifies. Without this
    /// test each half could be self-consistently wrong.
    #[test]
    fn what_is_minted_here_is_what_the_verifier_accepts() {
        let token = mint_hs256(KEY, "acme", "u1", NOW - 10, 600).expect("mints");
        let claims = verifier().verify(&token).expect("verifies");
        assert_eq!(claims.iss, "acme");
        assert_eq!(claims.sub, "u1");
        assert_eq!(claims.exp - claims.iat, 600);
    }

    #[test]
    fn a_token_signed_with_another_key_is_not_accepted() {
        let token = mint_hs256(b"not-acmes-key", "acme", "u1", NOW - 10, 600).expect("mints");
        assert!(verifier().verify(&token).is_err());
    }

    /// A subject holding the separator would let one token name another
    /// user's session, so it is refused where it is written rather than at
    /// the far end, where the caller is told only that it was denied.
    #[test]
    fn a_subject_that_could_name_another_session_is_refused_while_minting() {
        for bad in ["u1/../u2", "acme/web/u2", "", &"u".repeat(65)] {
            let err = mint_hs256(KEY, "acme", bad, NOW, 600).expect_err("refused");
            assert!(
                matches!(
                    err,
                    CannotMint::BadClaim {
                        what: "subject",
                        ..
                    }
                ),
                "{err:?}"
            );
        }
        let err = mint_hs256(KEY, "acme corp", "u1", NOW, 600).expect_err("refused");
        assert!(
            matches!(err, CannotMint::BadClaim { what: "tenant", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_life_longer_than_a_token_may_claim_is_refused_while_minting() {
        let err = mint_hs256(KEY, "acme", "u1", NOW, MAX_LIFETIME_SECS + 1).expect_err("refused");
        assert!(matches!(err, CannotMint::LifetimeTooLong { .. }), "{err:?}");
        // And the boundary itself mints, and verifies.
        let token = mint_hs256(KEY, "acme", "u1", NOW, MAX_LIFETIME_SECS).expect("mints");
        assert!(verifier().verify(&token).is_ok());
    }

    #[test]
    fn an_empty_key_is_refused_rather_than_signed_with() {
        assert_eq!(
            mint_hs256(b"", "acme", "u1", NOW, 600),
            Err(CannotMint::EmptyKey)
        );
    }
}
