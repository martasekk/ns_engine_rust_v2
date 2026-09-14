//! A channel with more than one session: TCP, one JSON object per line.
//!
//! The first channel in the tree whose `recv` yields `Incoming`s for more
//! than one session (multi-conversation plan Phase 3; findings §2.4).
//! `CliChannel` pins `"cli"` and `WithDesktop` joins the compose box to that
//! same session; here every connection names its session in its first line,
//! and the dispatcher (`nsengine::dispatch`) runs those sessions on their own
//! tasks. What `ns-app serve` listens on.
//!
//! # Wire
//!
//! One JSON object per line, both ways, UTF-8, `\n`-terminated.
//!
//! - The client's first line: `{"token":"…","session":"…"}`. What that line
//!   proves, and what session it stands for, is the resolver's to say
//!   (`nsidentity::IdentityResolver`): a refusal of any kind closes the
//!   connection with nothing sent back — a caller that failed the hello
//!   learns nothing about why, while the log names which refusal it was.
//!   Under the shared token the client still names its own session, and
//!   under a verified resolver the session is derived from the credential
//!   and the field is ignored, so no client can name itself into somebody
//!   else's conversation. A hello that does not arrive within the deadline,
//!   or that runs past [`HELLO_MAX`], is closed too. A valid hello *joins*
//!   the session:
//!   replies for that session id go to every connection holding it, and a
//!   later connection claiming the same id joins the others rather than
//!   displacing them. One conversation across a user's windows is the
//!   normal case — a second tab is not an impostor — and displacing would
//!   leave two tabs knocking each other offline in a loop (multi-tenant
//!   plan Phase 5, hazard H7).
//! - Then, from the client: `{"text":"…"}` per message. A line that is not
//!   that object is ignored, with one line on stderr naming the peer. EOF
//!   ends the connection's task and releases its slot.
//! - To the client: `{"session":"…","text":"…"}` per reply.
//!
//! # Shape
//!
//! Every connection is its own task (precedent: `serve_listener` in
//! `crates/pointer/src/agent.rs`; the lessons in `docs/windows-handoff.md`
//! §1). Each pushes into the queue of the company its hello named, looked
//! up per message, and one [`TenantChannel`] per company drains its own —
//! so a company whose engine is slow holds up its own sockets and nobody
//! else's, and a full queue is backpressure rather than a drop. A company
//! nobody is draining is announced once on
//! [`TcpChannel::next_active_tenant`], which is how a cold company gets an
//! engine and an evicted one gets it back. Dropping the listener is the
//! shutdown: the accept loop, every connection and every send waiting on a
//! full queue observe it, and each tenant channel reports `Closed` once it
//! has drained what it already had. `send` routes by session id to every
//! connection holding that session, each with its own bounded queue; a
//! reply for a session with no live connection, or for one that has stopped
//! reading, is logged and dropped — the engine's log already has it, and
//! that is what the log is for. One peer that stops reading therefore loses
//! its own replies and nobody else's. The connection cap is machine-wide,
//! not per socket.
//!
//! # Deliberately not here
//!
//! No TLS, and loopback by default: a non-loopback bind is refused unless
//! `allow_remote` says it was meant, and a resolver that proves nothing is
//! refused off loopback whatever `allow_remote` says. Who a connection is
//! belongs to `ns-identity`; this crate knows only that something turned a
//! hello into a session id.
//!
//! # Map
//!
//! This file is the map; each sibling owns one part of the listener:
//!
//! | module       | what it owns                                        |
//! |--------------|-----------------------------------------------------|
//! | [`listener`] | the socket, the accept loop and one connection      |
//! | [`queues`]   | one inbound queue per company, and the wake stream  |
//! | [`outbound`] | which connections hold a session, and delivery      |
//! | [`tenant`]   | one company's side of the listener                  |
//! | [`wire`]     | the line formats, and the bounds on a hello         |
//! | [`shared`]   | the state the accept loop and the channels share    |

mod listener;
mod outbound;
mod queues;
mod shared;
mod tenant;
mod wire;

// The names this crate was a single file under, kept exactly as they were:
// `nschannel_tcp::TcpChannel` is what the app imports, and a split is not a
// reason to rewrite its imports.
pub use listener::{BindError, TcpChannel};
pub use queues::INBOUND_DEPTH;
pub use tenant::TenantChannel;
pub use wire::{HELLO_MAX, HELLO_TIMEOUT};
