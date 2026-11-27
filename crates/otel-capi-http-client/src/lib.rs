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
pub use ureq::{self, Agent, Body};

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
        let mut owned = Vec::with_capacity(headers.len() * 2);
        for (name, value) in &headers {
            owned.push(CString::new(name.as_str()).map_err(|_| RequestError::InvalidHeader)?);
            owned.push(CString::new(value.as_bytes()).map_err(|_| RequestError::InvalidHeader)?);
        }
        // Take views only after every CString is in its final storage so the
        // pointers stay valid under Stacked Borrows.
        let header_views = owned
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let name = &pair[0];
                let value = &pair[1];
                otel_transport_header_t {
                    name: otel_str_t {
                        ptr: name.as_ptr().cast(),
                        len: name.as_bytes().len(),
                    },
                    value: otel_str_t {
                        ptr: value.as_ptr(),
                        len: value.as_bytes().len(),
                    },
                }
            })
            .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use otel_capi::abi::otel_transport_header_t;
    use std::ffi::CStr;
    use std::io;
    use ureq::config::Config;
    use ureq::http::Uri;
    use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
    };

    fn header_view(name: &CStr, value: &CStr) -> otel_transport_header_t {
        otel_transport_header_t {
            name: otel_str_t {
                ptr: name.as_ptr(),
                len: name.to_bytes().len(),
            },
            value: otel_str_t {
                ptr: value.as_ptr(),
                len: value.to_bytes().len(),
            },
        }
    }

    fn request_view(headers: &[otel_transport_header_t], body: &[u8]) -> otel_transport_request_t {
        otel_transport_request_t {
            method: c"POST".as_ptr(),
            url: c"http://localhost:4318/v1/traces".as_ptr(),
            headers: headers.as_ptr(),
            header_count: headers.len(),
            body: body.as_ptr(),
            body_len: body.len(),
            timeout_ms: 1000,
        }
    }

    fn response_with(headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
        let mut builder = ureq::http::Response::builder().status(201);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let reply = builder.body(Body::builder().data(body.to_vec())).unwrap();
        HttpResponse::new(reply).unwrap()
    }

    #[derive(Debug)]
    struct MockConnector {
        reply: Vec<u8>,
        fail: bool,
    }

    impl Connector<()> for MockConnector {
        type Out = MockTransport;

        fn connect(
            &self,
            _details: &ConnectionDetails,
            _chained: Option<()>,
        ) -> Result<Option<Self::Out>, ureq::Error> {
            if self.fail {
                return Err(ureq::Error::ConnectionFailed);
            }
            Ok(Some(MockTransport {
                buffers: LazyBuffers::new(1024, 1024),
                reply: self.reply.clone(),
                offset: 0,
            }))
        }
    }

    #[derive(Debug)]
    struct MockTransport {
        buffers: LazyBuffers,
        reply: Vec<u8>,
        offset: usize,
    }

    impl Transport for MockTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(
            &mut self,
            amount: usize,
            _timeout: NextTimeout,
        ) -> Result<(), ureq::Error> {
            let _ = &self.buffers.output()[..amount];
            Ok(())
        }

        fn await_input(&mut self, _timeout: NextTimeout) -> Result<bool, ureq::Error> {
            if self.offset >= self.reply.len() {
                return Err(ureq::Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mock response consumed",
                )));
            }
            let appended = {
                let input = self.buffers.input_append_buf();
                let chunk = &self.reply[self.offset..];
                let count = input.len().min(chunk.len());
                input[..count].copy_from_slice(&chunk[..count]);
                count
            };
            self.offset += appended;
            self.buffers.input_appended(appended);
            Ok(appended > 0)
        }

        fn is_open(&mut self) -> bool {
            self.offset < self.reply.len()
        }
    }

    #[derive(Debug)]
    struct MockResolver;

    impl Resolver for MockResolver {
        fn resolve(
            &self,
            _uri: &Uri,
            _config: &Config,
            _timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            let mut addrs = self.empty();
            addrs.push("127.0.0.1:80".parse().unwrap());
            Ok(addrs)
        }
    }

    fn mock_agent(reply: Vec<u8>, fail: bool) -> Agent {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        Agent::with_parts(config, MockConnector { reply, fail }, MockResolver)
    }

    fn http_reply(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut reply = b"HTTP/1.1 200 OK\r\n".to_vec();
        for (name, value) in headers {
            reply.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        reply.extend_from_slice(
            format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        reply.extend_from_slice(body);
        reply
    }

    #[test]
    fn build_request_copies_headers_and_body() {
        let name = CString::new("content-type").unwrap();
        let value = CString::new("application/x-protobuf").unwrap();
        let headers = [header_view(&name, &value)];
        let body = b"encoded payload";
        let view = request_view(&headers, body);

        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        )
        .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().to_string(), "http://localhost:4318/v1/traces");
        assert_eq!(
            request.headers().get("content-type").unwrap(),
            "application/x-protobuf"
        );
        assert_eq!(request.body().as_slice(), body.as_slice());
    }

    #[test]
    fn build_request_accepts_empty_view() {
        let view = request_view(&[], b"");
        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        )
        .unwrap();

        assert!(request.headers().is_empty());
        assert!(request.body().is_empty());
    }

    #[test]
    fn build_request_adds_extra_headers_and_skips_core_headers() {
        let skipped = CString::new("Content-Type").unwrap();
        let skipped_value = CString::new("text/plain").unwrap();
        let kept = CString::new("x-keep").unwrap();
        let kept_value = CString::new("kept").unwrap();
        let headers = [
            header_view(&skipped, &skipped_value),
            header_view(&kept, &kept_value),
        ];
        let view = request_view(&headers, b"body");
        let extra_headers = [(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/grpc"),
        )];
        let skip_headers = ["content-type"];
        let options = RequestOptions {
            extra_headers: &extra_headers,
            skip_headers: &skip_headers,
            body: None,
        };

        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &options,
        )
        .unwrap();

        assert_eq!(
            request.headers().get("content-type").unwrap(),
            "application/grpc"
        );
        assert_eq!(request.headers().get("x-keep").unwrap(), "kept");
        assert_eq!(request.headers().len(), 2);
    }

    #[test]
    fn build_request_rejects_null_headers_with_nonzero_count() {
        let mut view = request_view(&[], b"body");
        view.headers = ptr::null();
        view.header_count = 1;

        let result = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );

        assert!(matches!(result, Err(RequestError::InvalidArgument)));
    }

    #[test]
    fn build_request_rejects_null_header_name_and_value() {
        let name = CString::new("x-test").unwrap();
        let value = CString::new("value").unwrap();
        let mut null_name = header_view(&name, &value);
        null_name.name.ptr = ptr::null();
        let mut null_value = header_view(&name, &value);
        null_value.value.ptr = ptr::null();
        let null_name_headers = [null_name];
        let null_value_headers = [null_value];
        let mut null_name_view = request_view(&null_name_headers, b"body");
        let mut null_value_view = request_view(&null_value_headers, b"body");
        null_name_view.headers = null_name_headers.as_ptr();
        null_value_view.headers = null_value_headers.as_ptr();

        let name_result = build_request(
            &null_name_view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );
        let value_result = build_request(
            &null_value_view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );

        assert!(matches!(name_result, Err(RequestError::InvalidArgument)));
        assert!(matches!(value_result, Err(RequestError::InvalidArgument)));
    }

    #[test]
    fn build_request_rejects_invalid_header_name_and_value() {
        let bad_name = CString::new("bad name").unwrap();
        let good_value = CString::new("value").unwrap();
        let good_name = CString::new("x-test").unwrap();
        let bad_value = CString::new("bad\nvalue").unwrap();
        let bad_name_headers = [header_view(&bad_name, &good_value)];
        let bad_value_headers = [header_view(&good_name, &bad_value)];
        let mut name_view = request_view(&bad_name_headers, b"body");
        let mut value_view = request_view(&bad_value_headers, b"body");
        name_view.headers = bad_name_headers.as_ptr();
        value_view.headers = bad_value_headers.as_ptr();

        let name_result = build_request(
            &name_view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );
        let value_result = build_request(
            &value_view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );

        assert!(matches!(name_result, Err(RequestError::InvalidHeader)));
        assert!(matches!(value_result, Err(RequestError::InvalidHeader)));
    }

    #[test]
    fn build_request_rejects_null_body_with_nonzero_length() {
        let mut view = request_view(&[], b"body");
        view.body = ptr::null();

        let result = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        );

        assert!(matches!(result, Err(RequestError::InvalidArgument)));
    }

    #[test]
    fn build_request_body_override_replaces_core_body() {
        let mut view = request_view(&[], b"core body");
        view.body = ptr::null();
        let options = RequestOptions {
            body: Some(b"override".to_vec()),
            ..RequestOptions::default()
        };

        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &options,
        )
        .unwrap();

        assert_eq!(request.body().as_slice(), b"override");
    }

    #[test]
    fn http_response_conversion_keeps_views_valid() {
        let response = response_with(
            &[
                ("content-type", "application/x-protobuf"),
                ("x-test", "value"),
            ],
            b"reply bytes",
        );
        let mut view = otel_transport_response_t::EMPTY;

        assert_eq!(response.status_code(), 201);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/x-protobuf"
        );
        assert_eq!(response.body(), b"reply bytes");

        response.write_into(&mut view);
        assert_eq!(view.status_code, 201);
        assert_eq!(view.header_count, 2);
        assert_eq!(view.body_len, 11);
        // SAFETY: `view` was filled by `write_into` from a live `response`, and
        // `response` stays alive while the borrowed header views are read.
        let headers = unsafe { slice::from_raw_parts(view.headers, view.header_count) };
        let mut names = Vec::new();
        for header in headers {
            // SAFETY: the header name view points into a C string owned by `response`.
            let name =
                unsafe { slice::from_raw_parts(header.name.ptr.cast::<u8>(), header.name.len) };
            names.push(String::from_utf8_lossy(name).into_owned());
        }
        assert!(names.iter().any(|name| name == "content-type"));
        assert!(names.iter().any(|name| name == "x-test"));
        // SAFETY: `view.body` points into `response.body()`, which is still alive.
        let body = unsafe { slice::from_raw_parts(view.body, view.body_len) };
        assert_eq!(body, b"reply bytes");
    }

    #[test]
    fn http_response_empty_views_are_null() {
        let response = response_with(&[], b"");
        let mut view = otel_transport_response_t::EMPTY;

        response.write_into(&mut view);

        assert_eq!(view.status_code, 201);
        assert!(view.headers.is_null());
        assert_eq!(view.header_count, 0);
        assert!(view.body.is_null());
        assert_eq!(view.body_len, 0);
    }

    #[test]
    fn new_agent_applies_optional_connect_timeout() {
        let _ = new_agent(true, 25);
        let _ = new_agent(false, 0);
    }

    #[test]
    fn send_uses_configured_agent_and_returns_response() {
        let reply = http_reply(&[("content-type", "application/x-protobuf")], b"reply");
        let agent = mock_agent(reply, false);
        let view = request_view(&[], b"payload");
        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        )
        .unwrap();

        let response = send(&agent, request, 1000).unwrap();

        assert_eq!(response.status_code(), 200);
        assert_eq!(response.body(), b"reply");
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/x-protobuf"
        );
    }

    #[test]
    fn send_reports_transport_and_body_read_errors() {
        let view = request_view(&[], b"payload");

        let failed = mock_agent(Vec::new(), true);
        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            send(&failed, request, 1000),
            Err(RequestError::Transport)
        ));

        let truncated =
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nshort".to_vec();
        let agent = mock_agent(truncated, false);
        let request = build_request(
            &view,
            Method::POST,
            "http://localhost:4318/v1/traces",
            &RequestOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            send(&agent, request, 1000),
            Err(RequestError::Transport)
        ));
    }

    #[test]
    fn str_slice_accepts_null_view() {
        let view = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: a NULL pointer with zero length returns an empty slice without
        // dereferencing the pointer.
        assert!(unsafe { str_slice(view) }.is_empty());
    }
}
