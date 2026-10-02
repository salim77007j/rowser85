//! WebSocket transport (tokio-tungstenite, rustls).

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::NetError;

/// An established WebSocket connection.
pub struct WebSocket {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

/// Connects to a `ws://` or `wss://` endpoint, presenting the session
/// identity (`User-Agent`) and the initiating page's `Origin` on the
/// handshake — both are commonly required by live/chat backends.
pub async fn connect(
    url: &str,
    origin: Option<&str>,
    user_agent: &str,
) -> Result<WebSocket, NetError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| NetError::InvalidUrl(e.to_string()))?;
    let headers = request.headers_mut();
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    if let Ok(value) = HeaderValue::from_str(user_agent) {
        headers.insert("user-agent", value);
    }
    if let Some(origin) = origin {
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert("origin", value);
        }
    }
    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| NetError::Connect(e.to_string()))?;
    Ok(WebSocket { stream })
}

impl WebSocket {
    /// Sends a text message.
    pub async fn send_text(&mut self, text: &str) -> Result<(), NetError> {
        self.stream
            .send(Message::Text(text.to_owned().into()))
            .await
            .map_err(|e| NetError::Body(e.to_string()))
    }

    /// Sends a binary message.
    pub async fn send_binary(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        self.stream
            .send(Message::Binary(data.into()))
            .await
            .map_err(|e| NetError::Body(e.to_string()))
    }

    /// Receives the next message (text payload, binary payload, close or
    /// continue-waiting).
    pub async fn recv(&mut self) -> Result<WsEvent, NetError> {
        let message = self
            .stream
            .next()
            .await
            .ok_or(NetError::Connect("websocket closed".into()))?
            .map_err(|e| NetError::Body(e.to_string()))?;
        match message {
            Message::Text(text) => Ok(WsEvent::Text(text.to_string())),
            Message::Binary(data) => Ok(WsEvent::Binary(data.to_vec())),
            Message::Close(frame) => Ok(WsEvent::Closed(
                frame.map(|f| f.reason.to_string()).unwrap_or_default(),
            )),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(WsEvent::Continue),
        }
    }

    /// Closes the connection politely.
    pub async fn close(&mut self) -> Result<(), NetError> {
        self.stream
            .close(None)
            .await
            .map_err(|e| NetError::Body(e.to_string()))
    }
}

/// A received WebSocket event.
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// Text message.
    Text(String),
    /// Binary message.
    Binary(Vec<u8>),
    /// The peer closed the connection (with the close reason).
    Closed(String),
    /// Ping/pong (handled internally).
    Continue,
}
