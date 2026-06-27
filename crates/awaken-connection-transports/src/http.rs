//! HTTP request/response transport over the connection seam.

use async_trait::async_trait;
use awaken_connection::{Channel, ConnectError, Transport};
use awaken_connection_auth::{HeaderAuthMaterial, header_name_is_safe, header_value_is_safe};
use bytes::Bytes;
use reqwest::Method;
use serde_json::Value as JsonValue;
use thiserror::Error;
use url::Url;

/// Errors raised by the reusable HTTP transport.
#[derive(Debug, Error)]
pub enum HttpTransportError {
    /// The base URL is invalid or unsupported.
    #[error("invalid HTTP base URL: {0}")]
    InvalidBaseUrl(String),
    /// The request method is not a safe HTTP token.
    #[error("unsafe HTTP method: {0:?}")]
    UnsafeMethod(String),
    /// The request path could escape or corrupt the target URL/request line.
    #[error("unsafe HTTP path: {0:?}")]
    UnsafePath(String),
    /// A request header cannot be safely serialized.
    #[error("unsafe HTTP header: {0:?}")]
    UnsafeHeader(String),
    /// Reqwest returned an error.
    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

impl From<HttpTransportError> for ConnectError {
    fn from(error: HttpTransportError) -> Self {
        match error {
            HttpTransportError::InvalidBaseUrl(message) => ConnectError::Setup(message),
            HttpTransportError::UnsafeMethod(message)
            | HttpTransportError::UnsafePath(message)
            | HttpTransportError::UnsafeHeader(message) => ConnectError::Setup(message),
            HttpTransportError::Request(error) => ConnectError::Io(error.to_string()),
        }
    }
}

/// Base URL for an HTTP channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAddress {
    base_url: Url,
}

impl HttpAddress {
    /// Parse and validate an HTTP(S) base URL.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, HttpTransportError> {
        let raw = base_url.as_ref();
        let parsed = Url::parse(raw)
            .map_err(|error| HttpTransportError::InvalidBaseUrl(error.to_string()))?;
        match parsed.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(HttpTransportError::InvalidBaseUrl(format!(
                    "unsupported scheme `{scheme}`"
                )));
            }
        }
        if parsed.host_str().is_none() {
            return Err(HttpTransportError::InvalidBaseUrl(
                "missing host".to_string(),
            ));
        }
        Ok(Self { base_url: parsed })
    }

    /// The validated base URL.
    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.base_url
    }

    /// Build a URL under this address from a rooted path.
    pub fn request_url(&self, path: &str) -> Result<Url, HttpTransportError> {
        if !path_is_safe(path) {
            return Err(HttpTransportError::UnsafePath(path.to_string()));
        }
        let mut base = self.base_url.as_str().trim_end_matches('/').to_string();
        if path.is_empty() {
            base.push('/');
        } else {
            base.push_str(path);
        }
        Url::parse(&base).map_err(|error| HttpTransportError::InvalidBaseUrl(error.to_string()))
    }
}

