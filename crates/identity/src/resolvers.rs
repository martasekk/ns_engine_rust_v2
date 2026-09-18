//! What identity a credential stands for, once it is genuine.
//!
//! Both resolvers here take the TCP channel's [`Hello`]; a resolver over a
//! different arrival would sit beside them and reuse the same token check.

use nscore::SessionId;

use crate::hmac::ct_eq;
use crate::jwt::Hs256Verifier;
use crate::vocabulary::{
    session_id, Denied, Hello, Identity, IdentityResolver, TokenVerifier, Trust,
};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minting::*;

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
