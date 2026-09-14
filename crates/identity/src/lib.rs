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
//!
//! This file is the map. Each sibling owns one layer, and knows nothing of the
//! layer above it:
//!
//! | module         | what it owns                                       |
//! |----------------|----------------------------------------------------|
//! | [`vocabulary`] | identity, trust, claims, refusals, the two traits   |
//! | [`hmac`]       | HMAC-SHA256 and constant-time equality             |
//! | [`base64url`]  | decoding one segment of a compact JWS              |
//! | [`jwt`]        | is this token genuine, and inside its bounds       |
//! | [`resolvers`]  | what identity a genuine credential stands for      |

mod base64url;
mod hmac;
mod jwt;
mod resolvers;
mod vocabulary;

#[cfg(test)]
mod minting;

// The crate was a single file until it was five, and a split is not a reason
// to rewrite the imports of the app, the TCP channel or their tests: every
// name below is still `nsidentity::X`.
pub use hmac::hmac_sha256;
pub use jwt::{Hs256Verifier, TenantAuth, MAX_CLOCK_SKEW_SECS, MAX_LIFETIME_SECS};
pub use resolvers::SharedTokenResolver;
pub use vocabulary::{
    session_id, valid_claim, Claims, Denied, Hello, Identity, IdentityResolver, TokenVerifier,
    Trust,
};
