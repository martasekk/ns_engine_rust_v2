//! Where every way in meets the engines: one queue per company, one set of
//! windows per session, and nothing at all about credentials or sockets.
//!
//! A company's engine drains one [`TenantChannel`]. What fills it is not the
//! hub's business: a TCP socket (`ns-channel-tcp`), a browser's WebSocket or
//! a platform's signed webhook (`ns-channel-http`) all put messages in the
//! same queue and take replies out of the same map. That is the whole point
//! of this crate existing — the multi-tenant plan's D6 puts identity at the
//! edge, one resolver per arrival, and everything downstream reads only what
//! the resolver produced. The layer downstream of *every* edge is this one,
//! so it holds no resolver, no address and no wire format.
//!
//! # What an ingress does with it
//!
//! 1. [`Hub::ingress`] once, and hold the handle: the last one dropped is
//!    the shutdown, so a process with a socket and a webhook endpoint stays
//!    up until both are gone.
//! 2. Per arrival: resolve an identity its own way, then
//!    [`Hub::attach`] a sink for that session and drain the receiver it
//!    hands back — that is where replies come out, as plain text, in
//!    whatever shape the ingress's own wire calls for.
//! 3. Per message: [`Hub::sender_for`] the company, then
//!    [`Hub::hand_over`]. A full queue waits, which is backpressure on that
//!    company's own callers and on nobody else's.
//! 4. Around every other await: [`Hub::is_closed`] and [`Hub::notified`], in
//!    that order, so a shutdown is never missed by a task parked on a read.
//!
//! # What the registry does with it
//!
//! [`Hub::next_active_tenant`] names a company with messages and no engine
//! draining them, and [`Hub::tenant_channel`] hands over that company's
//! queue exactly once. Both were `TcpChannel`'s before there was a second
//! way in, and the app's `TenantSource` still names them.
//!
//! # Map
//!
//! | module       | what it owns                                        |
//! |--------------|-----------------------------------------------------|
//! | [`hub`]      | the shared state, the ingress count and the shutdown |
//! | [`queues`]   | one inbound queue per company, and the wake stream   |
//! | [`outbound`] | which sinks hold a session, and delivery             |
//! | [`tenant`]   | one company's side of the hub                        |

mod hub;
mod outbound;
mod queues;
mod tenant;

pub use hub::{Hub, Ingress};
pub use outbound::{SinkHandle, OUTBOUND_DEPTH};
pub use queues::INBOUND_DEPTH;
pub use tenant::TenantChannel;
