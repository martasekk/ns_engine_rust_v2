//! Who is on the other end of a connection, and the credential that proves it.
//!
//! A channel turns what arrives into an [`Identity`]; everything above it works
//! from the identity and never sees the credential. The arrival type is the
//! trait's parameter, not a shared enum: a bearer token in a TCP hello and an
//! HMAC over a webhook's raw body are different shapes, and a channel should
//! never have to name another channel's arrival to name its own resolver.
//!
//! This lives outside `ns-core` on purpose. Every crate in the workspace
//! depends on core; nothing inside the engine resolves an identity.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use nscore::SessionId;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;

/// Who a connection speaks for, once what it presented has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The company the session belongs to. Also the first segment of the
    /// session id, so two companies cannot collide in one flat namespace.
    pub tenant: String,
    pub session: SessionId,
    pub trust: Trust,
}

/// How much the harness may believe about an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Nobody proved anything: loopback development, or a shared token.
    Anonymous,
    /// A credential signed by the tenant's own key checked out.
    Verified,
    /// A platform (WhatsApp, Slack) vouched for the end user itself.
    PlatformVerified,
}

/// The registered JWT claims this harness reads. `iss` is the tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    /// Issued at, unix seconds.
    pub iat: u64,
    /// Expires at, unix seconds.
    pub exp: u64,
}

/// Why a credential was refused.
///
/// These variants exist to say precisely what went wrong *in a log line*. A
/// caller must never echo one to a client, nor render one into a wire error:
/// telling an attacker whether the tenant is unknown, the signature wrong or
/// the token merely stale hands them an oracle. Log the `Denied` and send back
/// one flat refusal.
#[derive(Debug, thiserror::Error)]
pub enum Denied {
    #[error("malformed token: {0}")]
    Malformed(&'static str),
    #[error("unsupported algorithm {alg:?}: only HS256 is accepted")]
    UnsupportedAlgorithm { alg: String },
    #[error("no signing key configured for tenant {tenant:?}")]
    UnknownTenant { tenant: String },
    #[error("signature matches no active key of tenant {tenant:?}")]
    BadSignature { tenant: String },
    #[error("expired at {exp}, now {now} (unix seconds)")]
    Expired { exp: u64, now: u64 },
    #[error("issued at {iat}, before tenant floor {floor} (unix seconds)")]
    IssuedBeforeFloor { iat: u64, floor: u64 },
    #[error("issued at {iat}, ahead of now {now} by more than {skew}s (unix seconds)")]
    IssuedInTheFuture { iat: u64, now: u64, skew: u64 },
    #[error("claims a life of {lifetime}s, more than the {max}s a token may hold")]
    LifetimeTooLong { lifetime: u64, max: u64 },
    #[error("presented token does not match the shared token of tenant {tenant:?}")]
    WrongSharedToken { tenant: String },
}

/// Turns one channel's arrival into an identity.
///
/// `A` is that channel's own arrival type ([`Hello`] for the TCP hello, a
/// signed request for a webhook). The future is boxed by `async_trait` because
/// channels hold a `dyn IdentityResolver<A>`.
#[async_trait::async_trait]
pub trait IdentityResolver<A: Send + 'static>: Send + Sync {
    async fn resolve(&self, arrival: A) -> Result<Identity, Denied>;
    /// True when this resolver proves nothing and the listener must therefore
    /// refuse to serve anything but loopback.
    fn loopback_only(&self) -> bool {
        false
    }
    /// For the startup banner and the refusal log line.
    fn describe(&self) -> &str;
}

/// Checks a bearer token a client presents. The seam between "is this token
/// genuine" and "what identity does it stand for", so that a later resolver
/// over a different arrival can reuse the token check unchanged.
pub trait TokenVerifier: Send + Sync {
    fn verify(&self, token: &str) -> Result<Claims, Denied>;
}

