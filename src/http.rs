//! A minimal, replaceable HTTP transport.
//!
//! The transport is synchronous: typical callers (desktop apps, command-line tools, workers)
//! wait for the reply anyway, and an async runtime would add weight without adding parallelism.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// The HTTP methods the client uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`, with a JSON body.
    Post,
    /// `DELETE`.
    Delete,
}

/// A request handed to a [`Transport`].
///
/// Its `Debug` output hides the `Authorization` header, so a request can be logged safely.
#[derive(Clone)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The absolute URL.
    pub url: String,
    /// Header names and values, in order.
    pub headers: Vec<(String, String)>,
    /// The JSON body, for `POST`.
    pub body: Option<String>,
    /// The time the whole request may take.
    pub timeout: Duration,
}

impl HttpRequest {
    /// The value of header `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str(),
                    if k.eq_ignore_ascii_case("authorization") {
                        "Bearer ***"
                    } else {
                        v.as_str()
                    },
                )
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .finish_non_exhaustive()
    }
}

/// A response returned by a [`Transport`].
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// Header names and values, in order.
    pub headers: Vec<(String, String)>,
    /// The body, as text.
    pub body: String,
}

impl HttpResponse {
    /// Whether the status is in the `2xx` range.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The value of header `name`, compared case-insensitively: servers spell headers such as
    /// `Retry-After` in different cases.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON: an empty body reads as `{}`, a body that isn't JSON as `None`.
    pub(crate) fn json(&self) -> Option<Value> {
        if self.body.trim().is_empty() {
            return Some(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(&self.body).ok()
    }
}

/// Sends the client's HTTP requests.
///
/// Implement it to use your own HTTP stack, a proxy, or a fake in tests, and pass it to
/// [`Fantastico::with_transport`](crate::Fantastico::with_transport).
pub trait Transport: Send + Sync {
    /// Send `request` and wait for the reply.
    ///
    /// # Errors
    ///
    /// A transport failure (DNS, TLS, connection, timeout) as text. An HTTP error status is not
    /// an `Err`: return it as an [`HttpResponse`] with its status.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String>;
}

/// A larger response body is refused rather than read into memory.
#[cfg(feature = "ureq")]
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// The default [`Transport`]: blocking `ureq` with rustls and the Mozilla root certificates.
#[cfg(feature = "ureq")]
#[cfg_attr(docsrs, doc(cfg(feature = "ureq")))]
pub struct UreqTransport {
    agent: ureq::Agent,
}

#[cfg(feature = "ureq")]
impl Default for UreqTransport {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

#[cfg(feature = "ureq")]
impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqTransport").finish_non_exhaustive()
    }
}

#[cfg(feature = "ureq")]
fn with_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    request: &HttpRequest,
) -> ureq::RequestBuilder<B> {
    for (k, v) in &request.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .config()
        .timeout_global(Some(request.timeout))
        .build()
}

#[cfg(feature = "ureq")]
impl Transport for UreqTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        let url = request.url.as_str();
        let result = match request.method {
            Method::Get => with_headers(self.agent.get(url), request).call(),
            Method::Delete => with_headers(self.agent.delete(url), request).call(),
            Method::Post => with_headers(self.agent.post(url), request)
                .send(request.body.as_deref().unwrap_or("")),
        };
        let mut response = result.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}
