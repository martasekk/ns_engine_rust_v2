//! The words the rest of the crate is spoken in.
//!
//! An identity, how much of it is believed, the claims it was read out of, and
//! the two traits a channel holds. Nothing here knows how a signature is
//! checked or what a token looks like.

use nscore::SessionId;
use serde::{Deserialize, Serialize};

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
/// `session` is honoured only by [`SharedTokenResolver`](crate::SharedTokenResolver),
/// which proves nothing anyway and is loopback-only. A verified resolver
/// derives the session from the token and ignores this field, so a client
/// cannot name itself into somebody else's conversation. The field stays on
/// the wire so that the format, and every existing test of it, is unchanged.
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
