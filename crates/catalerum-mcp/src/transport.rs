//! The MCP **stdio** transport (SOUL §26): newline-delimited JSON-RPC. Each line
//! read from the input is one request; each response is written as one line. A
//! parse failure yields a JSON-RPC parse-error response (id `null`) rather than
//! killing the stream; a notification produces no line.
//!
//! Generic over the reader/writer so it drives real stdin/stdout in the binary and
//! in-memory pipes in tests.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::{JsonRpcResponse, PARSE_ERROR};
use crate::server::McpServer;
use serde_json::Value;

/// Drive `server` over a line-delimited JSON-RPC stream until the reader hits EOF.
///
/// # Errors
/// Propagates an I/O error from reading the input or writing a response.
pub async fn serve<R, W>(server: &McpServer, reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str(&line) {
            Ok(req) => server.handle(req).await,
            Err(e) => Some(JsonRpcResponse::error(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
            )),
        };
        if let Some(response) = response {
            let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }
    }
    Ok(())
}