/// What a TCP client sends first.
///
/// `session` is honoured only by [`SharedTokenResolver`], which proves nothing
/// anyway and is loopback-only. A verified resolver derives the session from
/// the token and ignores this field, so a client cannot name itself into
/// somebody else's conversation. The field stays on the wire so that the
/// format, and every existing test of it, is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub token: String,
    #[serde(default)]
    pub session: Option<String>,
}

/// The one place that knows how a verified session id is shaped.
///
/// `/` is the namespace separator, which is exactly why [`valid_claim`] refuses
/// a claim containing one: a `sub` of `"b/web/c"` would otherwise let a token
/// for tenant `a` name the session of any subject it liked.
pub fn session_id(iss: &str, sub: &str) -> SessionId {
    SessionId(format!("{iss}/web/{sub}"))
}

/// `[A-Za-z0-9_.:-]{1,64}`, the charset a claim may use to become part of a
/// session id.
pub fn valid_claim(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

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

#[async_trait::async_trait]
impl IdentityResolver<Hello> for Hs256Verifier {
    async fn resolve(&self, arrival: Hello) -> Result<Identity, Denied> {
        let claims = self.verify(&arrival.token)?;
        Ok(Identity {
            session: session_id(&claims.iss, &claims.sub),
            tenant: claims.iss,
            trust: Trust::Verified,
        })
    }

    fn describe(&self) -> &str {
        "JWT HS256"
    }
}

/// One token for everybody, for loopback development and the existing tests.
/// It proves nothing, so it is the only resolver that lets a client name its
/// own session.
pub struct SharedTokenResolver {
    token: String,
    tenant: String,
}

impl SharedTokenResolver {
    pub fn new(token: impl Into<String>, tenant: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            tenant: tenant.into(),
        }
    }
}

#[async_trait::async_trait]
impl IdentityResolver<Hello> for SharedTokenResolver {
    async fn resolve(&self, arrival: Hello) -> Result<Identity, Denied> {
        if !ct_eq(self.token.as_bytes(), arrival.token.as_bytes()) {
            return Err(Denied::WrongSharedToken {
                tenant: self.tenant.clone(),
            });
        }
        // The client is the only thing that can name a session under this
        // resolver, so a hello that names none is refused rather than given a
        // made-up id: substituting one would silently join every anonymous
        // client to a single conversation.
        let session = arrival
            .session
            .filter(|s| !s.is_empty())
            .ok_or(Denied::Malformed(
                "the shared token needs a session id in the hello",
            ))?;
        Ok(Identity {
            tenant: self.tenant.clone(),
            session: SessionId(session),
            trust: Trust::Anonymous,
        })
    }

    fn loopback_only(&self) -> bool {
        true
    }

    fn describe(&self) -> &str {
        "shared token (loopback only)"
    }
}

enum Segment {
    Header,
    Payload,
    Signature,
}

fn b64url(segment: &str, which: Segment) -> Result<Vec<u8>, Denied> {
    URL_SAFE_NO_PAD.decode(segment).map_err(|_| match which {
        Segment::Header => Denied::Malformed("header is not base64url"),
        Segment::Payload => Denied::Malformed("payload is not base64url"),
        Segment::Signature => Denied::Malformed("signature is not base64url"),
    })
}

