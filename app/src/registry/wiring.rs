//! The three edges the registry has to the world, each behind a trait.
//!
//! A build, a listener and a run. Production passes the real thing through
//! every one of them; a test stands on the other side and drives the
//! registry by hand, which is why none of the three is a concrete type
//! here.

use crate::factory::{BuiltTenant, StartupError};
use nscore::Channel;
use nsengine::dispatch::{Dispatcher, ShardSlots, TurnFailure};
use nsengine::turn::EngineError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A future that has been boxed to cross a trait boundary. No `futures`
/// dependency for one alias.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a company is built, given its id and the channel its engine drains.
///
/// The channel comes from the registry rather than from the closure because
/// the registry is what must drop it at the right moment; a builder that
/// fetched its own would leave the eviction ordering to whoever wrote the
/// closure.
pub(crate) type BuildTenant<R> = Arc<
    dyn Fn(String, Arc<dyn Channel>) -> BoxFuture<'static, Result<R, StartupError>> + Send + Sync,
>;

/// The listener side, as the registry needs it.
///
/// Two methods, both of them `TcpChannel`'s. A trait rather than the
/// concrete type so a test can drive the wake stream by hand instead of
/// binding a socket and opening connections to provoke one.
pub(crate) trait TenantSource: Send + Sync + 'static {
    /// This company's inbound queue, or `None` when it has none or somebody
    /// already drains it.
    fn tenant_channel(&self, tenant: &str) -> Option<Arc<dyn Channel>>;
    /// The next company with messages and no engine draining them. `None`
    /// when the listener is gone, which is the shard's shutdown.
    ///
    /// Cancel-safe: the registry polls this inside a `select!` and drops the
    /// future when a sweep or a shard failure wins the race.
    fn next_active_tenant(&self) -> BoxFuture<'_, Option<String>>;
}

/// The hub, which is where every way in meets: the socket, the browser's
/// WebSocket and each platform's webhook all fill the same company queues,
/// so the registry watches one thing and not one per listener.
impl TenantSource for nschannel_hub::Hub {
    fn tenant_channel(&self, tenant: &str) -> Option<Arc<dyn Channel>> {
        let channel = nschannel_hub::Hub::tenant_channel(self, tenant)?;
        Some(channel as Arc<dyn Channel>)
    }

    fn next_active_tenant(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(nschannel_hub::Hub::next_active_tenant(self))
    }
}

/// What a built company does when it is let go: it runs until it stops.
///
/// `BuiltTenant` is the production implementor and its `run` is exactly the
/// dispatcher `main` builds today, with serve's isolating failure policy and
/// the process-wide shard ceiling added.
pub(crate) trait RunTenant: Send + 'static {
    fn run(self, shard: ShardSlots) -> BoxFuture<'static, Result<(), EngineError>>;
}

impl RunTenant for BuiltTenant {
    fn run(self, shard: ShardSlots) -> BoxFuture<'static, Result<(), EngineError>> {
        // A shard: one session's failure ends that session, and a failure
        // wide enough to end this company's dispatcher is caught by the
        // registry rather than by the process.
        Box::pin(
            Dispatcher::with_failure_policy(
                self.engine,
                self.channel,
                self.worker_slots,
                TurnFailure::Isolate,
            )
            .with_shard_slots(shard)
            .run(),
        )
    }
}
