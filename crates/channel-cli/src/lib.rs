use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Generic over reader/writer so tests can use in-memory buffers.
pub struct CliChannel<R, W> {
    reader: R,
    writer: W,
    session: SessionId,
}

impl CliChannel<BufReader<tokio::io::Stdin>, tokio::io::Stdout> {
    pub fn new_stdio() -> Self {
        Self::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
    }
}

impl<R: AsyncBufRead + Unpin + Send + Sync, W: AsyncWrite + Unpin + Send + Sync> CliChannel<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, session: SessionId("cli".into()) }
    }

    pub fn into_writer(self) -> W {
        self.writer
    }
}

#[async_trait]
impl<R: AsyncBufRead + Unpin + Send + Sync, W: AsyncWrite + Unpin + Send + Sync> Channel for CliChannel<R, W> {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        self.writer
            .write_all(b"you> ")
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))?;
        self.writer.flush().await.map_err(|e| ChannelError::Io(e.to_string()))?;

        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))?;
        let text = line.trim();
        if n == 0 || text == "/quit" {
            return Err(ChannelError::Closed);
        }
        Ok(Incoming { session: self.session.clone(), text: text.to_string() })
    }

    async fn send(&mut self, _session: &SessionId, text: &str) -> Result<(), ChannelError> {
        self.writer
            .write_all(format!("bot> {text}\n").as_bytes())
            .await
            .map_err(|e| ChannelError::Io(e.to_string()))?;
        self.writer.flush().await.map_err(|e| ChannelError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::Channel;

    #[tokio::test]
    async fn recv_reads_line_send_writes_line() {
        let input = b"hello world\n/quit\n".to_vec();
        let mut ch = CliChannel::new(tokio::io::BufReader::new(&input[..]), Vec::<u8>::new());
        let inc = ch.recv().await.unwrap();
        assert_eq!(inc.text, "hello world");
        ch.send(&inc.session, "hi there").await.unwrap();
        assert!(matches!(ch.recv().await, Err(nscore::ChannelError::Closed)));
        let out = String::from_utf8(ch.into_writer()).unwrap();
        assert!(out.contains("bot> hi there"));
    }
}
