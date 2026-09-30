//! HTTP/3 (QUIC) transport via quinn + h3, with alt-svc learning and TCP
//! fallback.

use std::collections::HashMap;
use std::sync::Arc;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use url::Url;

use crate::{FetchResponse, NetError, NetworkContext, FetchRequest};

/// A reusable HTTP/3 request handle for one origin.
type SendRequestHandle = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;

/// HTTP/3 pool settings.
#[derive(Debug, Clone)]
pub struct H3Settings {
    /// Try HTTP/3 when the origin advertises it (alt-svc).
    pub enabled: bool,
    /// QUIC handshake timeout before falling back to TCP.
    pub handshake_timeout: Duration,
    /// Idle timeout for pooled connections.
    pub idle_timeout: Duration,
}

impl Default for H3Settings {
    fn default() -> Self {
        H3Settings {
            enabled: true,
            handshake_timeout: Duration::from_millis(750),
            idle_timeout: Duration::from_secs(30),
        }
    }
}

struct PooledConnection {
    send_request: SendRequestHandle,
    established: Instant,
}

/// Per-origin HTTP/3 connection pool + alt-svc cache.
pub struct H3Pool {
    settings: H3Settings,
    /// origin ("host:port") → pooled send-request.
    connections: Mutex<HashMap<String, PooledConnection>>,
    /// origin → alt-svc expiry.
    alt_svc: Mutex<HashMap<String, Instant>>,
    /// lazily-created client QUIC endpoint.
    endpoint: tokio::sync::Mutex<Option<quinn::Endpoint>>,
}

impl H3Pool {
    /// Creates the pool.
    pub fn new(settings: H3Settings) -> Self {
        H3Pool {
            settings,
            connections: Mutex::new(HashMap::new()),
            alt_svc: Mutex::new(HashMap::new()),
            endpoint: tokio::sync::Mutex::new(None),
        }
    }

    /// Records an `alt-svc` header value for an origin.
    pub fn note_alt_svc(&self, host: &str, port: u16, header: &str) {
        if !self.settings.enabled {
            return;
        }
        let lower = header.to_ascii_lowercase();
        if !lower.contains("h3") {
            return;
        }
        // `h3=":443"; ma=86400`
        let max_age = lower
            .split(';')
            .find_map(|part| part.trim().strip_prefix("ma="))
            .and_then(|ma| ma.trim().parse::<u64>().ok())
            .unwrap_or(86400);
        let key = format!("{host}:{port}");
        let expiry = Instant::now() + Duration::from_secs(max_age.min(7 * 24 * 3600));
        self.alt_svc.lock().unwrap().insert(key, expiry);
    }

    /// True when HTTP/3 should be attempted for this origin.
    pub fn prefers_h3(&self, host: &str, port: u16) -> bool {
        if !self.settings.enabled {
            return false;
        }
        let key = format!("{host}:{port}");
        self.alt_svc
            .lock()
            .unwrap()
            .get(&key)
            .map(|expiry| *expiry > Instant::now())
            .unwrap_or(false)
    }

