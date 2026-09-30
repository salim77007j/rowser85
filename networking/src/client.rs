//! HTTP/1.1 + HTTP/2 transport (hyper + rustls + hickory DNS).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use hickory_resolver::config::{NameServerConfig, NameServerConfigGroup, ResolverConfig};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::TokioResolver;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_rustls::HttpsConnector;
use url::Url;

use crate::{FetchResponse, NetError, NetworkContext, FetchRequest};

/// The concrete HTTP client type.
pub type HttpClient =
    Client<HttpsConnector<HttpConnector<HickoryDnsResolver>>, http_body_util::Full<Bytes>>;

/// Wrapper making the hickory resolver usable by hyper-util.
#[derive(Clone, Debug)]
pub struct HickoryDnsResolver {
    resolver: TokioResolver,
}

impl tower_service::Service<Name> for HickoryDnsResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = hickory_resolver::ResolveError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let resolver = self.resolver.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(host).await?;
            let addrs: Vec<SocketAddr> =
                lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            Ok(addrs.into_iter())
        })
    }
}

/// DNS configuration.
#[derive(Debug, Clone)]
pub struct HickoryDnsConfig {
    /// Secure DNS mode.
    pub mode: DnsMode,
    /// Query timeout.
    pub timeout: Duration,
}

/// DNS transport selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DnsMode {
    /// OS resolver (`/etc/resolv.conf`).
    #[default]
    System,
    /// DNS-over-HTTPS (RFC 8484) to the given template-less URL.
    Doh(String),
    /// DNS-over-TLS to `host:port`.
    Dot(String),
    /// DNS-over-QUIC to `host:port`.
    Doq(String),
}

/// Builds the resolver for the given configuration.
pub async fn build_resolver(config: &HickoryDnsConfig) -> Result<TokioResolver, NetError> {
    let mut group = NameServerConfigGroup::new();
    match &config.mode {
        DnsMode::System => {
            // System config (reads /etc/resolv.conf).
            let builder = TokioResolver::builder_tokio()
                .map_err(|e| NetError::Dns(e.to_string()))?;
            return Ok(builder.build());
        }
        DnsMode::Doh(url) => {
            let url = url.trim_start_matches("https://");
            let (host, port) = split_host_port(url, 443);
            group.push(NameServerConfig::new(
                SocketAddr::new(parse_host(&host)?, port),
                Protocol::Https,
            ));
        }
        DnsMode::Dot(target) => {
            let (host, port) = split_host_port(target, 853);
            group.push(NameServerConfig::new(
                SocketAddr::new(parse_host(&host)?, port),
                Protocol::Tls,
            ));
        }
        DnsMode::Doq(target) => {
            let (host, port) = split_host_port(target, 853);
            group.push(NameServerConfig::new(
                SocketAddr::new(parse_host(&host)?, port),
                Protocol::Quic,
            ));
        }
    }
    let config_struct = ResolverConfig::from_parts(None, vec![], group);
    let mut builder = TokioResolver::builder_with_config(
        config_struct,
        TokioConnectionProvider::default(),
    );
    builder.options_mut().timeout = config.timeout;
    Ok(builder.build())
}

fn split_host_port(input: &str, default_port: u16) -> (String, u16) {
    match input.rsplit_once(':') {
        Some((host, port)) => (host.trim_matches(|c| c == '[' || c == ']').to_owned(), port.parse().unwrap_or(default_port)),
        None => (input.trim_matches(|c| c == '[' || c == ']').to_owned(), default_port),
    }
}

fn parse_host(host: &str) -> Result<std::net::IpAddr, NetError> {
    // DoH/DoT targets are usually IPs; resolve names via the system first.
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    use std::net::ToSocketAddrs;
    let resolved = (host, 443u16)
        .to_socket_addrs()
        .map_err(|e| NetError::Dns(e.to_string()))?
        .next()
        .ok_or_else(|| NetError::Dns("could not resolve DoH endpoint".into()))?;
    Ok(resolved.ip())
}

/// Builds the hyper client (h1 + h2, rustls, connection pooling).
pub fn build_http_client(resolver: &TokioResolver) -> HttpClient {
    let dns = HickoryDnsResolver { resolver: resolver.clone() };
    let mut http = HttpConnector::new_with_resolver(dns);
    http.set_nodelay(true);
    http.set_keepalive(Some(Duration::from_secs(30)));
    http.set_keepalive_interval(Some(Duration::from_secs(15)));
    http.set_happy_eyeballs_timeout(Some(Duration::from_millis(150)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4)
        .build(https)
}

/// Executes one HTTP/1.1 or HTTP/2 exchange (no redirect handling here).
pub async fn fetch_http(
    ctx: &NetworkContext,
    request: &FetchRequest,
    url: &Url,
) -> Result<FetchResponse, NetError> {
    let uri = url.as_str().to_owned();
    let mut builder = http::Request::builder()
        .method(request.method.as_str())
        .uri(uri)
        .header("host", url.host_str().unwrap_or_default());
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = http_body_util::Full::new(request.body.clone().unwrap_or_default());
    let req = builder
        .body(body)
        .map_err(|e| NetError::Http(e.to_string()))?;

    let fut = ctx.http.request(req);
    let response = match tokio::time::timeout(ctx.request_timeout, fut).await {
        Ok(result) => result.map_err(|e| NetError::Http(e.to_string()))?,
        Err(_) => return Err(NetError::Timeout(ctx.request_timeout)),
    };

    let status = response.status().as_u16();
    let version = response.version();
    let transport: &'static str = match version {
        http::Version::HTTP_2 => "h2",
        http::Version::HTTP_3 => "h3",
        _ => "h1",
    };
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(n, v)| (n.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let (parts, body) = response.into_parts();
    let _ = parts;
    let bytes = read_body(body, ctx.request_timeout).await?;
    Ok(FetchResponse {
        url: request.url.clone(),
        status,
        headers,
        body: bytes,
        transport,
    })
}

async fn read_body(body: Incoming, timeout: Duration) -> Result<Bytes, NetError> {
    let collected = match tokio::time::timeout(timeout, body.collect()).await {
        Ok(result) => result.map_err(|e| NetError::Body(e.to_string()))?,
        Err(_) => return Err(NetError::Timeout(timeout)),
    };
    Ok(collected.to_bytes())
}

/// Resolves the CNAME chain for a host (best-effort; empty on failure).
pub async fn cname_chain(resolver: &TokioResolver, host: &str) -> Vec<String> {
    use hickory_resolver::proto::rr::RecordType;
    let name = match host.parse::<hickory_resolver::proto::rr::Name>() {
        Ok(name) => name,
        Err(_) => return Vec::new(),
    };
    match resolver.lookup(name, RecordType::CNAME).await {
        Ok(lookup) => lookup
            .records()
            .iter()
            .filter_map(|record| {
                record
                    .data()
                    .as_cname()
                    .map(|cname| cname.0.to_string().trim_end_matches('.').to_ascii_lowercase())
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_host_port_works() {
        assert_eq!(split_host_port("1.1.1.1", 443), ("1.1.1.1".to_owned(), 443));
        assert_eq!(split_host_port("dns.example:853", 53), ("dns.example".to_owned(), 853));
        assert_eq!(split_host_port("[::1]:53", 53), ("::1".to_owned(), 53));
    }

    #[tokio::test]
    async fn system_resolver_builds() {
        let resolver = build_resolver(&HickoryDnsConfig {
            mode: DnsMode::System,
            timeout: Duration::from_secs(5),
        })
        .await
        .unwrap();
        let client = build_http_client(&resolver);
        let _ = client;
    }
}
