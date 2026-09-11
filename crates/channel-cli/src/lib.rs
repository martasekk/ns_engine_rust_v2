use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Generic over reader/writer so tests can use in-memory buffers.
///
/// Each half sits behind its own lock because the engine's dispatcher keeps
/// one `recv` pending for the whole run while the session task calls `send`
/// through the same handle (multi-conversation plan Phase 2, D2.1). A `recv`
/// holds the reader's lock across the whole `read_line` and only that one,
/// so a reply never waits on the keyboard.
pub struct CliChannel<R, W> {
    reader: Mutex<R>,
    writer: Mutex<W>,
    session: SessionId,
    /// Whether a `you> ` prompt is on the screen waiting for a line.
    ///
    /// The dispatcher starts the next `recv` the moment a line arrives,
    /// before the turn has run, so the prompt cannot be written at the top
    /// of `recv` as it was under the serial loop — it would land ahead of
    /// the reply. `recv` prompts only when nothing is showing (the first
    /// wait), and `send` re-prompts after each reply: the same bytes in the
    /// same order as before — `you> `, the line, `bot> …`, `you> `.
    prompted: AtomicBool,
}

impl CliChannel<BufReader<tokio::io::Stdin>, tokio::io::Stdout> {
    pub fn new_stdio() -> Self {
        Self::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
    }
}

impl<R: AsyncBufRead + Unpin + Send + Sync, W: AsyncWrite + Unpin + Send + Sync> CliChannel<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            session: SessionId("cli".into()),
            prompted: AtomicBool::new(false),
        }
    }

    /// The session every line is delivered to. `cli` is the default and the
    /// only id this channel had; M12 T6.1 lets a metered run name its own,
    /// so its log is separable in the store from every other CLI session.
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = SessionId(session.into());
        self
    }

    pub fn into_writer(self) -> W {
        self.writer.into_inner()
    }

    async fn write(&self, bytes: &[u8]) -> Result<(), ChannelError> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(bytes)
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))
    }
}

#[async_trait]
impl<R: AsyncBufRead + Unpin + Send + Sync, W: AsyncWrite + Unpin + Send + Sync> Channel
    for CliChannel<R, W>
{
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        if !self.prompted.swap(true, Ordering::SeqCst) {
            self.write(b"you> ").await?;
        }

        let mut line = String::new();
        let n = self
            .reader
            .lock()
            .await
            .read_line(&mut line)
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))?;
        let text = line.trim();
        if n == 0 || text == "/quit" {
            return Err(ChannelError::Closed);
        }
        Ok(Incoming {
            session: self.session.clone(),
            text: text.to_string(),
        })
    }

    async fn send(&self, _session: &SessionId, text: &str) -> Result<(), ChannelError> {
        // The reply, then the prompt for the line that the pending `recv`
        // is already waiting for.
        self.write(format!("bot> {text}\nyou> ").as_bytes()).await?;
        self.prompted.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::Channel;

    #[tokio::test]
    async fn recv_reads_line_send_writes_line() {
        let input = b"hello world\n/quit\n".to_vec();
        let ch = CliChannel::new(tokio::io::BufReader::new(&input[..]), Vec::<u8>::new());
        let inc = ch.recv().await.unwrap();
        assert_eq!(inc.text, "hello world");
        ch.send(&inc.session, "hi there").await.unwrap();
        assert!(matches!(ch.recv().await, Err(nscore::ChannelError::Closed)));
        let out = String::from_utf8(ch.into_writer()).unwrap();
        assert!(out.contains("bot> hi there"));
    }

    /// The dispatcher restarts `recv` before the reply is written. The
    /// terminal must still read `you> `, the line, `bot> …`, `you> ` — the
    /// prompt after the reply, not ahead of it, and never twice.
    #[tokio::test]
    async fn the_prompt_follows_the_reply_when_recv_is_already_pending() {
        let (mut keyboard, stdin) = tokio::io::duplex(64);
        let ch = CliChannel::new(tokio::io::BufReader::new(stdin), Vec::<u8>::new());
        keyboard.write_all(b"hello\n").await.unwrap();
        let inc = ch.recv().await.unwrap();
        // The next wait is already in flight when the reply arrives.
        let mut waiting = ch.recv();
        assert!(still_pending(&mut waiting).await);
        ch.send(&inc.session, "hi").await.unwrap();
        drop(keyboard);
        assert!(matches!(waiting.await, Err(nscore::ChannelError::Closed)));
        let out = String::from_utf8(ch.into_writer()).unwrap();
        assert_eq!(out, "you> bot> hi\nyou> ");
    }

    /// Polls once; true if the future is still pending.
    async fn still_pending<F: std::future::Future + Unpin>(f: &mut F) -> bool {
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::pin::Pin::new(&mut *f).poll(cx).is_pending())
        })
        .await
    }
}
