//! A way in over TCP: one JSON object per line, one socket, many companies.
//!
//! The first channel in the tree whose messages were for more than one
//! session (multi-conversation plan Phase 3; findings §2.4). `CliChannel`
//! pins `"cli"` and `WithDesktop` joins the compose box to that same session;
//! here every connection names its session in its first line, and the
//! dispatcher (`nsengine::dispatch`) runs those sessions on their own tasks.
//! What `ns-app serve` listens on, and what a desktop app or a script
//! connects to.
//!
//! Everything below the socket — one queue per company, which windows hold a
//! session, the shutdown — belongs to `ns-channel-hub` and is shared with
//! every other way in. This crate owns the socket, the hello and the line
//! format, and nothing else.
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
//!   the session: replies for that session id go to every window holding it,
//!   and a later connection claiming the same id joins the others rather
//!   than displacing them. One conversation across a user's windows is the
//!   normal case — a second tab is not an impostor — and displacing would
//!   leave two tabs knocking each other offline in a loop (multi-tenant plan
//!   Phase 5, hazard H7).
//! - Then, from the client: `{"text":"…"}` per message. A line that is not
//!   that object is ignored, with one line on stderr naming the peer. EOF
//!   ends the connection's task and releases its slot.
//! - To the client: `{"session":"…","text":"…"}` per reply.
//!
//! The browser cannot open a TCP socket, so `ns-channel-http` speaks this
//! same vocabulary over a WebSocket: one client, two transports, one wire.
//!
//! # Shape
//!
//! Every connection is its own task (precedent: `serve_listener` in
//! `crates/pointer/src/agent.rs`; the lessons in `docs/windows-handoff.md`
//! §1). Each pushes into the hub queue of the company its hello named,
//! looked up per message — so a company whose engine is slow holds up its
//! own sockets and nobody else's, and a full queue is backpressure rather
//! than a drop. Dropping the listener closes this way in; the hub shuts down
//! once the last way in has gone. `send` is the hub's: a reply goes to every
//! window holding that session, whichever transport each arrived on.
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
//! | module       | what it owns                                        |
//! |--------------|-----------------------------------------------------|
//! | [`listener`] | the socket, the accept loop and one connection      |
//! | [`wire`]     | the line formats, and the bounds on a hello         |

mod listener;
mod wire;

// The names this crate was a single file under, kept exactly as they were:
// `nschannel_tcp::TcpChannel` is what the app imports, and moving the queues
// into the hub is not a reason to rewrite its imports.
pub use listener::{BindError, TcpChannel};
pub use nschannel_hub::{TenantChannel, INBOUND_DEPTH};
pub use wire::{HELLO_MAX, HELLO_TIMEOUT};
