//! Shared blocking HTTP client for the loadable `otel-capi` transports.
//!
//! Both the HTTP and gRPC transports use this crate so they share one `ureq`
//! version, one system-TLS configuration, and one ABI-to-response conversion.

use std::ffi::CString;
use std::ptr;
use std::slice;
use std::time::Duration;

use otel_capi::abi::{
    otel_str_t, otel_transport_header_t, otel_transport_request_t, otel_transport_response_t,
};
pub use ureq::http::header::{HeaderMap, HeaderName, HeaderValue};
pub use ureq::http::{Method, Request};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
pub use ureq::{Agent, Body};

/// Failure converting or sending a core transport request.
#[derive(Debug, Clone, Copy)]
pub enum RequestError {
    /// A required pointer was NULL or a view had an invalid length.
    InvalidArgument,
    /// A header name or value could not be represented.
    InvalidHeader,
    /// The request body could not be represented.
    InvalidBody,
    /// The HTTP client could not complete the request.
    Transport,
}

/// Per-request overrides used when converting a core transport request.
#[derive(Default)]
pub struct RequestOptions<'a> {
    /// Headers appended before headers supplied by the core.
    pub extra_headers: &'a [(HeaderName, HeaderValue)],
    /// Core header names that must not be forwarded.
    pub skip_headers: &'a [&'a str],
    /// Replaces the body supplied by the core, used for gRPC framing.
    pub body: Option<Vec<u8>>,
}

/// Creates the shared blocking client using the host TLS stack and trust store.
pub fn new_agent(follow_redirects: bool, connect_timeout_ms: u64) -> Agent {
    let mut config = Agent::config_builder()
        .max_redirects(if follow_redirects { 10 } else { 0 })
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        );
    if connect_timeout_ms > 0 {
        config = config.timeout_connect(Some(Duration::from_millis(connect_timeout_ms)));
    }
    Agent::new_with_config(config.build())
}

/// Converts the core ABI request into a `ureq` request.
///
/// The caller validates `method` and `url`; this function validates the
/// caller-owned header and body views.
pub fn build_request(
    view: &otel_transport_request_t,
    method: Method,
    url: &str,
    options: &RequestOptions<'_>,
) -> Result<Request<Vec<u8>>, RequestError> {
    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in options.extra_headers {
        builder = builder.header(name.clone(), value.clone());
    }
    if view.header_count > 0 {
        if view.headers.is_null() {
            return Err(RequestError::InvalidArgument);
        }
        // SAFETY: the core library supplies `header_count` readable headers.
        let headers = unsafe { slice::from_raw_parts(view.headers, view.header_count) };
        for header in headers {
            if header.name.ptr.is_null() || header.value.ptr.is_null() {
                return Err(RequestError::InvalidArgument);
            }
            // SAFETY: the core library supplies borrowed views for this call.
            let name = unsafe { str_slice(header.name) };
            // SAFETY: The caller guarantees the pointer and length describe readable, initialized bytes for this call.
            let value = unsafe { str_slice(header.value) };
            let name = HeaderName::from_bytes(name).map_err(|_| RequestError::InvalidHeader)?;
            if options
                .skip_headers
                .iter()
                .any(|skipped| name.as_str().eq_ignore_ascii_case(skipped))
            {
                continue;
            }
            let value = HeaderValue::from_bytes(value).map_err(|_| RequestError::InvalidHeader)?;
            builder = builder.header(name, value);
        }
    }
    let body = if let Some(body) = &options.body {
        body.clone()
    } else if view.body_len == 0 {
        Vec::new()
    } else if view.body.is_null() {
        return Err(RequestError::InvalidArgument);
    } else {
        // SAFETY: the core library guarantees `body_len` readable bytes.
        unsafe { slice::from_raw_parts(view.body, view.body_len) }.to_vec()
    };
    builder.body(body).map_err(|_| RequestError::InvalidBody)
}

/// Sends a request with the core-provided timeout.
pub fn send(
    agent: &Agent,
    request: Request<Vec<u8>>,
    timeout_ms: u64,
) -> Result<HttpResponse, RequestError> {
    let request = agent
        .configure_request(request)
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .build();
    HttpResponse::new(agent.run(request).map_err(|_| RequestError::Transport)?)
}

/// A response whose C-string backing memory stays alive for the ABI callback.
pub struct HttpResponse {
    status_code: u16,
    headers: HeaderMap,
    body: Vec<u8>,
    owned: Vec<CString>,
    header_views: Vec<otel_transport_header_t>,
}

impl HttpResponse {
    fn new(reply: ureq::http::Response<Body>) -> Result<Self, RequestError> {
        let status_code = reply.status().as_u16();
        let headers = reply.headers().clone();
        let mut owned = Vec::new();
        let mut header_views = Vec::new();
        for (name, value) in &headers {
            let name = CString::new(name.as_str()).map_err(|_| RequestError::InvalidHeader)?;
            let value = CString::new(value.as_bytes()).map_err(|_| RequestError::InvalidHeader)?;
            header_views.push(otel_transport_header_t {
                name: otel_str_t {
                    ptr: name.as_ptr().cast(),
                    len: name.as_bytes().len(),
                },
                value: otel_str_t {
                    ptr: value.as_ptr(),
                    len: value.as_bytes().len(),
                },
            });
            owned.push(name);
            owned.push(value);
        }
        let body = reply
            .into_body()
            .read_to_vec()
            .map_err(|_| RequestError::Transport)?;
        Ok(Self {
            status_code,
            headers,
            body,
            owned,
            header_views,
        })
    }

    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Copies borrowed views into `response`. `self` must outlive the callback.
    pub fn write_into(&self, response: &mut otel_transport_response_t) {
        // Keep the C-strings backing `header_views` alive for this call.
        let _ = &self.owned;
        response.status_code = self.status_code;
        response.headers = if self.header_views.is_empty() {
            ptr::null()
        } else {
            self.header_views.as_ptr()
        };
        response.header_count = self.header_views.len();
        response.body = if self.body.is_empty() {
            ptr::null()
        } else {
            self.body.as_ptr()
        };
        response.body_len = self.body.len();
    }
}

unsafe fn str_slice(view: otel_str_t) -> &'static [u8] {
    if view.ptr.is_null() {
        return &[];
    }
    // SAFETY: the caller guarantees the view is readable and length-aware.
    unsafe { slice::from_raw_parts(view.ptr.cast::<u8>(), view.len) }
}
