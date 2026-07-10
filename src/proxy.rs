use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as ClientMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

/// 16 MiB frames / 64 MiB messages, enough for large CDP payloads such as
/// screencast frames while still bounding memory per connection.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub type Upstream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why the proxy loop ended. Used for logging and for picking the close
/// code sent to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    ClientDisconnected,
    ClientError,
    ChromiumClosed,
    ChromiumError,
    SessionTimeout,
    ServerShutdown,
}

impl CloseReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::ClientDisconnected => "client_disconnected",
            CloseReason::ClientError => "client_error",
            CloseReason::ChromiumClosed => "chromium_closed",
            CloseReason::ChromiumError => "chromium_error",
            CloseReason::SessionTimeout => "session_timeout",
            CloseReason::ServerShutdown => "server_shutdown",
        }
    }
}

fn client_to_upstream(message: ClientMessage) -> UpstreamMessage {
    match message {
        ClientMessage::Text(text) => {
            // The payload was already validated as UTF-8 by axum; converting
            // through Bytes avoids copying it. Cloning Bytes only bumps a
            // refcount, keeping the impossible error arm lossless.
            let bytes: bytes::Bytes = text.into();
            match bytes.clone().try_into() {
                Ok(utf8) => UpstreamMessage::Text(utf8),
                Err(_) => UpstreamMessage::Binary(bytes),
            }
        }
        ClientMessage::Binary(data) => UpstreamMessage::Binary(data),
        ClientMessage::Ping(data) => UpstreamMessage::Ping(data),
        ClientMessage::Pong(data) => UpstreamMessage::Pong(data),
        ClientMessage::Close(frame) => UpstreamMessage::Close(frame.map(|f| TsCloseFrame {
            code: f.code.into(),
            reason: {
                let bytes: bytes::Bytes = f.reason.into();
                bytes.try_into().unwrap_or_default()
            },
        })),
    }
}

fn upstream_to_client(message: UpstreamMessage) -> Option<ClientMessage> {
    match message {
        UpstreamMessage::Text(text) => {
            let bytes: bytes::Bytes = text.into();
            bytes.try_into().ok().map(ClientMessage::Text)
        }
        UpstreamMessage::Binary(data) => Some(ClientMessage::Binary(data)),
        UpstreamMessage::Ping(data) => Some(ClientMessage::Ping(data)),
        UpstreamMessage::Pong(data) => Some(ClientMessage::Pong(data)),
        UpstreamMessage::Close(frame) => Some(ClientMessage::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: {
                let bytes: bytes::Bytes = f.reason.into();
                bytes.try_into().unwrap_or_default()
            },
        }))),
        // Raw frames are an implementation detail; never produced in
        // standard read mode.
        UpstreamMessage::Frame(_) => None,
    }
}

/// Relays frames between the client and Chromium without interpreting CDP.
///
/// The select loop awaits each forwarded send before reading the next
/// message from the same side, which propagates backpressure naturally.
/// Payloads are `Bytes` handles, so no data is copied while relaying.
pub async fn run(
    client: WebSocket,
    upstream: Upstream,
    session_timeout: Duration,
    cancel: &CancellationToken,
) -> CloseReason {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    let timeout = tokio::time::sleep(session_timeout);
    tokio::pin!(timeout);

    let reason = loop {
        tokio::select! {
            message = client_rx.next() => match message {
                Some(Ok(message)) => {
                    let is_close = matches!(message, ClientMessage::Close(_));
                    if upstream_tx.send(client_to_upstream(message)).await.is_err() {
                        break CloseReason::ChromiumError;
                    }
                    if is_close {
                        break CloseReason::ClientDisconnected;
                    }
                }
                Some(Err(_)) => break CloseReason::ClientError,
                None => break CloseReason::ClientDisconnected,
            },
            message = upstream_rx.next() => match message {
                Some(Ok(message)) => {
                    let is_close = matches!(message, UpstreamMessage::Close(_));
                    if let Some(converted) = upstream_to_client(message)
                        && client_tx.send(converted).await.is_err()
                    {
                        break CloseReason::ClientDisconnected;
                    }
                    if is_close {
                        break CloseReason::ChromiumClosed;
                    }
                }
                Some(Err(_)) => break CloseReason::ChromiumError,
                None => break CloseReason::ChromiumClosed,
            },
            _ = &mut timeout => break CloseReason::SessionTimeout,
            _ = cancel.cancelled() => break CloseReason::ServerShutdown,
        }
    };

    // Best-effort graceful close towards the client with a meaningful code.
    // 1000: normal closure, 1001: going away, 1011: internal error.
    let close_code: u16 = match reason {
        CloseReason::ChromiumClosed => 1000,
        CloseReason::SessionTimeout | CloseReason::ServerShutdown => 1001,
        CloseReason::ChromiumError => 1011,
        CloseReason::ClientDisconnected | CloseReason::ClientError => 0,
    };
    if close_code != 0 {
        let frame = CloseFrame {
            code: close_code,
            reason: reason.as_str().into(),
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            client_tx.send(ClientMessage::Close(Some(frame))),
        )
        .await;
    }
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        upstream_tx.send(UpstreamMessage::Close(None)),
    )
    .await;

    reason
}
