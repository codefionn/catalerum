//! The outbound WebSocket client: connect, announce, serve ops, reconnect.
//!
//! The daemon dials the server (never the reverse), so it works behind NAT with no
//! inbound ports. After the handshake it sends its [`AgentToServer::Hello`], then
//! serves each [`ServerToAgent::Request`] concurrently (a long `exec` never blocks
//! heartbeats or other ops) and answers [`ServerToAgent::Ping`] with a `Pong`. A
//! dropped connection reconnects with capped exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use catalerum_core::computer::{AgentToServer, OpResponse, ServerToAgent};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::ops::AgentState;

/// Run the connect/serve/reconnect loop until the process is asked to stop.
pub async fn run(state: Arc<AgentState>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&state).await {
            Ok(()) => {
                tracing::info!("disconnected; reconnecting shortly");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                // `{:#}` prints the whole context chain — the TLS/DNS/HTTP root
                // cause, not just our outermost "websocket connect" label.
                tracing::warn!(error = %format_args!("{e:#}"), "connection failed; retrying in {:?}", backoff);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One connection lifetime: handshake → Hello → serve until close/error.
async fn connect_once(state: &Arc<AgentState>) -> Result<()> {
    let url = state.config.connect_url();
    tracing::info!("connecting to {}", redact(&url));
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| match e {
            // A 2xx means something served a normal webpage instead of the WS
            // upgrade — almost always the web UI's origin pasted as server_url.
            tokio_tungstenite::tungstenite::Error::Http(ref resp) if resp.status().is_success() => {
                anyhow::anyhow!(
                    "server answered {} with a plain page instead of a WebSocket upgrade — \
                     server_url likely points at the web UI; use the API origin \
                     (e.g. https://api.<your-domain>)",
                    resp.status()
                )
            }
            e => anyhow::Error::new(e).context("websocket connect (check server_url and token)"),
        })?;
    tracing::info!("connected; announcing capabilities");
    let (mut write, mut read) = ws.split();

    // Announce the machine's capabilities first.
    let hello = AgentToServer::Hello {
        capabilities: state.capabilities(),
    };
    write
        .send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await
        .context("sending hello")?;

    // One outbound channel funnels responses + pongs from concurrent op tasks to
    // the single socket writer.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = read.next().await {
        let msg = msg.context("reading a frame")?;
        match msg {
            Message::Text(t) => dispatch(state, &t, &tx),
            Message::Binary(b) => dispatch(state, &String::from_utf8_lossy(&b), &tx),
            Message::Ping(p) => {
                let _ = tx.send(Message::Pong(p));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    Ok(())
}

/// Parse and act on one inbound server frame.
fn dispatch(state: &Arc<AgentState>, text: &str, tx: &mpsc::UnboundedSender<Message>) {
    let frame: ServerToAgent = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "ignoring unparseable server frame");
            return;
        }
    };
    match frame {
        ServerToAgent::Ping => {
            let _ = tx.send(encode(&AgentToServer::Pong));
        }
        ServerToAgent::Request { id, op } => {
            let verb = op.verb();
            tracing::debug!(request = %id, op = verb, "serving op");
            let state = state.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let resp = match state.execute(op).await {
                    Ok(data) => OpResponse::ok(id, data),
                    Err(e) => {
                        tracing::info!(op = verb, "op refused/failed: {e}");
                        OpResponse::err(id, e)
                    }
                };
                let _ = tx.send(encode(&AgentToServer::Response(resp)));
            });
        }
    }
}

/// Encode an outbound frame as a text message (falls back to a Close-less no-op on
/// the impossible serialization error).
fn encode(frame: &AgentToServer) -> Message {
    match serde_json::to_string(frame) {
        Ok(s) => Message::Text(s.into()),
        Err(_) => Message::Text(String::new().into()),
    }
}

/// Strip the token query so a connect URL is safe to log.
fn redact(url: &str) -> String {
    match url.split_once("?token=") {
        Some((base, _)) => format!("{base}?token=<redacted>"),
        None => url.to_string(),
    }
}
