use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use maya_core::security::{
    derive_filename_from_url, parse_content_disposition_filename, resolve_and_validate_addrs,
    validate_url,
};
use maya_core::{
    ByteStream, DownloadRequest, MayaError, ProbeResult, ProtocolClient, ProtocolKind, Result,
    SecurityPolicy,
};
use reqwest::header::{
    ACCEPT_RANGES, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    COOKIE, ETAG, LAST_MODIFIED, LOCATION, PROXY_AUTHORIZATION, RANGE, USER_AGENT,
};
use reqwest::redirect::Policy;
use url::{Host, Url};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_USER_AGENT: &str = concat!("maya/", env!("CARGO_PKG_VERSION"));

/// HTTP/HTTPS implementation of [`maya_core::ProtocolClient`] with per-hop SSRF validation.
#[derive(Debug, Clone)]
pub struct HttpProtocolClient {
    connect_timeout: Duration,
    user_agent: String,
}

impl Default for HttpProtocolClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpProtocolClient {
    /// Construct an HTTP/HTTPS client with default connect timeout and User-Agent.
    pub fn new() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }
    }

    /// Override the TCP/TLS connect timeout per hop.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Override the default `User-Agent` header sent when none is configured on the request.
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    async fn build_pinned_client(
        &self,
        url: &Url,
        policy: &SecurityPolicy,
    ) -> Result<reqwest::Client> {
        let host = match url.host() {
            Some(Host::Domain(domain)) => domain.to_string(),
            Some(Host::Ipv4(ip)) => ip.to_string(),
            Some(Host::Ipv6(ip)) => ip.to_string(),
            None => {
                return Err(MayaError::InvalidConfig(format!(
                    "URL `{url}` is missing a host"
                )));
            }
        };

        let port = url
            .port_or_known_default()
            .ok_or_else(|| MayaError::InvalidConfig(format!("URL `{url}` is missing a port")))?;

        let addrs = resolve_and_validate_addrs(&host, port, policy).await?;
        if addrs.is_empty() {
            return Err(MayaError::Network(format!(
                "DNS resolution returned no addresses for `{host}`"
            )));
        }

        let mut builder = reqwest::Client::builder()
            .redirect(Policy::none())
            .use_rustls_tls()
            .connect_timeout(self.connect_timeout);

        if let Some(Host::Domain(domain)) = url.host() {
            builder = builder.resolve_to_addrs(domain, &addrs);
        }

        builder
            .build()
            .map_err(|err| MayaError::Network(format!("failed to build HTTP client: {err}")))
    }

    async fn send_with_redirects(
        &self,
        method: Method,
        request: &DownloadRequest,
        range_header: Option<&str>,
    ) -> Result<(reqwest::Response, Url)> {
        let initial_url = validate_url(&request.source, &request.security)?;
        let mut current_url = initial_url.clone();

        for hop in 0..=request.security.max_redirects {
            let client = self
                .build_pinned_client(&current_url, &request.security)
                .await?;

            let same_origin = current_url.origin() == initial_url.origin();
            let mut req_builder = client.request(method.clone(), current_url.clone());

            let mut has_user_agent = false;
            for (name, value) in &request.headers {
                let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
                    MayaError::InvalidConfig(format!("invalid HTTP header name `{name}`: {err}"))
                })?;

                if range_header.is_some() && header_name == RANGE {
                    continue;
                }
                if !same_origin
                    && (header_name == AUTHORIZATION
                        || header_name == COOKIE
                        || header_name == PROXY_AUTHORIZATION)
                {
                    continue;
                }
                if header_name == USER_AGENT {
                    has_user_agent = true;
                }

                let header_value = HeaderValue::from_str(value).map_err(|err| {
                    MayaError::InvalidConfig(format!(
                        "invalid HTTP header value for `{name}`: {err}"
                    ))
                })?;
                req_builder = req_builder.header(header_name, header_value);
            }

            if !has_user_agent {
                req_builder = req_builder.header(USER_AGENT, &self.user_agent);
            }
            if let Some(range) = range_header {
                req_builder = req_builder.header(RANGE, range);
            }

            let response = req_builder
                .send()
                .await
                .map_err(|err| MayaError::Network(err.to_string()))?;

            if is_redirect_status(response.status()) {
                if hop == request.security.max_redirects {
                    return Err(MayaError::Protocol(format!(
                        "exceeded maximum redirects ({})",
                        request.security.max_redirects
                    )));
                }

                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|val| val.to_str().ok())
                    .ok_or_else(|| {
                        MayaError::Protocol("redirect response missing Location header".to_string())
                    })?;

                let next_url = current_url.join(location)?;
                current_url = validate_url(next_url.as_str(), &request.security)?;
                continue;
            }

            return Ok((response, current_url));
        }

        Err(MayaError::Protocol(format!(
            "exceeded maximum redirects ({})",
            request.security.max_redirects
        )))
    }
}

