//! Rrowser networking: the fetch pipeline.
//!
//! * HTTP/1.1 + HTTP/2 over **hyper** with **rustls** (TLS 1.3 default)
//! * HTTP/3 over **quinn** (QUIC) with graceful TCP fallback
//! * DNS via **hickory** with DNS-over-HTTPS / DNS-over-TLS / DoQ support
//! * WebSocket via **tokio-tungstenite**
//! * Brave-adblock request filtering, CNAME-cloaking checks, HTTPS upgrade,
//!   CHIPS-aware cookie handling and redirect re-verification
//!
//! Every request passes the same privacy pipeline (see [`fetch`]) before a
//! socket is opened: blocklist → HTTPS upgrade → DNS (+ CNAME guard) →
//! transport. Redirects re-run the full pipeline so trackers cannot smuggle
//! requests through 3xx hops.

pub mod client;
pub mod h3;
pub mod scheduler;
pub mod ws;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use url::Url;

pub use client::HickoryDnsConfig;
pub use h3::H3Settings;
pub use scheduler::{ResourcePriority, ResourceScheduler};

use rowser_privacy::PrivacySettings;
use rowser_privacy::fingerprint::{ACCEPT_LANGUAGE, USER_AGENT};
use rowser_storage::cookies::{CookieJar, ThirdPartyPolicy};
use rowser_storage::Storage;

/// The identity every outgoing request presents (user agent + languages).
///
/// Sites increasingly hard-fail requests without a browser User-Agent
/// (Wikipedia: 403 robot policy; Google/YouTube: h2 RST_STREAM protocol
/// error), so this is applied to EVERY request on EVERY transport (h1/h2/h3,
/// websockets, downloads) from ONE session-scoped value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// User-Agent string (must match `navigator.userAgent`).
    pub user_agent: String,
    /// Accept-Language header value.
    pub accept_language: String,
}

impl Default for ClientIdentity {
    fn default() -> Self {
        ClientIdentity {
            user_agent: USER_AGENT.to_owned(),
            accept_language: ACCEPT_LANGUAGE.to_owned(),
        }
    }
}

/// Network-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The request was blocked by the privacy engine.
    #[error("blocked: {0}")]
    Blocked(String),
    /// The URL could not be parsed.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// DNS resolution failed.
    #[error("dns: {0}")]
    Dns(String),
    /// TLS failed.
    #[error("tls: {0}")]
    Tls(String),
    /// The connection failed.
    #[error("connect: {0}")]
    Connect(String),
    /// The HTTP exchange failed.
    #[error("http: {0}")]
    Http(String),
    /// The response body could not be read.
    #[error("body: {0}")]
    Body(String),
    /// Too many redirects.
    #[error("too many redirects")]
    TooManyRedirects,
    /// Request timed out.
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    /// Unsupported scheme.
    #[error("unsupported scheme: {0}")]
    UnsupportedScheme(String),
    /// Storage (cookies/cache) failure.
    #[error("storage: {0}")]
    Storage(String),
}

impl From<rowser_storage::StorageError> for NetError {
    fn from(err: rowser_storage::StorageError) -> Self {
        NetError::Storage(err.to_string())
    }
}

/// A fetch request.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// Target URL.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Request body (None for GET/HEAD).
    pub body: Option<Bytes>,
    /// Resource classification for filter matching.
    pub resource_type: ResourceKind,
    /// Initiating document URL (empty for main frames).
    pub source_url: String,
    /// Top-level site (registrable domain) for cookie partitioning.
    pub top_site: Option<String>,
}

impl Default for FetchRequest {
    fn default() -> Self {
        FetchRequest {
            url: String::new(),
            method: "GET".to_owned(),
            headers: Vec::new(),
            body: None,
            resource_type: ResourceKind::Document,
            source_url: String::new(),
            top_site: None,
        }
    }
}

/// Resource classification (maps to EasyList request types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResourceKind {
    /// Main frame.
    #[default]
    Document,
    /// Sub-frame.
    SubDocument,
    /// Stylesheet.
    Stylesheet,
    /// Script.
    Script,
    /// Image.
    Image,
    /// Font.
    Font,
    /// XHR/fetch.
    Xhr,
    /// WebSocket.
    WebSocket,
    /// Media.
    Media,
    /// Anything else.
    Other,
}

