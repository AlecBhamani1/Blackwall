//! How JSON-RPC messages reach an MCP server: newline-framed stdio or streamable HTTP.
use super::MAX_MESSAGE_BYTES;
use crate::tools::ToolError;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Dropping a transport ends its session.
pub(super) trait Transport: Send {
    fn send<'a>(&'a mut self, message: &'a Value) -> BoxFuture<'a, Result<(), ToolError>>;
    /// The next message from the server. An error means the server cannot be used again.
    fn recv(&mut self) -> BoxFuture<'_, Result<Value, ToolError>>;
    /// The protocol version agreed during initialization.
    fn negotiated(&mut self, _version: &str) {}
    /// A request ran out of time: stop whatever is still running for this server.
    fn abandon(&mut self) {}
}

fn unavailable() -> ToolError {
    ToolError::Execution("The MCP server stopped responding.".into())
}

/// A local server process speaking one JSON message per line.
pub(super) struct StdioTransport {
    pub reader: Box<dyn AsyncBufRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    #[cfg(any(unix, windows))]
    pub process: Option<crate::tools::ServerProcess>,
}

impl Transport for StdioTransport {
    fn send<'a>(&'a mut self, message: &'a Value) -> BoxFuture<'a, Result<(), ToolError>> {
        Box::pin(async move {
            let mut line = serde_json::to_vec(message).map_err(|_| ToolError::Arguments)?;
            line.push(b'\n');
            self.writer
                .write_all(&line)
                .await
                .map_err(|_| unavailable())?;
            self.writer.flush().await.map_err(|_| unavailable())
        })
    }

    fn recv(&mut self) -> BoxFuture<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            match read_message(&mut self.reader).await {
                Ok(Some(message)) => Ok(message),
                _ => Err(unavailable()),
            }
        })
    }

    fn abandon(&mut self) {
        // A late reply would arrive out of order, so the server is stopped and not reused.
        #[cfg(any(unix, windows))]
        self.process.take();
    }
}

/// One bounded line of JSON. Blank and non-JSON lines are skipped.
pub(super) async fn read_message(
    reader: &mut (dyn AsyncBufRead + Send + Unpin),
) -> std::io::Result<Option<Value>> {
    loop {
        let mut line = vec![];
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(None);
            }
            let (chunk, done) = match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&available[..end], true),
                None => (available, false),
            };
            if line.len() + chunk.len() > MAX_MESSAGE_BYTES {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            line.extend_from_slice(chunk);
            let used = chunk.len() + usize::from(done);
            reader.consume(used);
            if done {
                break;
            }
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&line) {
            if message.is_object() {
                return Ok(Some(message));
            }
        }
    }
}