#[async_trait]
impl ProtocolClient for HttpProtocolClient {
    fn supports(&self, source: &str) -> bool {
        let trimmed = source.trim();
        trimmed
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || trimmed
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    }

    async fn probe(&self, request: &DownloadRequest) -> Result<ProbeResult> {
        let (head_response, mut resolved_url) = self
            .send_with_redirects(Method::HEAD, request, None)
            .await?;
        let head_status = head_response.status();
        let head_headers = head_response.headers().clone();
        drop(head_response);

        let head_rejected = matches!(head_status.as_u16(), 400 | 403 | 405 | 501);
        if !head_rejected && !head_status.is_success() {
            return Err(MayaError::HttpStatus {
                status: head_status.as_u16(),
                message: status_message(head_status),
            });
        }

        let head_content_length = parse_content_length(&head_headers);
        let head_accept_ranges = header_str(&head_headers, ACCEPT_RANGES)
            .is_some_and(|val| val.trim().eq_ignore_ascii_case("bytes"));

        let (total_size, supports_ranges, final_headers) = if !head_rejected
            && head_content_length.is_some()
        {
            (head_content_length, head_accept_ranges, head_headers)
        } else {
            let (range_response, range_url) = self
                .send_with_redirects(Method::GET, request, Some("bytes=0-0"))
                .await?;
            resolved_url = range_url;
            let range_status = range_response.status();
            let range_headers = range_response.headers().clone();
            drop(range_response);

            match range_status {
                StatusCode::PARTIAL_CONTENT => {
                    let total = header_str(&range_headers, CONTENT_RANGE)
                        .and_then(parse_content_range_total);
                    let merged =
                        merge_headers(&head_headers, &range_headers, head_status.is_success());
                    (total, true, merged)
                }
                StatusCode::OK => {
                    let total = parse_content_length(&range_headers).or(head_content_length);
                    let merged =
                        merge_headers(&head_headers, &range_headers, head_status.is_success());
                    (total, false, merged)
                }
                StatusCode::RANGE_NOT_SATISFIABLE => {
                    let total = header_str(&range_headers, CONTENT_RANGE)
                        .and_then(parse_content_range_total);
                    if total == Some(0) {
                        let merged =
                            merge_headers(&head_headers, &range_headers, head_status.is_success());
                        (Some(0), true, merged)
                    } else {
                        return Err(MayaError::HttpStatus {
                            status: range_status.as_u16(),
                            message: status_message(range_status),
                        });
                    }
                }
                _ => {
                    return Err(MayaError::HttpStatus {
                        status: range_status.as_u16(),
                        message: status_message(range_status),
                    });
                }
            }
        };

        if let (Some(max_bytes), Some(total)) = (request.security.max_download_bytes, total_size) {
            if total > max_bytes {
                return Err(MayaError::SecurityViolation(format!(
                    "payload size {total} bytes exceeds max_download_bytes ({max_bytes})"
                )));
            }
        }

        let suggested_filename = header_str(&final_headers, CONTENT_DISPOSITION)
            .and_then(parse_content_disposition_filename)
            .or_else(|| Some(derive_filename_from_url(&resolved_url)));

        let protocol = if resolved_url.scheme().eq_ignore_ascii_case("https") {
            ProtocolKind::Https
        } else {
            ProtocolKind::Http
        };

        Ok(ProbeResult {
            protocol,
            resolved_url: resolved_url.to_string(),
            total_size,
            supports_ranges,
            suggested_filename,
            etag: header_str(&final_headers, ETAG).map(str::to_string),
            last_modified: header_str(&final_headers, LAST_MODIFIED).map(str::to_string),
            content_type: header_str(&final_headers, CONTENT_TYPE).map(str::to_string),
        })
    }