/// A fetch response.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// Final URL (after redirects).
    pub url: String,
    /// Status code.
    pub status: u16,
    /// Response headers (name, value) in order.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Bytes,
    /// Transport used ("h1", "h2" or "h3").
    pub transport: &'static str,
}

impl FetchResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Value of `Content-Type`.
    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("text/plain")
    }

    /// True for 2xx statuses.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Shared, thread-safe network state.
pub struct NetworkContext {
    /// HTTP/1.1 + HTTP/2 client.
    pub http: client::HttpClient,
    /// Session request identity (UA + languages) applied to all requests.
    pub client: ClientIdentity,
    /// DNS resolver (DoH/DoT capable).
    pub resolver: hickory_resolver::TokioResolver,
    /// HTTP/3 pool.
    pub h3: h3::H3Pool,
    /// Privacy settings (HTTPS upgrade, third-party cookie policy).
    pub settings: std::sync::RwLock<PrivacySettings>,
    /// Persistent storage (cookies + cache).
    pub storage: Arc<Storage>,
    /// HTTP cache budget.
    pub cache_budget: u64,
    /// Per-request timeout.
    pub request_timeout: Duration,
}

/// Builds the default network context for a profile.
pub async fn build_context(
    storage: Arc<Storage>,
    settings: PrivacySettings,
    dns: HickoryDnsConfig,
    h3_settings: H3Settings,
    client: ClientIdentity,
) -> Result<NetworkContext, NetError> {
    let resolver = client::build_resolver(&dns).await?;
    let http = client::build_http_client(&resolver);
    let h3_pool = h3::H3Pool::new(h3_settings);
    Ok(NetworkContext {
        http,
        client,
        resolver,
        h3: h3_pool,
        settings: std::sync::RwLock::new(settings),
        storage,
        cache_budget: 32 * 1024 * 1024,
        request_timeout: Duration::from_secs(30),
    })
}

