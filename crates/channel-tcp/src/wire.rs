//! The lines on the wire, and the bounds on the first of them.
//!
//! One JSON object per line each way. The hello's deadline and length live
//! here too: they are a property of that first line, not of the socket.

use std::time::Duration;

use nscore::SessionId;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;

/// How long a connection has to send its hello before it is closed. Without
/// it a silent peer would hold one of `max_connections` for ever.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// The most a hello may be. Applied with `take`, so a longer one is refused
/// without ever being buffered.
pub const HELLO_MAX: u64 = 8 * 1024;
#[derive(serde::Deserialize)]
pub(crate) struct Text {
    pub(crate) text: String,
}

#[derive(serde::Serialize)]
struct Reply<'a> {
    session: &'a str,
    text: &'a str,
}

/// Writes replies as they come until the sender goes (this connection's
/// entry was released) or the socket refuses one.
pub(crate) async fn write_replies(
    mut w: OwnedWriteHalf,
    session: SessionId,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(text) = rx.recv().await {
        let mut line = serde_json::to_string(&Reply {
            session: &session.0,
            text: &text,
        })
        .expect("two strings serialize");
        line.push('\n');
        if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
            break;
        }
    }
}