    async fn fetch_range(
        &self,
        request: &DownloadRequest,
        start: u64,
        end: Option<u64>,
    ) -> Result<ByteStream> {
        if let Some(end_val) = end {
            if end_val < start {
                return Err(MayaError::InvalidConfig(format!(
                    "invalid byte range: start ({start}) > end ({end_val})"
                )));
            }
            if end_val == start {
                return Ok(Box::pin(futures::stream::empty()));
            }
        }

        let range_header = if start > 0 || end.is_some() {
            Some(match end {
                Some(end_val) => format!("bytes={start}-{}", end_val - 1),
                None => format!("bytes={start}-"),
            })
        } else {
            None
        };

        let (response, _) = self
            .send_with_redirects(Method::GET, request, range_header.as_deref())
            .await?;
        let status = response.status();

        if start > 0 && status == StatusCode::OK {
            return Err(MayaError::Protocol("server ignored byte range".to_string()));
        }

        if !status.is_success() {
            return Err(MayaError::HttpStatus {
                status: status.as_u16(),
                message: status_message(status),
            });
        }

        if let Some(max_bytes) = request.security.max_download_bytes {
            if let Some(total) =
                header_str(response.headers(), CONTENT_RANGE).and_then(parse_content_range_total)
            {
                if total > max_bytes {
                    return Err(MayaError::SecurityViolation(format!(
                        "payload size {total} bytes exceeds max_download_bytes ({max_bytes})"
                    )));
                }
            }
            if let Some(len) = parse_content_length(response.headers()) {
                if len > max_bytes || start.saturating_add(len) > max_bytes {
                    return Err(MayaError::SecurityViolation(format!(
                        "payload size exceeds max_download_bytes ({max_bytes})"
                    )));
                }
            }
        }

        // If `start == 0` and `end` was requested, cap output if a server replied `200 OK`
        // with more bytes than requested.
        let max_slice_bytes = if status == StatusCode::OK {
            end.map(|end_val| end_val.saturating_sub(start))
        } else {
            None
        };

        let inner = response
            .bytes_stream()
            .map(|res| res.map_err(|err| MayaError::Network(err.to_string())));

        Ok(Box::pin(BoundedByteStream {
            inner: Box::pin(inner),
            received: 0,
            max_total_bytes: request.security.max_download_bytes,
            max_slice_bytes,
            done: false,
        }))
    }
}

struct BoundedByteStream {
    inner: ByteStream,
    received: u64,
    max_total_bytes: Option<u64>,
    max_slice_bytes: Option<u64>,
    done: bool,
}

impl Stream for BoundedByteStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(mut chunk))) => {
                if let Some(slice_cap) = self.max_slice_bytes {
                    let remaining = slice_cap.saturating_sub(self.received);
                    if remaining == 0 {
                        self.done = true;
                        return Poll::Ready(None);
                    }
                    if (chunk.len() as u64) > remaining {
                        chunk = chunk.slice(..remaining as usize);
                        self.done = true;
                    }
                }

                let next_received = self.received.saturating_add(chunk.len() as u64);
                if let Some(max_bytes) = self.max_total_bytes {
                    if next_received > max_bytes {
                        self.done = true;
                        return Poll::Ready(Some(Err(MayaError::SecurityViolation(format!(
                            "streamed payload exceeded max_download_bytes ({max_bytes})"
                        )))));
                    }
                }