/// Runs the full fetch pipeline.
pub async fn fetch(ctx: &NetworkContext, request: FetchRequest) -> Result<FetchResponse, NetError> {
    let mut current = request.clone();
    let mut redirects = 0;
    loop {
        // 1. Parse + scheme dispatch.
        let mut url = Url::parse(&current.url).map_err(|e| NetError::InvalidUrl(e.to_string()))?;
        match url.scheme() {
            "data" => return fetch_data_url(&current),
            "about" => {
                return Ok(FetchResponse {
                    url: current.url,
                    status: 200,
                    headers: vec![("content-type".into(), "text/html".into())],
                    body: Bytes::new(),
                    transport: "internal",
                })
            }
            "http" => {
                // Localhost is treated as a secure context (matching
                // browser norms), so it is exempt from upgrade.
                let is_local = url
                    .host_str()
                    .map(|h| {
                        h.eq_ignore_ascii_case("localhost")
                            || h.starts_with("127.")
                            || h.starts_with("[::1]")
                    })
                    .unwrap_or(false);
                if !is_local && ctx.settings.read().map(|s| s.https_upgrade).unwrap_or(true) {
                    url.set_scheme("https").ok();
                    current.url = url.to_string();
                }
            }
            "https" => {}
            "ws" | "wss" => {
                return Err(NetError::UnsupportedScheme(
                    "use ws::connect for websockets".into(),
                ))
            }
            other => return Err(NetError::UnsupportedScheme(other.to_owned())),
        }

        // 2. Cookies (CHIPS-aware request cookies).
        // (Request blocking + CNAME cloaking run on the engine loop before
        // the fetch is spawned — see the engine crate.)
        let jar: CookieJar = ctx.storage.cookies();
        let policy = if ctx
            .settings
            .read()
            .map(|s| s.block_third_party_cookies)
            .unwrap_or(true)
        {
            ThirdPartyPolicy::Partitioned
        } else {
            ThirdPartyPolicy::AllowAll
        };
        if let Some(cookie_header) = jar.cookie_header(&url, current.top_site.as_deref(), policy)? {
            current
                .headers
                .retain(|(n, _)| !n.eq_ignore_ascii_case("cookie"));
            current.headers.push(("cookie".into(), cookie_header));
        }

        // 3. Standard browser headers (identity + fetch metadata). Applied
        // on every hop so Sec-Fetch-Site is recomputed per redirect target.
        apply_default_headers(&mut current, &ctx.client, &url);

        // 4. Dispatch.
        let response = dispatch(ctx, &current, &url).await?;

        // 5. Set-Cookie handling.
        let mut set_cookies: Vec<String> = Vec::new();
        let mut kept_headers: Vec<(String, String)> = Vec::new();
        for (name, value) in response.headers.clone() {
            if name.eq_ignore_ascii_case("set-cookie") {
                set_cookies.push(value);
            } else {
                kept_headers.push((name, value));
            }
        }
        for raw in set_cookies {
            jar.set_cookie(&raw, &url, current.top_site.as_deref(), policy)?;
        }

        // 6. Redirects.
        if (301..400).contains(&response.status) && redirects < 20 {
            redirects += 1;
            if let Some(location) = response.header("location") {
                let joined = url
                    .join(location.trim())
                    .map(|u| u.to_string())
                    .map_err(|e| NetError::InvalidUrl(e.to_string()))?;
                current.url = joined;
                current.source_url = request.source_url.clone();
                // Preserve partition context across redirects.
                continue;
            }
        }
        if redirects >= 20 {
            return Err(NetError::TooManyRedirects);
        }

        // 7. HTTP cache store for cacheable subresources.
        if response.is_success()
            && matches!(
                current.resource_type,
                ResourceKind::Stylesheet
                    | ResourceKind::Script
                    | ResourceKind::Font
                    | ResourceKind::Image
            )
        {
            let cacheable = response
                .header("cache-control")
                .map(|c| !c.contains("no-store"))
                .unwrap_or(true);
            if cacheable {
                let cache = ctx.storage.http_cache(ctx.cache_budget);
                let _ = cache.put(
                    &response.url,
                    response.status,
                    response.headers.clone(),
                    response.body.to_vec(),
                );
            }
        }

        let final_response = FetchResponse {
            url: response.url,
            status: response.status,
            headers: kept_headers,
            body: response.body,
            transport: response.transport,
        };
        // Alt-svc learning (HTTP/3 advertisement over HTTP/2).
        if let Some(alt_svc) = final_response.header("alt-svc") {
            let host = url.host_str().unwrap_or_default().to_owned();
            let port = url.port_or_known_default().unwrap_or(443);
            ctx.h3.note_alt_svc(&host, port, alt_svc);
        }
        return Ok(final_response);
    }
}

async fn dispatch(
    ctx: &NetworkContext,
    request: &FetchRequest,
    url: &Url,
) -> Result<FetchResponse, NetError> {
    let host = url.host_str().unwrap_or_default().to_owned();
    let port = url.port_or_known_default().unwrap_or(443);

    // Prefer HTTP/3 for known-h3 origins (learned via alt-svc), fall back to
    // TCP (hyper) on any QUIC failure.
    if url.scheme() == "https" && ctx.h3.prefers_h3(&host, port) {
        match ctx.h3.fetch(ctx, request, url).await {
            Ok(response) => return Ok(response),
            Err(err) => {
                tracing::debug!(target: "rowser::net", "h3 fallback to tcp: {err}");
            }
        }
    }
    client::fetch_http(ctx, request, url).await
}

/// `data:` URL scheme support.
fn fetch_data_url(request: &FetchRequest) -> Result<FetchResponse, NetError> {
    let rest = request
        .url
        .strip_prefix("data:")
        .ok_or_else(|| NetError::InvalidUrl("not a data URL".into()))?;
    let (meta, payload) = match rest.split_once(',') {
        Some((m, p)) => (m, p),
        None => return Err(NetError::InvalidUrl("malformed data URL".into())),
    };
    let is_base64 = meta.to_ascii_lowercase().ends_with(";base64");
    let mime = meta.trim_end_matches(";base64");
    let mime = if mime.is_empty() { "text/plain" } else { mime };
    let body: Vec<u8> = if is_base64 {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| NetError::Body(e.to_string()))?
    } else {
        percent_encoding::percent_decode_str(payload)
            .decode_utf8()
            .map_err(|e| NetError::Body(e.to_string()))?
            .as_bytes()
            .to_vec()
    };
    Ok(FetchResponse {
        url: request.url.clone(),
        status: 200,
        headers: vec![("content-type".to_owned(), mime.to_owned())],
        body: Bytes::from(body),
        transport: "internal",
    })
}

