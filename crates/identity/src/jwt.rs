//! Is this token genuine: HS256 compact-JWS verification and the bounds a
//! claim must sit inside.
//!
//! One key set per tenant, a clock that is injected, and nothing about who
//! presented the token or over which channel.

use std::collections::HashMap;

use serde::Deserialize;

use crate::base64url::{b64url, Segment};
use crate::hmac::{ct_eq, hmac_sha256};
use crate::vocabulary::{valid_claim, Claims, Denied, TokenVerifier};

/// How far ahead of our own clock a token's `iat` may sit. Minting and
/// presenting are seconds apart in practice, and two machines' clocks differ
/// by less than this or they have a worse problem than authentication.
pub const MAX_CLOCK_SKEW_SECS: u64 = 60;

/// The longest life a token may claim, `exp - iat`. A credential that outlives
/// the working day it was minted for is a credential nobody is tracking.
pub const MAX_LIFETIME_SECS: u64 = 24 * 60 * 60;

/// One tenant's signing material.
pub struct TenantAuth {
    /// The key tokens are signed with now.
    pub current: Vec<u8>,
    /// The key it replaced, still accepted so that rotating is not an outage.
    /// Dropped once every token minted under it has expired.
    pub previous: Option<Vec<u8>>,
    /// Tokens issued before this instant are refused however well signed: the
    /// tenant's way of revoking everything minted up to a breach. Unix seconds.
    pub iat_floor: u64,
}

impl TenantAuth {
    /// Current key first. Both are always tried, so which key signed a token
    /// is not readable from how long the refusal took.
    fn keys(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.current.as_slice()).chain(self.previous.as_deref())
    }
}

/// HS256 compact-JWS verification, one key set per tenant.
pub struct Hs256Verifier {
    tenants: HashMap<String, TenantAuth>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

impl Hs256Verifier {
    pub fn new(tenants: HashMap<String, TenantAuth>) -> Self {
        Self::with_clock(tenants, Box::new(|| nscore::time::now_ms() / 1000))
    }

    /// The clock is injected the way the engine's is (`Engine::with_clock`), so
    /// that expiry is testable without sleeping. Unix seconds.
    pub fn with_clock(
        tenants: HashMap<String, TenantAuth>,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self { tenants, clock }
    }
}

impl TokenVerifier for Hs256Verifier {
    fn verify(&self, token: &str) -> Result<Claims, Denied> {
        let mut segments = token.split('.');
        let (h, p, s) = match (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => {
                return Err(Denied::Malformed(
                    "a compact JWS has exactly three dot-separated segments",
                ))
            }
        };

        let header: Header = serde_json::from_slice(&b64url(h, Segment::Header)?)
            .map_err(|_| Denied::Malformed("header is not a JSON object with an alg"))?;
        // The header names the algorithm but does not get to choose it: an
        // `alg: none` or `RS256` token is refused before a signature is
        // computed at all, not verified some other way.
        if header.alg != "HS256" {
            return Err(Denied::UnsupportedAlgorithm { alg: header.alg });
        }

        let claims: Claims =
            serde_json::from_slice(&b64url(p, Segment::Payload)?).map_err(|_| {
                Denied::Malformed("payload is not a JSON object with iss, sub, iat and exp")
            })?;
        let signature = b64url(s, Segment::Signature)?;

        let auth = self
            .tenants
            .get(&claims.iss)
            .ok_or_else(|| Denied::UnknownTenant {
                tenant: claims.iss.clone(),
            })?;
        // The signed input is the token's own bytes, not a re-encoding of what
        // was decoded, so an encoding that differs cannot slip through.
        let signed = &token[..h.len() + 1 + p.len()];
        let mut matched = false;
        for key in auth.keys() {
            // No early return: every configured key is tried on every token.
            matched |= ct_eq(&hmac_sha256(key, signed.as_bytes()), &signature);
        }
        if !matched {
            return Err(Denied::BadSignature {
                tenant: claims.iss.clone(),
            });
        }

        // Only now, with the signature good, are the claims worth reading.
        if !valid_claim(&claims.iss) || !valid_claim(&claims.sub) {
            return Err(Denied::Malformed(
                "iss and sub must each match [A-Za-z0-9_.:-]{1,64}",
            ));
        }
        let now = (self.clock)();
        if claims.exp <= now {
            return Err(Denied::Expired {
                exp: claims.exp,
                now,
            });
        }
        // A token dated in the future is what makes `iat_floor` mean
        // anything. Without this, someone who held the key for a minute mints
        // a batch dated a century out, and every one of them survives the
        // rotation that was supposed to end them: the floor only refuses
        // tokens issued *before* it, and these claim to be issued after.
        if claims.iat > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
            return Err(Denied::IssuedInTheFuture {
                iat: claims.iat,
                now,
                skew: MAX_CLOCK_SKEW_SECS,
            });
        }
        // And a ceiling on the life a token may claim, so that a stolen one is
        // a bounded problem even before anybody notices it was stolen.
        let lifetime = claims.exp.saturating_sub(claims.iat);
        if lifetime > MAX_LIFETIME_SECS {
            return Err(Denied::LifetimeTooLong {
                lifetime,
                max: MAX_LIFETIME_SECS,
            });
        }
        if claims.iat < auth.iat_floor {
            return Err(Denied::IssuedBeforeFloor {
                iat: claims.iat,
                floor: auth.iat_floor,
            });
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minting::*;

    #[test]
    fn a_token_signed_with_the_wrong_key_is_refused() {
        let v = verifier(acme(None, 0));
        let err = v.verify(&good_token(b"not-the-key")).unwrap_err();
        assert!(matches!(err, Denied::BadSignature { .. }), "{err:?}");
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let v = verifier(acme(None, 0));
        let token = good_token(KEY);
        let mut parts = token.split('.');
        let (h, _, s) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        // Same shape, a different subject, the original signature.
        let swapped = b64u(claims_json("acme", "u2", NOW_SECS - 60, NOW_SECS + 600).as_bytes());
        let err = v.verify(&format!("{h}.{swapped}.{s}")).unwrap_err();
        assert!(matches!(err, Denied::BadSignature { .. }), "{err:?}");
    }

    #[test]
    fn an_expired_token_is_refused() {
        let v = verifier(acme(None, 0));
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", NOW_SECS - 600, NOW_SECS - 1),
        );
        let err = v.verify(&token).unwrap_err();
        assert!(
            matches!(err, Denied::Expired { exp, now } if exp == NOW_SECS - 1 && now == NOW_SECS),
            "{err:?}"
        );
    }