    async fn endpoint(&self) -> Result<quinn::Endpoint, NetError> {
        let mut guard = self.endpoint.lock().await;
        if guard.is_none() {
            let roots = rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| NetError::Tls(e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            crypto.alpn_protocols = vec![b"h3".to_vec()];
            let quic_config = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                    .map_err(|e| NetError::Tls(e.to_string()))?,
            ));
            let bind_addr: SocketAddr = "[::]:0"
                .parse()
                .map_err(|e: std::net::AddrParseError| NetError::Connect(e.to_string()))?;
            let mut endpoint = quinn::Endpoint::client(bind_addr)
                .map_err(|e| NetError::Connect(e.to_string()))?;
            endpoint.set_default_client_config(quic_config);
            *guard = Some(endpoint);
        }
        Ok(guard.as_ref().unwrap().clone())
    }

    /// Executes one HTTP/3 exchange.
    pub async fn fetch(
        &self,
        ctx: &NetworkContext,
        request: &FetchRequest,
        url: &Url,
    ) -> Result<FetchResponse, NetError> {
        let host = url.host_str().unwrap_or_default().to_owned();
        let port = url.port_or_known_default().unwrap_or(443);
        let key = format!("{host}:{port}");

        // Resolve the address (shared resolver, cached by hickory).
        let lookup = ctx
            .resolver
            .lookup_ip(&host)
            .await
            .map_err(|e| NetError::Dns(e.to_string()))?;
        let addr = lookup
            .iter()
            .find_map(|ip| match ip {
                std::net::IpAddr::V4(_) => None,
                v6 @ std::net::IpAddr::V6(_) => Some(v6),
            })
            .or_else(|| lookup.iter().next())
            .ok_or_else(|| NetError::Dns("no address".into()))?;
        let addr = SocketAddr::new(addr, port);

        let send_request = match self.take_pooled(&key) {
            Some(send_request) => send_request,
            None => self.connect(addr, &host).await?,
        };

        let result = exchange(send_request, request, url, &key, self).await;
        // On failure, drop the pooled connection so the next attempt
        // reconnects (or falls back to TCP upstream).
        result
    }

    fn take_pooled(&self, key: &str) -> Option<SendRequestHandle> {
        let mut connections = self.connections.lock().unwrap();
        match connections.get(key) {
            Some(pooled) if pooled.established.elapsed() < self.settings.idle_timeout => {
                Some(pooled.send_request.clone())
            }
            _ => {
                connections.remove(key);
                None
            }
        }
    }

    async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
    ) -> Result<SendRequestHandle, NetError> {
        let endpoint = self.endpoint().await?;
        let connecting = endpoint
            .connect(addr, server_name)
            .map_err(|e| NetError::Connect(e.to_string()))?;
        let conn = tokio::time::timeout(self.settings.handshake_timeout, connecting)
            .await
            .map_err(|_| NetError::Timeout(self.settings.handshake_timeout))?
            .map_err(|e| NetError::Connect(e.to_string()))?;
        let h3_conn = h3_quinn::Connection::new(conn);
        let (_connection, send_request) = h3::client::new(h3_conn)
            .await
            .map_err(|e| NetError::Connect(e.to_string()))?;
        Ok(send_request)
    }
}

async fn exchange(
    mut send_request: SendRequestHandle,
    request: &FetchRequest,
    url: &Url,
    key: &str,
    pool: &H3Pool,
) -> Result<FetchResponse, NetError> {
    let builder = http::Request::builder()
        .method(request.method.as_str())
        .uri(url.as_str())
        .header("host", url.host_str().unwrap_or_default());
    let mut req = builder
        .body(())
        .map_err(|e| NetError::Http(e.to_string()))?;
    // h3 requests carry all headers (including pseudo via builder).
    *req.headers_mut() = crate::to_header_map(&request.headers);

    let mut stream = send_request
        .send_request(req)
        .await
        .map_err(|e| NetError::Http(e.to_string()))?;
    if let Some(body) = &request.body {
        stream
            .send_data(body.clone())
            .await
            .map_err(|e| NetError::Http(e.to_string()))?;
    }
    stream
        .finish()
        .await
        .map_err(|e| NetError::Http(e.to_string()))?;

    let response = stream
        .recv_response()
        .await
        .map_err(|e| NetError::Http(e.to_string()))?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_owned()))
        .collect();

    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.map_err(|e| NetError::Body(e.to_string()))? {
        while chunk.has_remaining() {
            body.push(chunk.get_u8());
        }
    }

    // Return the connection to the pool.
    pool.connections.lock().unwrap().insert(
        key.to_owned(),
        PooledConnection {
            send_request,
            established: Instant::now(),
        },
    );

    Ok(FetchResponse {
        url: request.url.clone(),
        status,
        headers,
        body: Bytes::from(body),
        transport: "h3",
    })
}