/// A reusable HTTP transport.
#[derive(Clone, Debug, Default)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// Build a transport using `client`.
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Bind a client to a base address and already-materialized header material.
    ///
    /// HTTP channel establishment performs no network I/O; requests are issued
    /// later per exchange.
    #[must_use]
    pub fn establish(&self, addr: HttpAddress, material: &HeaderAuthMaterial) -> HttpChannel {
        HttpChannel {
            client: self.client.clone(),
            address: addr,
            material: material.clone(),
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    type Channel = HttpChannel;
    type Address = HttpAddress;
    type HandshakeMaterial = HeaderAuthMaterial;

    fn scheme(&self) -> &str {
        "http"
    }

    async fn dial(
        &self,
        addr: Self::Address,
        material: &Self::HandshakeMaterial,
    ) -> Result<Self::Channel, ConnectError> {
        Ok(self.establish(addr, material))
    }
}

/// An HTTP channel bound to one base URL and one material set.
#[derive(Clone, Debug)]
pub struct HttpChannel {
    client: reqwest::Client,
    address: HttpAddress,
    material: HeaderAuthMaterial,
}

impl Channel for HttpChannel {}

impl HttpChannel {
    /// The reqwest client this channel exchanges over.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The base address this channel is bound to.
    #[must_use]
    pub fn address(&self) -> &HttpAddress {
        &self.address
    }

    /// The already-materialized header material bound to this channel.
    #[must_use]
    pub fn material(&self) -> &HeaderAuthMaterial {
        &self.material
    }

    fn apply_material(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in self.material.headers() {
            builder = builder.header(name, value);
        }
        builder
    }

    /// Build a request builder to an absolute URL string, applying channel material.
    pub fn build_request_to_url_str(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        self.apply_material(self.client.request(method, url))
    }

    /// Build a request builder to an absolute URL, applying channel material.
    pub fn build_request_to_url(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        self.apply_material(self.client.request(method, url))
    }

    /// Build a request builder to a rooted path under the channel base URL.
    pub fn build_request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, HttpTransportError> {
        let url = self.address.request_url(path)?;
        Ok(self.build_request_to_url(method, url))
    }

    /// Issue a request over this channel.
    pub async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        request.validate()?;
        let url = self.address.request_url(&request.path)?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| HttpTransportError::UnsafeMethod(request.method.clone()))?;
        let mut builder = self.build_request_to_url(method, url);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.json(&body);
        }
        let response = builder.send().await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    Bytes::copy_from_slice(value.as_bytes()),
                )
            })
            .collect();
        let body = response.bytes().await?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Request data accepted by [`HttpChannel`].
#[derive(Clone, Debug, PartialEq)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: String,
    /// Rooted path relative to the channel base URL.
    pub path: String,
    /// Extra headers to send with the request.
    pub headers: Vec<(String, String)>,
    /// Optional JSON body.
    pub body: Option<JsonValue>,
}

impl HttpRequest {
    /// Build a request with no extra headers or body.
    #[must_use]
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Attach one safe header.
    pub fn with_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, HttpTransportError> {
        let name = name.into();
        let value = value.into();
        validate_header(&name, &value)?;
        self.headers.push((name, value));
        Ok(self)
    }

    /// Attach a JSON body.
    #[must_use]
    pub fn with_json(mut self, body: JsonValue) -> Self {
        self.body = Some(body);
        self
    }

    fn validate(&self) -> Result<(), HttpTransportError> {
        if !method_is_safe(&self.method) {
            return Err(HttpTransportError::UnsafeMethod(self.method.clone()));
        }
        if !path_is_safe(&self.path) {
            return Err(HttpTransportError::UnsafePath(self.path.clone()));
        }
        for (name, value) in &self.headers {
            validate_header(name, value)?;
        }
        Ok(())
    }
}

/// Response data returned by [`HttpChannel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as raw field-value bytes. No UTF-8 decoding is assumed.
    pub headers: Vec<(String, Bytes)>,
    /// Raw response body.
    pub body: Bytes,
}

fn method_is_safe(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= 16
        && method.bytes().all(|byte| byte.is_ascii_alphabetic())
}