                self.received = next_received;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.done = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn is_redirect_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn status_message(status: StatusCode) -> String {
    status
        .canonical_reason()
        .unwrap_or("HTTP error")
        .to_string()
}

fn header_str(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, CONTENT_LENGTH)?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Parses `Content-Range: bytes 0-0/<total>` or `bytes */<total>` into `Some(total)`.
fn parse_content_range_total(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let rest = trimmed
        .get(..6)
        .filter(|prefix| prefix.eq_ignore_ascii_case("bytes "))
        .map(|_| &trimmed[6..])?;
    let (_, total_part) = rest.split_once('/')?;
    let total_str = total_part.trim();
    if total_str == "*" {
        return None;
    }
    total_str.parse::<u64>().ok()
}

fn merge_headers(head: &HeaderMap, fallback: &HeaderMap, include_head: bool) -> HeaderMap {
    let mut merged = HeaderMap::new();
    if include_head {
        for (k, v) in head {
            merged.insert(k, v.clone());
        }
    }
    for (k, v) in fallback {
        merged.insert(k, v.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_server<F, Fut>(handler: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let handler = Arc::new(handler);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let resp = handler(req).await;
                    let _ = socket.write_all(&resp).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://127.0.0.1:{}", addr.port()), hits)
    }

    #[test]
    fn supports_http_and_https_case_insensitive() {
        let client = HttpProtocolClient::new();
        assert!(client.supports("http://example.com/file.bin"));
        assert!(client.supports("HTTPS://EXAMPLE.COM/file.bin"));
        assert!(!client.supports("magnet:?xt=urn:btih:abc"));
        assert!(!client.supports("file:///tmp/test"));
        assert!(!client.supports("/tmp/test.torrent"));
    }

    #[test]
    fn parses_content_range_totals() {
        assert_eq!(parse_content_range_total("bytes 0-0/1024"), Some(1024));
        assert_eq!(parse_content_range_total("BYTES 0-0/0"), Some(0));
        assert_eq!(parse_content_range_total("bytes */42"), Some(42));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("invalid"), None);
    }

    #[tokio::test]
    async fn blocks_private_ip_before_connecting() {
        let (base_url, hits) = spawn_mock_server(|_| async {
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntest".to_vec()
        })
        .await;

        let client = HttpProtocolClient::new();
        // Default SecurityPolicy has allow_private_ips = false
        let req = DownloadRequest::new(format!("{base_url}/file.bin"), PathBuf::from("."));
        let err = client.probe(&req).await.unwrap_err();
        assert!(
            matches!(err, MayaError::SecurityViolation(_)),
            "expected SecurityViolation, got {err:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn blocks_ssrf_redirect_to_forbidden_scheme_or_blocked_host() {
        let (base_url, _hits) = spawn_mock_server(|req| async move {
            if req.contains("GET /to-file") || req.contains("HEAD /to-file") {
                b"HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            } else if req.contains("/to-blocked") {
                b"HTTP/1.1 302 Found\r\nLocation: http://metadata.internal/latest\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            } else {
                b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();

        // 1. Redirect to file:// blocked by validate_url
        let mut req1 = DownloadRequest::new(format!("{base_url}/to-file"), PathBuf::from("."));
        req1.security.allow_private_ips = true;
        let err1 = client.probe(&req1).await.unwrap_err();
        assert!(
            matches!(
                err1,
                MayaError::SecurityViolation(_) | MayaError::UnsupportedScheme(_)
            ),
            "expected security/scheme error for file:// redirect, got {err1:?}"
        );

        // 2. Redirect to blocked host rejected
        let mut req2 = DownloadRequest::new(format!("{base_url}/to-blocked"), PathBuf::from("."));
        req2.security.allow_private_ips = true;
        req2.security.blocked_hosts = vec!["metadata.internal".to_string()];
        let err2 = client.probe(&req2).await.unwrap_err();
        assert!(
            matches!(err2, MayaError::SecurityViolation(_)),
            "expected SecurityViolation for blocked host redirect, got {err2:?}"
        );

        // 3. Redirect to host outside allowlist rejected (simulates SSRF redirect away from allowed host)
        let mut req3 = DownloadRequest::new(format!("{base_url}/to-imds"), PathBuf::from("."));
        req3.security.allow_private_ips = true;
        req3.security.allowed_hosts = vec!["127.0.0.1".to_string()];
        let err3 = client.probe(&req3).await.unwrap_err();
        assert!(
            matches!(err3, MayaError::SecurityViolation(_)),
            "expected SecurityViolation for IMDS redirect, got {err3:?}"
        );
    }

    #[tokio::test]
    async fn head_fallback_to_range_zero_probe() {
        let (base_url, _hits) = spawn_mock_server(|req| async move {
            if req.starts_with("HEAD ") {
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".to_vec()
            } else if req.starts_with("GET ") && req.to_ascii_lowercase().contains("range: bytes=0-0")
            {
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/12345\r\nContent-Length: 1\r\nContent-Disposition: attachment; filename=\"archive.tar.gz\"\r\nETag: \"abc123\"\r\nContent-Type: application/gzip\r\n\r\nx"
                    .to_vec()
            } else {
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/download"), PathBuf::from("."));
        req.security.allow_private_ips = true;

        let probe = client.probe(&req).await.unwrap();
        assert_eq!(probe.protocol, ProtocolKind::Http);
        assert_eq!(probe.total_size, Some(12345));
        assert!(probe.supports_ranges);
        assert_eq!(probe.suggested_filename.as_deref(), Some("archive.tar.gz"));
        assert_eq!(probe.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(probe.content_type.as_deref(), Some("application/gzip"));
    }

    #[tokio::test]
    async fn fetch_range_errors_when_server_ignores_range() {
        let (base_url, _hits) = spawn_mock_server(|_| async {
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789".to_vec()
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/data.bin"), PathBuf::from("."));
        req.security.allow_private_ips = true;

        let err = match client.fetch_range(&req, 5, Some(10)).await {
            Ok(_) => panic!("expected error when server ignores byte range"),
            Err(err) => err,
        };
        assert!(
            matches!(err, MayaError::Protocol(ref msg) if msg.contains("ignored byte range")),
            "expected Protocol error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn probe_rejects_payload_exceeding_max_download_bytes() {
        let (base_url, _hits) = spawn_mock_server(|_| async {
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nAccept-Ranges: bytes\r\n\r\n".to_vec()
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/big.iso"), PathBuf::from("."));
        req.security.allow_private_ips = true;
        req.security.max_download_bytes = Some(500);

        let err = client.probe(&req).await.unwrap_err();
        assert!(matches!(err, MayaError::SecurityViolation(_)));
    }

    #[tokio::test]
    async fn head_omitting_content_length_falls_back_to_range_zero() {
        let (base_url, _hits) = spawn_mock_server(|req| async move {
            if req.starts_with("HEAD ") {
                b"HTTP/1.1 200 OK\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\r\n"
                    .to_vec()
            } else if req.to_ascii_lowercase().contains("range: bytes=0-0") {
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/2048\r\nContent-Length: 1\r\n\r\na"
                    .to_vec()
            } else {
                b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/file.bin"), PathBuf::from("."));
        req.security.allow_private_ips = true;

        let probe = client.probe(&req).await.unwrap();
        assert_eq!(probe.total_size, Some(2048));
        assert!(probe.supports_ranges);
        assert_eq!(
            probe.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        assert_eq!(probe.suggested_filename.as_deref(), Some("file.bin"));
    }

    #[tokio::test]
    async fn fetch_range_streams_bounded_and_open_ranges() {
        let (base_url, _hits) = spawn_mock_server(|req| async move {
            let lower = req.to_ascii_lowercase();
            if lower.contains("range: bytes=2-5") {
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-5/10\r\nContent-Length: 4\r\n\r\n2345"
                    .to_vec()
            } else if lower.contains("range: bytes=6-") {
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 6-9/10\r\nContent-Length: 4\r\n\r\n6789"
                    .to_vec()
            } else {
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/payload.bin"), PathBuf::from("."));
        req.security.allow_private_ips = true;

        let mut stream1 = client.fetch_range(&req, 2, Some(6)).await.unwrap();
        let mut collected1 = Vec::new();
        while let Some(chunk) = stream1.next().await {
            collected1.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected1, b"2345");

        let mut stream2 = client.fetch_range(&req, 6, None).await.unwrap();
        let mut collected2 = Vec::new();
        while let Some(chunk) = stream2.next().await {
            collected2.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected2, b"6789");
    }

    #[tokio::test]
    async fn follows_valid_redirects_up_to_max_redirects() {
        let (base_url, _hits) = spawn_mock_server(|req| async move {
            if req.contains(" /step1 ") {
                b"HTTP/1.1 302 Found\r\nLocation: /step2\r\nContent-Length: 0\r\n\r\n".to_vec()
            } else if req.contains(" /step2 ") {
                b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /final.zip\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 512\r\nAccept-Ranges: bytes\r\n\r\n"
                    .to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();
        let mut req = DownloadRequest::new(format!("{base_url}/step1"), PathBuf::from("."));
        req.security.allow_private_ips = true;
        req.security.max_redirects = 2;

        let probe = client.probe(&req).await.unwrap();
        assert_eq!(probe.resolved_url, format!("{base_url}/final.zip"));
        assert_eq!(probe.total_size, Some(512));
        assert!(probe.supports_ranges);

        req.security.max_redirects = 1;
        let err = client.probe(&req).await.unwrap_err();
        assert!(matches!(err, MayaError::Protocol(_)));
    }

    #[tokio::test]
    async fn strips_sensitive_headers_on_cross_origin_redirect_and_keeps_same_origin() {
        use tokio::sync::Mutex;

        let cross_headers = Arc::new(Mutex::new(Vec::<String>::new()));
        let cross_headers_clone = Arc::clone(&cross_headers);

        let (cross_url, _) = spawn_mock_server(move |req| {
            let cross_headers = Arc::clone(&cross_headers_clone);
            async move {
                cross_headers.lock().await.push(req);
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\n\r\ndata".to_vec()
            }
        })
        .await;

        let same_headers = Arc::new(Mutex::new(Vec::<String>::new()));
        let same_headers_clone = Arc::clone(&same_headers);
        let cross_dest = format!("{cross_url}/target");

        let (origin_url, _) = spawn_mock_server(move |req| {
            let same_headers = Arc::clone(&same_headers_clone);
            let cross_dest = cross_dest.clone();
            async move {
                if req.contains(" /to-same ") {
                    b"HTTP/1.1 302 Found\r\nLocation: /same-dest\r\nContent-Length: 0\r\n\r\n"
                        .to_vec()
                } else if req.contains(" /same-dest ") {
                    same_headers.lock().await.push(req);
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\n\r\ndata"
                        .to_vec()
                } else if req.contains(" /to-cross ") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {cross_dest}\r\nContent-Length: 0\r\n\r\n"
                    )
                    .into_bytes()
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                }
            }
        })
        .await;

        let client = HttpProtocolClient::new();

        // 1. Same-origin redirect preserves Authorization, Cookie, Proxy-Authorization, and custom headers
        let mut same_req =
            DownloadRequest::new(format!("{origin_url}/to-same"), PathBuf::from("."));
        same_req.security.allow_private_ips = true;
        same_req.headers = vec![
            (
                "Authorization".to_string(),
                "Bearer secret-token".to_string(),
            ),
            ("Cookie".to_string(), "session=abc".to_string()),
            ("Proxy-Authorization".to_string(), "Basic xyz".to_string()),
            ("X-Custom".to_string(), "keep-me".to_string()),
        ];

        let mut same_stream = client.fetch_range(&same_req, 0, None).await.unwrap();
        while let Some(chunk) = same_stream.next().await {
            chunk.unwrap();
        }
        let recorded_same = same_headers.lock().await;
        assert_eq!(recorded_same.len(), 1);
        let lower_same = recorded_same[0].to_ascii_lowercase();
        assert!(lower_same.contains("authorization: bearer secret-token"));
        assert!(lower_same.contains("cookie: session=abc"));
        assert!(lower_same.contains("proxy-authorization: basic xyz"));
        assert!(lower_same.contains("x-custom: keep-me"));

        // 2. Cross-origin redirect strips Authorization, Cookie, and Proxy-Authorization while keeping X-Custom
        let mut cross_req =
            DownloadRequest::new(format!("{origin_url}/to-cross"), PathBuf::from("."));
        cross_req.security.allow_private_ips = true;
        cross_req.headers = same_req.headers.clone();

        let mut cross_stream = client.fetch_range(&cross_req, 0, None).await.unwrap();
        while let Some(chunk) = cross_stream.next().await {
            chunk.unwrap();
        }
        let recorded_cross = cross_headers.lock().await;
        assert_eq!(recorded_cross.len(), 1);
        let lower_cross = recorded_cross[0].to_ascii_lowercase();
        assert!(!lower_cross.contains("authorization:"));
        assert!(!lower_cross.contains("cookie:"));
        assert!(!lower_cross.contains("proxy-authorization:"));
        assert!(lower_cross.contains("x-custom: keep-me"));
    }

    #[tokio::test]
    async fn blocks_ipv4_mapped_ipv6_ssrf_and_redirect() {
        let (base_url, hits) = spawn_mock_server(|req| async move {
            if req.contains(" /redirect-v6 ") {
                b"HTTP/1.1 302 Found\r\nLocation: http://[::ffff:127.0.0.1]/secret\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntest".to_vec()
            }
        })
        .await;

        let client = HttpProtocolClient::new();

        // 1. Direct IPv4-mapped IPv6 loopback is blocked under default SecurityPolicy before connecting
        let port = Url::parse(&base_url).unwrap().port().unwrap();
        let direct_req = DownloadRequest::new(
            format!("http://[::ffff:127.0.0.1]:{port}/secret"),
            PathBuf::from("."),
        );
        let err = client.probe(&direct_req).await.unwrap_err();
        assert!(
            matches!(err, MayaError::SecurityViolation(_)),
            "expected SecurityViolation for IPv4-mapped IPv6, got {err:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        // 2. Redirect to http://[::ffff:127.0.0.1]/secret is also validated and rejected
        let mut redir_req =
            DownloadRequest::new(format!("{base_url}/redirect-v6"), PathBuf::from("."));
        redir_req.security.allow_private_ips = true;
        redir_req.security.allowed_hosts = vec!["127.0.0.1".to_string()];
        let err_redir = client.probe(&redir_req).await.unwrap_err();
        assert!(
            matches!(err_redir, MayaError::SecurityViolation(_)),
            "expected SecurityViolation for IPv4-mapped IPv6 redirect, got {err_redir:?}"
        );
    }
}