/// HMAC-SHA256 (RFC 2104), proven against the RFC 4231 vectors in the tests.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Equality that does not stop at the first differing byte. The lengths are
/// not secret; the bytes are.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_SECS: u64 = 1_700_000_000;
    const KEY: &[u8] = b"current-signing-key";
    const OLD_KEY: &[u8] = b"previous-signing-key";

    /// This crate has no async runtime of its own and needs none: a resolver's
    /// future does no I/O yet, so the test drives it to completion by hand.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        // Safety: `fut` is owned here and never moved again.
        let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        loop {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    fn b64u(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A compact JWS over any header and payload, however implausible: the
    /// tests need to mint tokens a well-behaved client never would.
    fn mint(key: &[u8], header: &str, payload: &str) -> String {
        let signed = format!("{}.{}", b64u(header.as_bytes()), b64u(payload.as_bytes()));
        let sig = hmac_sha256(key, signed.as_bytes());
        format!("{signed}.{}", b64u(&sig))
    }

    fn claims_json(iss: &str, sub: &str, iat: u64, exp: u64) -> String {
        format!(r#"{{"iss":"{iss}","sub":"{sub}","iat":{iat},"exp":{exp}}}"#)
    }

    fn good_token(key: &[u8]) -> String {
        mint(
            key,
            r#"{"alg":"HS256","typ":"JWT"}"#,
            &claims_json("acme", "u1", NOW_SECS - 60, NOW_SECS + 600),
        )
    }

    fn verifier(auth: TenantAuth) -> Hs256Verifier {
        Hs256Verifier::with_clock(
            HashMap::from([("acme".to_string(), auth)]),
            Box::new(|| NOW_SECS),
        )
    }

    fn acme(previous: Option<Vec<u8>>, iat_floor: u64) -> TenantAuth {
        TenantAuth {
            current: KEY.to_vec(),
            previous,
            iat_floor,
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // RFC 4231 section 4, cases 1-4, 6 and 7. Case 5 is a truncation case
        // and is checked on its first 128 bits.
        let cases: &[(Vec<u8>, Vec<u8>, &str)] = &[
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                (1u8..=25).collect(),
                vec![0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                vec![0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.".to_vec(),
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (i, (key, data, want)) in cases.iter().enumerate() {
            assert_eq!(
                hex(&hmac_sha256(key, data)),
                *want,
                "RFC 4231 case {}",
                i + 1
            );
        }
        // Case 5, truncated to 128 bits.
        assert_eq!(
            hex(&hmac_sha256(&[0x0c; 20], b"Test With Truncation")[..16]),
            "a3b6167473100ee06e0c796c2955552b"
        );
    }

    #[test]
    fn a_valid_token_resolves_to_a_verified_identity() {
        let v = verifier(acme(None, 0));
        let id = block_on(v.resolve(Hello {
            token: good_token(KEY),
            session: None,
        }))
        .expect("valid token");
        assert_eq!(
            id,
            Identity {
                tenant: "acme".into(),
                session: SessionId("acme/web/u1".into()),
                trust: Trust::Verified,
            }
        );
        assert!(!v.loopback_only());
    }

    #[test]
    fn the_jwt_resolver_ignores_a_client_chosen_session() {
        let v = verifier(acme(None, 0));
        let id = block_on(v.resolve(Hello {
            token: good_token(KEY),
            session: Some("acme/web/somebody-else".into()),
        }))
        .expect("valid token");
        assert_eq!(id.session, SessionId("acme/web/u1".into()));
    }

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

    #[test]
    fn the_shared_token_resolver_is_loopback_only_and_keeps_the_clients_session() {
        let r = SharedTokenResolver::new("dev-token", "local");
        assert!(r.loopback_only());
        let id = block_on(r.resolve(Hello {
            token: "dev-token".into(),
            session: Some("a".into()),
        }))
        .expect("the shared token");
        assert_eq!(
            id,
            Identity {
                tenant: "local".into(),
                session: SessionId("a".into()),
                trust: Trust::Anonymous,
            }
        );

        let err = block_on(r.resolve(Hello {
            token: "wrong".into(),
            session: Some("a".into()),
        }))
        .unwrap_err();
        assert!(matches!(err, Denied::WrongSharedToken { .. }), "{err:?}");
    }

    #[test]
    fn the_shared_resolver_refuses_a_missing_session() {
        // Nothing but the client can name a session here, so there is no id to
        // substitute: a hello without one is refused, exactly as the TCP
        // channel has always refused an empty one.
        let r = SharedTokenResolver::new("dev-token", "local");
        for session in [None, Some(String::new())] {
            let err = block_on(r.resolve(Hello {
                token: "dev-token".into(),
                session,
            }))
            .unwrap_err();
            assert!(matches!(err, Denied::Malformed(_)), "{err:?}");
        }
    }
}