    #[test]
    fn a_token_dated_in_the_future_is_refused() {
        let v = verifier(acme(None, 0));
        let ahead = NOW_SECS + MAX_CLOCK_SKEW_SECS + 1;
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", ahead, ahead + 600),
        );
        let err = v.verify(&token).unwrap_err();
        assert!(
            matches!(err, Denied::IssuedInTheFuture { iat, .. } if iat == ahead),
            "{err:?}"
        );
    }

    #[test]
    fn a_token_inside_the_skew_allowance_is_accepted() {
        // Two machines' clocks disagree by a little; that is not an attack.
        let v = verifier(acme(None, 0));
        let ahead = NOW_SECS + MAX_CLOCK_SKEW_SECS;
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", ahead, ahead + 600),
        );
        assert_eq!(v.verify(&token).unwrap().sub, "u1");
    }

    #[test]
    fn a_token_claiming_more_than_a_days_life_is_refused() {
        let v = verifier(acme(None, 0));
        let iat = NOW_SECS - 10;
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", iat, iat + MAX_LIFETIME_SECS + 1),
        );
        let err = v.verify(&token).unwrap_err();
        assert!(
            matches!(err, Denied::LifetimeTooLong { max, .. } if max == MAX_LIFETIME_SECS),
            "{err:?}"
        );
    }

    /// The attack the two bounds above exist to stop, end to end.
    ///
    /// Someone holds the signing key for a moment and mints a token dated a
    /// century out. The tenant notices, rotates the key, and sets the floor to
    /// the moment of the breach. The old key is still accepted — that is what
    /// rotation means — so the signature still matches, the token is not
    /// expired, and the floor cannot touch it because the floor only refuses
    /// tokens issued *earlier*. Without an upper bound on `iat`, revoking is
    /// impossible short of dropping the previous key and cutting off every
    /// honest client mid-rotation.
    #[test]
    fn a_future_dated_token_cannot_step_over_the_revocation_floor() {
        let rotated = verifier(acme(Some(KEY.to_vec()), NOW_SECS));
        let far = NOW_SECS + 60 * 60 * 24 * 365 * 80;
        let minted_during_the_breach = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", far, far + 600),
        );
        let err = rotated.verify(&minted_during_the_breach).unwrap_err();
        assert!(
            matches!(err, Denied::IssuedInTheFuture { .. }),
            "a future-dated token survived the rotation that was meant to end it: {err:?}"
        );
    }

    #[test]
    fn a_token_issued_before_the_floor_is_refused() {
        let floor = NOW_SECS - 100;
        let v = verifier(acme(None, floor));
        let stale = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", floor - 1, NOW_SECS + 600),
        );
        let err = v.verify(&stale).unwrap_err();
        assert!(
            matches!(err, Denied::IssuedBeforeFloor { iat, .. } if iat == floor - 1),
            "{err:?}"
        );
        // A token minted at the floor itself still works.
        let fresh = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", floor, NOW_SECS + 600),
        );
        assert!(v.verify(&fresh).is_ok());
    }

    #[test]
    fn a_token_signed_by_the_previous_key_is_accepted_during_rotation() {
        let v = verifier(acme(Some(OLD_KEY.to_vec()), 0));
        assert!(v.verify(&good_token(KEY)).is_ok(), "current key");
        assert!(v.verify(&good_token(OLD_KEY)).is_ok(), "previous key");

        // And once the old key is retired, its tokens stop working.
        let retired = verifier(acme(None, 0));
        assert!(matches!(
            retired.verify(&good_token(OLD_KEY)).unwrap_err(),
            Denied::BadSignature { .. }
        ));
    }

    #[test]
    fn a_token_claiming_alg_none_is_refused() {
        let v = verifier(acme(None, 0));
        let claims = claims_json("acme", "u1", NOW_SECS - 60, NOW_SECS + 600);
        // An unsigned token, the classic attack.
        let unsigned = format!(
            "{}.{}.",
            b64u(br#"{"alg":"none","typ":"JWT"}"#),
            b64u(claims.as_bytes())
        );
        assert!(matches!(
            v.verify(&unsigned).unwrap_err(),
            Denied::UnsupportedAlgorithm { ref alg } if alg == "none"
        ));
        // And an RS256 header over a valid HS256 signature: the algorithm is
        // ours to pick, not the header's.
        let confused = mint(KEY, r#"{"alg":"RS256"}"#, &claims);
        assert!(matches!(
            v.verify(&confused).unwrap_err(),
            Denied::UnsupportedAlgorithm { ref alg } if alg == "RS256"
        ));
    }

    #[test]
    fn a_subject_containing_a_separator_is_refused() {
        let v = verifier(acme(None, 0));
        // Signed by the real key: only the charset rule stands between this
        // token and the session id of every other subject of this tenant.
        for sub in ["web/u2", "u2/web/u3", "../u2", "u 2", "u\u{0}2"] {
            let token = mint(
                KEY,
                r#"{"alg":"HS256","typ":"JWT"}"#,
                &claims_json("acme", sub, NOW_SECS - 60, NOW_SECS + 600),
            );
            let err = v.verify(&token).unwrap_err();
            assert!(matches!(err, Denied::Malformed(_)), "{sub:?}: {err:?}");
        }
    }

    #[test]
    fn an_oversized_claim_is_refused() {
        let v = verifier(acme(None, 0));
        let long = "u".repeat(65);
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", &long, NOW_SECS - 60, NOW_SECS + 600),
        );
        assert!(matches!(
            v.verify(&token).unwrap_err(),
            Denied::Malformed(_)
        ));
        // 64 is still fine.
        let at_limit = "u".repeat(64);
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", &at_limit, NOW_SECS - 60, NOW_SECS + 600),
        );
        assert!(v.verify(&token).is_ok());
        assert!(!valid_claim(""));
    }

    #[test]
    fn a_token_for_an_unknown_tenant_is_refused() {
        let v = verifier(acme(None, 0));
        let token = mint(
            KEY,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("other", "u1", NOW_SECS - 60, NOW_SECS + 600),
        );
        assert!(matches!(
            v.verify(&token).unwrap_err(),
            Denied::UnknownTenant { .. }
        ));
    }

    #[test]
    fn a_malformed_token_is_refused_without_panicking() {
        let v = verifier(acme(None, 0));
        let good = good_token(KEY);
        let h = good.split('.').next().unwrap().to_string();
        // A payload that decodes to bytes that are not UTF-8 at all.
        let non_utf8 = format!("{h}.{}.{}", b64u(&[0xff, 0xfe, 0x80]), b64u(&[0u8; 32]));

        let cases: Vec<String> = vec![
            String::new(),
            ".".into(),
            "..".into(),
            "...".into(),
            "onlyone".into(),
            "two.parts".into(),
            format!("{good}.extra"),
            format!("{h}.{h}"),
            // Invalid base64url in each segment in turn.
            format!("!!!.{}.{}", b64u(b"{}"), b64u(&[0u8; 32])),
            format!("{h}.!!!.{}", b64u(&[0u8; 32])),
            format!(
                "{h}.{}.!!!",
                b64u(claims_json("acme", "u1", 1, 2).as_bytes())
            ),
            // Padded base64, which a compact JWS forbids.
            format!("{h}.{}.{}", "eyJhIjoxfQ==", b64u(&[0u8; 32])),
            non_utf8,
            // Well-formed JSON, wrong shape.
            mint(KEY, r#"{"typ":"JWT"}"#, "{}"),
            mint(KEY, r#"{"alg":"HS256"}"#, r#"{"iss":"acme"}"#),
            mint(KEY, r#"{"alg":"HS256"}"#, r#"["not","an","object"]"#),
            mint(KEY, "not json", "{}"),
            // A signature of the wrong length.
            format!(
                "{h}.{}.{}",
                b64u(claims_json("acme", "u1", 1, u64::MAX).as_bytes()),
                b64u(b"short")
            ),
        ];
        for case in cases {
            assert!(v.verify(&case).is_err(), "accepted {case:?}");
        }
    }
}
