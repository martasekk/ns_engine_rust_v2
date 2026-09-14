//! The ways in that speak HTTP: a browser's chat window, a request from a
//! desktop app or a script, and a platform's signed webhook.
//!
//! `ns-channel-tcp` is one JSON object per line on a socket, which is a fine
//! protocol for a program and no protocol at all for a browser — a page
//! cannot open a TCP connection. This crate is the same conversation, in the
//! three envelopes the rest of the world actually uses, on one port:
//!
//! | route | who it is for | how a reply gets back |
//! |---|---|---|
//! | `GET /chat` (upgraded) | a web chat window, a desktop app | over the same WebSocket, as it happens |
//! | `POST /v1/messages` | a script, a back end, anything with `curl` | in the response to that request |
//! | `POST /hooks/<platform>` | WhatsApp, and platforms shaped like it | a call to the platform's API |
//! | `GET /healthz` | whatever watches the process | — |
//!
//! Every one of them ends in the same place: [`nschannel_hub`], one queue per
//! company and one set of windows per session. So a company reached over
//! WhatsApp and a company reached through a web widget are the same kind of
//! thing to the engine, which cannot tell them apart and has no field with
//! which to ask.
//!
//! # Why webhooks are their own shape
//!
//! A platform does not hold a connection open. It posts when its user says
//! something, wants `200` in milliseconds, and expects the reply as a call
//! to its own API some seconds later. Three consequences, each handled in
//! [`hooks`]: the sender's id is an identifier and not a credential, so what
//! is verified is a signature over the raw bytes; a retry must not run the
//! turn twice, so message ids are remembered across restarts ([`seen`]); and
//! there is no socket to reply on, so the way back is a task holding a hub
//! sink and calling the platform's API.
//!
//! # What is deliberately not here
//!
//! **TLS.** Terminate it in front — a reverse proxy, or the platform's own
//! tunnel. The same rule as the socket's: a non-loopback bind is refused
//! unless it was meant, and a resolver that proves nothing is refused off
//! loopback whatever else is configured.
//!
//! **Sessions of its own.** Who a caller is belongs to `ns-identity`; this
//! crate knows only that something turned a hello, a bearer token or a
//! signature into an identity.
//!
//! **A UI.** `examples/web-chat.html` is a page that speaks `/chat` in about
//! a hundred lines, to be read and thrown away, not served from here.
//!
//! # Map
//!
//! | module        | what it owns                                       |
//! |---------------|----------------------------------------------------|
//! | [`server`]    | the socket, the routes, CORS and the bounds        |
//! | [`http`]      | one bounded request in, one response out. Public,  |
//! |               | because `ns-app admin` serves its page on the same |
//! |               | parser rather than writing a second one            |
//! | [`ws`]        | the upgrade, and frames both ways                  |
//! | [`chat`]      | a window that holds a session while it is open     |
//! | [`messages`]  | one message, one reply, nothing held open          |
//! | [`hooks`]     | a platform's delivery, and the way back to it      |
//! | [`platform`]  | what a platform adapter is, and the way out        |
//! | [`whatsapp`]  | one such adapter, and the shape of the next        |
//! | [`seen`]      | which messages were already answered               |
//! | [`wire`]      | the JSON shapes, shared with the socket            |

mod chat;
mod hooks;
pub mod http;
mod messages;
pub mod platform;
mod seen;
mod server;
pub mod whatsapp;
mod wire;
mod ws;

pub use hooks::PlatformEndpoint;
pub use seen::SeenIds;
pub use server::{
    BindError, HttpChannel, HttpConfig, Origins, CHAT_PATH, HEALTH_PATH, HOOKS_PREFIX,
    MESSAGES_PATH,
};