fn path_is_safe(path: &str) -> bool {
    (path.is_empty() || path.starts_with('/'))
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && !path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

fn validate_header(name: &str, value: &str) -> Result<(), HttpTransportError> {
    if !header_name_is_safe(name) || !header_value_is_safe(value) {
        return Err(HttpTransportError::UnsafeHeader(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_connection::Transport;
    use reqwest::header::AUTHORIZATION;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;

    async fn one_shot_server(response_body: &'static str) -> (HttpAddress, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_for_task = Arc::clone(&captured);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 8192];
            let n = stream.read(&mut buffer).await.unwrap();
            *captured_for_task.lock().await = String::from_utf8_lossy(&buffer[..n]).into_owned();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (
            HttpAddress::new(format!("http://{addr}/api")).unwrap(),
            captured,
        )
    }

    async fn raw_response_server(response: &'static [u8]) -> HttpAddress {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 8192];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream.write_all(response).await.unwrap();
        });
        HttpAddress::new(format!("http://{addr}/api")).unwrap()
    }

    #[tokio::test]
    async fn dial_is_lazy_and_request_sends_material_headers_and_json_body() {
        let (address, captured) = one_shot_server(r#"{"ok":true}"#).await;
        let material = HeaderAuthMaterial::bearer("tok_123").unwrap();
        let channel = HttpTransport::default()
            .dial(address, &material)
            .await
            .unwrap();

        let response = channel
            .request(
                HttpRequest::new("POST", "/messages")
                    .with_header("X-Trace", "abc")
                    .unwrap()
                    .with_json(serde_json::json!({ "text": "hello" })),
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, Bytes::from_static(br#"{"ok":true}"#));
        let request = captured.lock().await.clone();
        assert!(
            request.starts_with("POST /api/messages HTTP/1.1"),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer tok_123"),
            "{request}"
        );
        assert!(request.contains("x-trace: abc"), "{request}");
        assert!(request.contains(r#"{"text":"hello"}"#), "{request}");
    }

    #[test]
    fn build_request_applies_material_without_network_io() {
        let address = HttpAddress::new("https://example.com/api").unwrap();
        let material = HeaderAuthMaterial::bearer("tok_builder").unwrap();
        let channel = HttpTransport::default().establish(address, &material);

        let request = channel
            .build_request(Method::GET, "/inspect")
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(request.url().as_str(), "https://example.com/api/inspect");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer tok_builder"
        );
    }

    #[tokio::test]
    async fn rejects_non_http_base_url() {
        let error = HttpAddress::new("ftp://example.com").unwrap_err();
        assert!(matches!(error, HttpTransportError::InvalidBaseUrl(_)));
    }

    #[tokio::test]
    async fn response_headers_are_retained_as_raw_bytes() {
        let address = raw_response_server(
            b"HTTP/1.1 204 No Content\r\nX-Raw: \x80\xff\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let channel = HttpTransport::default()
            .dial(address, &HeaderAuthMaterial::none())
            .await
            .unwrap();

        let response = channel
            .request(HttpRequest::new("GET", "/raw"))
            .await
            .unwrap();

        assert_eq!(response.status, 204);
        let raw_header = response
            .headers
            .iter()
            .find(|(name, _)| name == "x-raw")
            .expect("raw header");
        assert_eq!(raw_header.1, Bytes::from_static(&[0x80, 0xff]));
    }

    #[tokio::test]
    async fn rejects_unsafe_request_before_network() {
        let address = HttpAddress::new("http://127.0.0.1:9").unwrap();
        let channel = HttpTransport::default()
            .dial(address, &HeaderAuthMaterial::none())
            .await
            .unwrap();

        assert!(matches!(
            channel
                .request(HttpRequest::new("GET /x", "/ok"))
                .await
                .unwrap_err(),
            HttpTransportError::UnsafeMethod(_)
        ));
        assert!(matches!(
            channel
                .request(HttpRequest::new("GET", "relative"))
                .await
                .unwrap_err(),
            HttpTransportError::UnsafePath(_)
        ));
    }

    #[test]
    fn request_builder_rejects_unsafe_header() {
        assert!(matches!(
            HttpRequest::new("GET", "/ok")
                .with_header("X-Test", "bad\r\nx")
                .unwrap_err(),
            HttpTransportError::UnsafeHeader(_)
        ));
    }

    #[test]
    fn request_url_rejects_dot_segments_that_escape_base_path() {
        let address = HttpAddress::new("https://example.com/api/v1").unwrap();
        for path in ["/../admin", "/./admin", "/users/../../admin"] {
            assert!(matches!(
                address.request_url(path).unwrap_err(),
                HttpTransportError::UnsafePath(_)
            ));
        }
    }

    #[test]
    fn base_url_path_is_preserved() {
        let address = HttpAddress::new("https://example.com/api/").unwrap();
        assert_eq!(
            address.request_url("/v1").unwrap().as_str(),
            "https://example.com/api/v1"
        );
        assert_eq!(
            address.request_url("").unwrap().as_str(),
            "https://example.com/api/"
        );
    }
}