/// Builds a standard header list for a request (user agent etc.).
fn apply_default_headers(
    request: &mut FetchRequest,
    client: &ClientIdentity,
    url: &Url,
) {
    fn has(headers: &[(String, String)], name: &str) -> bool {
        headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }
    fn set(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
        headers.push((name.to_owned(), value.to_owned()));
    }

    // Identity headers — only if the caller did not provide them.
    if !has(&request.headers, "user-agent") {
        set(&mut request.headers, "user-agent", &client.user_agent);
    }
    if !has(&request.headers, "accept-language") {
        set(&mut request.headers, "accept-language", &client.accept_language);
    }
    if !has(&request.headers, "accept") {
        let accept = match request.resource_type {
            ResourceKind::Document | ResourceKind::SubDocument => {
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
            }
            ResourceKind::Stylesheet => "text/css,*/*;q=0.1",
            ResourceKind::Image => {
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8"
            }
            _ => "*/*",
        };
        set(&mut request.headers, "accept", accept);
    }
    if matches!(request.resource_type, ResourceKind::Document)
        && !has(&request.headers, "upgrade-insecure-requests")
    {
        set(&mut request.headers, "upgrade-insecure-requests", "1");
    }

    // Sec-Fetch-* metadata. These are browser-controlled (the fetch spec
    // forbids JS from setting them), so we always (re)compute them — which
    // also keeps them correct across redirect hops.
    let (mode, dest) = match request.resource_type {
        ResourceKind::Document => ("navigate", "document"),
        ResourceKind::SubDocument => ("navigate", "iframe"),
        ResourceKind::Stylesheet => ("no-cors", "style"),
        ResourceKind::Image => ("no-cors", "image"),
        ResourceKind::Script => ("no-cors", "script"),
        ResourceKind::Font => ("no-cors", "font"),
        ResourceKind::Media => ("no-cors", "audio"),
        ResourceKind::Xhr => ("cors", ""),
        ResourceKind::WebSocket => ("websocket", ""),
        ResourceKind::Other => ("no-cors", ""),
    };
    let site = fetch_site(url, &request.source_url);
    request
        .headers
        .retain(|(n, _)| !(n.eq_ignore_ascii_case("sec-fetch-mode")
            || n.eq_ignore_ascii_case("sec-fetch-dest")
            || n.eq_ignore_ascii_case("sec-fetch-site")));
    set(&mut request.headers, "sec-fetch-mode", mode);
    if !dest.is_empty() {
        set(&mut request.headers, "sec-fetch-dest", dest);
    }
    set(&mut request.headers, "sec-fetch-site", site);
}

/// Computes the Sec-Fetch-Site value for a request to `url` initiated from
/// `source` (empty = browser-initiated).
fn fetch_site(url: &Url, source: &str) -> &'static str {
    let Some(source) = Url::parse(source).ok() else {
        return "none";
    };
    let (target, initiator) = match (url.host_str(), source.host_str()) {
        (Some(t), Some(i)) => (t, i),
        _ => return "none",
    };
    if target.eq_ignore_ascii_case(initiator) {
        "same-origin"
    } else {
        let t_reg = rowser_privacy::psl_registrable(target);
        let i_reg = rowser_privacy::psl_registrable(initiator);
        if !t_reg.is_empty() && t_reg.eq_ignore_ascii_case(&i_reg) {
            "same-site"
        } else {
            "cross-site"
        }
    }
}

/// Converts our header pairs to `http` crate types.
pub(crate) fn to_header_map(headers: &[(String, String)]) -> http::HeaderMap {
    let mut map = http::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        map.insert(name, value);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_base64() {
        let request = FetchRequest {
            url: "data:text/html;base64,PGI+aGk8L2I+".to_owned(),
            ..FetchRequest::default()
        };
        let response = fetch_data_url(&request).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("text/html"));
        assert_eq!(response.body.as_ref(), b"<b>hi</b>");
    }

    #[test]
    fn data_url_percent_encoded() {
        let request = FetchRequest {
            url: "data:text/plain,Hello%20World".to_owned(),
            ..FetchRequest::default()
        };
        let response = fetch_data_url(&request).unwrap();
        assert_eq!(response.body.as_ref(), b"Hello World");
    }
}
