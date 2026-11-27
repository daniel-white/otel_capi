//! Generic callback transport adapter for OTLP exporters.
//!
//! A C caller registers a callback by name on `otel_runtime_t`. The core
//! library encodes telemetry with `opentelemetry-otlp` and invokes the selected
//! callback synchronously from the exporter's own background thread. The core
//! contains no HTTP client, Tokio reactor, or protocol-specific implementation;
//! concrete transports live in their own loadable crates.

use std::ffi::{CStr, CString, c_void};
use std::fmt;
use std::slice;
use std::time::Duration;

use async_trait::async_trait;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};

use crate::abi::{
    otel_str_t, otel_transport_header_t, otel_transport_request_t, otel_transport_response_t,
    otel_transport_send_fn,
};
use crate::config;
use crate::error::{Error, otel_result_t, otel_strerror, set_error};
use crate::handles::RuntimeInner;
use crate::strings::str_slice;

/// Function pointer and opaque state for a named transport.
#[derive(Copy, Clone)]
pub(crate) struct Transport {
    pub(crate) send: otel_transport_send_fn,
    pub(crate) user_data: *mut c_void,
}

// SAFETY: `user_data` is owned by the C caller, which must keep it valid and
// safe for concurrent access until the runtime is released. The transport is
// only invoked from OpenTelemetry exporter threads.
unsafe impl Send for Transport {}
// SAFETY: the callback and `user_data` are safe for concurrent access for as
// long as the runtime holds the transport.
unsafe impl Sync for Transport {}

impl Transport {
    pub(crate) fn new(send: otel_transport_send_fn, user_data: *mut c_void) -> Self {
        Self { send, user_data }
    }
}

/// Adapts a registered C callback to `opentelemetry_http::HttpClient`.
pub(crate) struct CallbackTransportClient {
    name: String,
    transport: Transport,
    timeout_ms: u64,
}

impl CallbackTransportClient {
    pub(crate) fn new(name: String, transport: Transport, timeout: Duration) -> Self {
        Self {
            name,
            transport,
            timeout_ms: timeout.as_millis() as u64,
        }
    }
}

impl fmt::Debug for CallbackTransportClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackTransportClient")
            .field("name", &self.name)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}

/// Builds the callback client for `name` from the runtime registry.
pub(crate) fn callback_client(
    runtime: &RuntimeInner,
    name: &str,
    signal: &str,
) -> crate::error::Result<CallbackTransportClient> {
    let transport = runtime.transport(name)?.ok_or_else(|| {
        Error::new(
            otel_result_t::OTEL_ERR_CONFIG,
            format!(
                "transport \"{name}\" is not registered; call otel_runtime_set_transport before creating providers"
            ),
        )
    })?;
    Ok(CallbackTransportClient::new(
        name.to_owned(),
        transport,
        config::otlp_timeout(signal)?,
    ))
}

#[async_trait]
impl HttpClient for CallbackTransportClient {
    async fn send_bytes(
        &self,
        request: Request<Bytes>,
    ) -> std::result::Result<Response<Bytes>, HttpError> {
        let response = self.invoke(request)?;
        Ok(response)
    }
}

impl CallbackTransportClient {
    fn invoke(&self, request: Request<Bytes>) -> std::result::Result<Response<Bytes>, HttpError> {
        let mut owned = Vec::with_capacity(2 + request.headers().len() * 2);
        owned.push(c_string(request.method().as_str().as_bytes(), "method")?);
        owned.push(c_string(request.uri().to_string().as_bytes(), "URL")?);
        for (name, value) in request.headers() {
            owned.push(c_string(name.as_str().as_bytes(), "header name")?);
            owned.push(c_string(value.as_bytes(), "header value")?);
        }

        let headers: Vec<otel_transport_header_t> = owned[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| otel_transport_header_t {
                name: otel_str_t {
                    ptr: pair[0].as_ptr(),
                    len: pair[0].as_bytes().len(),
                },
                value: otel_str_t {
                    ptr: pair[1].as_ptr(),
                    len: pair[1].as_bytes().len(),
                },
            })
            .collect();
        let body = request.body();
        let request_view = otel_transport_request_t {
            method: owned[0].as_ptr(),
            url: owned[1].as_ptr(),
            headers: headers.as_ptr(),
            header_count: headers.len(),
            body: body.as_ptr(),
            body_len: body.len(),
            timeout_ms: self.timeout_ms,
        };
        let mut response_view = otel_transport_response_t::EMPTY;

        // SAFETY: the callback is registered with a live runtime and `owned`
        // keeps every borrowed string alive for the call.
        let status = unsafe {
            (self.transport.send)(&request_view, &mut response_view, self.transport.user_data)
        };
        if status != otel_result_t::OTEL_OK {
            // SAFETY: The caller guarantees the preconditions of this unsafe operation.
            let detail = unsafe { response_error(&response_view) };
            let message = match detail {
                Some(detail) => format!("transport \"{}\" failed: {detail}", self.name),
                None => format!(
                    "transport \"{}\" failed: {}",
                    self.name,
                    status_text(status)
                ),
            };
            return Err(callback_error(status, message));
        }

        if !(100..=599).contains(&response_view.status_code) {
            return Err(callback_error(
                otel_result_t::OTEL_ERR_EXPORT,
                format!(
                    "transport \"{}\" returned invalid status code {}",
                    self.name, response_view.status_code
                ),
            ));
        }

        let mut builder = Response::builder().status(response_view.status_code);
        if response_view.header_count > 0 {
            if response_view.headers.is_null() {
                return Err(callback_error(
                    otel_result_t::OTEL_ERR_EXPORT,
                    format!("transport \"{}\" response headers are NULL", self.name),
                ));
            }
            // SAFETY: the callback guarantees this array for `header_count`.
            let headers =
                unsafe { slice::from_raw_parts(response_view.headers, response_view.header_count) };
            for header in headers {
                // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
                let name = unsafe { str_slice(header.name, "response header name") }
                    .map_err(convert_error)?;
                // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
                let value = unsafe { str_slice(header.value, "response header value") }
                    .map_err(convert_error)?;
                builder = builder.header(name, value);
            }
        }

        let body = if response_view.body_len == 0 {
            Bytes::new()
        } else {
            if response_view.body.is_null() {
                return Err(callback_error(
                    otel_result_t::OTEL_ERR_EXPORT,
                    format!("transport \"{}\" response body is NULL", self.name),
                ));
            }
            // SAFETY: the callback guarantees `body_len` readable bytes.
            let bytes =
                unsafe { slice::from_raw_parts(response_view.body, response_view.body_len) };
            Bytes::copy_from_slice(bytes)
        };

        builder
            .body(body)
            .map_err(|err| callback_error(otel_result_t::OTEL_ERR_EXPORT, err.to_string()))
    }
}

fn c_string(bytes: &[u8], name: &str) -> std::result::Result<CString, HttpError> {
    CString::new(bytes).map_err(|_| {
        callback_error(
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
            format!("{name} contains a NUL byte"),
        )
    })
}

unsafe fn response_error(response: &otel_transport_response_t) -> Option<String> {
    if response.error.ptr.is_null() || response.error.len == 0 {
        return None;
    }
    // SAFETY: the callback supplies a valid view for the call.
    unsafe { str_slice(response.error, "transport error") }
        .ok()
        .map(str::to_owned)
}

fn status_text(status: otel_result_t) -> String {
    // SAFETY: `otel_strerror` returns a pointer to a static C string.
    unsafe { CStr::from_ptr(otel_strerror(status as i32)) }
        .to_string_lossy()
        .into_owned()
}

fn callback_error(status: otel_result_t, message: impl Into<String>) -> HttpError {
    let message = message.into();
    set_error(status, &message);
    Box::new(CallbackError { status, message })
}

fn convert_error(error: Error) -> HttpError {
    callback_error(error.status, error.message)
}

#[derive(Debug)]
struct CallbackError {
    status: otel_result_t,
    message: String,
}

impl fmt::Display for CallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transport callback failed ({}): {}",
            self.status as i32, self.message
        )
    }
}

impl std::error::Error for CallbackError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_char;
    use std::ptr;

    #[derive(Default)]
    struct Observed {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u64,
    }

    // The core copies response data after the callback returns, so these views
    // must point to storage that outlives the callback body.
    #[derive(Copy, Clone)]
    struct Headers([otel_transport_header_t; 1]);

    // SAFETY: Static test headers are immutable and point only to static bytes.
    unsafe impl Sync for Headers {}

    static RESPONSE_BODY: &[u8] = b"accepted";
    static RESPONSE_HEADERS: Headers = Headers([otel_transport_header_t {
        name: otel_str_t {
            ptr: b"x-test".as_ptr().cast::<c_char>(),
            len: b"x-test".len(),
        },
        value: otel_str_t {
            ptr: b"yes".as_ptr().cast::<c_char>(),
            len: b"yes".len(),
        },
    }]);
    static INVALID_UTF8_HEADERS: Headers = Headers([otel_transport_header_t {
        name: otel_str_t {
            ptr: b"\xff".as_ptr().cast::<c_char>(),
            len: 1,
        },
        value: otel_str_t {
            ptr: b"yes".as_ptr().cast::<c_char>(),
            len: b"yes".len(),
        },
    }]);

    fn request() -> Request<Bytes> {
        Request::builder()
            .method("POST")
            .uri("https://example.com/v1/traces")
            .header("content-type", "application/x-protobuf")
            .body(Bytes::from_static(b"payload"))
            .unwrap()
    }

    fn client(send: otel_transport_send_fn) -> CallbackTransportClient {
        CallbackTransportClient::new(
            "unit".to_owned(),
            Transport::new(send, ptr::null_mut()),
            Duration::from_millis(250),
        )
    }

    unsafe extern "C" fn record_and_succeed(
        request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: The test invokes this callback with valid borrowed request and
        // response pointers, and `user_data` points to a live `Observed` for this call.
        unsafe {
            let view = &*request;
            let observed = &mut *user_data.cast::<Observed>();
            observed.method = CStr::from_ptr(view.method).to_string_lossy().into_owned();
            observed.url = CStr::from_ptr(view.url).to_string_lossy().into_owned();
            for header in slice::from_raw_parts(view.headers, view.header_count) {
                let name = str_slice(header.name, "header name").unwrap();
                let value = str_slice(header.value, "header value").unwrap();
                observed.headers.push((name.to_owned(), value.to_owned()));
            }
            observed.body = slice::from_raw_parts(view.body, view.body_len).to_vec();
            observed.timeout_ms = view.timeout_ms;

            *response = otel_transport_response_t {
                status_code: 202,
                body: RESPONSE_BODY.as_ptr(),
                body_len: RESPONSE_BODY.len(),
                headers: RESPONSE_HEADERS.0.as_ptr(),
                header_count: RESPONSE_HEADERS.0.len(),
                ..otel_transport_response_t::EMPTY
            };
        }
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn fail_with_error(
        _request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        let message = b"backend down";
        // SAFETY: The test supplies a valid response pointer for this call.
        unsafe {
            (*response).error = otel_str_t {
                ptr: message.as_ptr().cast::<c_char>(),
                len: message.len(),
            };
        }
        otel_result_t::OTEL_ERR_EXPORT
    }

    unsafe extern "C" fn fail_without_error(
        _request: *const otel_transport_request_t,
        _response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        otel_result_t::OTEL_ERR_INTERNAL
    }

    unsafe extern "C" fn invalid_status(
        _request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: The test supplies a valid response pointer for this call.
        unsafe { (*response).status_code = 99 };
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn null_headers(
        _request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: The test supplies a valid response pointer for this call.
        unsafe {
            (*response).status_code = 200;
            (*response).header_count = 1;
        }
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn null_body(
        _request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: The test supplies a valid response pointer for this call.
        unsafe {
            (*response).status_code = 200;
            (*response).body_len = 1;
        }
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn invalid_utf8_header(
        _request: *const otel_transport_request_t,
        response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: The test supplies a valid response pointer for this call.
        unsafe {
            *response = otel_transport_response_t {
                status_code: 200,
                headers: INVALID_UTF8_HEADERS.0.as_ptr(),
                header_count: INVALID_UTF8_HEADERS.0.len(),
                ..otel_transport_response_t::EMPTY
            };
        }
        otel_result_t::OTEL_OK
    }

    #[test]
    fn invoke_passes_request_fields_and_copies_response() {
        let mut observed = Observed::default();
        let callback = CallbackTransportClient::new(
            "unit".to_owned(),
            Transport::new(
                record_and_succeed,
                (&mut observed as *mut Observed).cast::<c_void>(),
            ),
            Duration::from_millis(250),
        );

        let response = callback.invoke(request()).unwrap();
        assert_eq!(response.status(), 202);
        assert_eq!(response.body().as_ref(), &b"accepted"[..]);
        assert_eq!(response.headers().get("x-test").unwrap(), "yes");

        assert_eq!(observed.method, "POST");
        assert_eq!(observed.url, "https://example.com/v1/traces");
        assert_eq!(
            observed.headers,
            vec![(
                "content-type".to_owned(),
                "application/x-protobuf".to_owned()
            )]
        );
        assert_eq!(observed.body, b"payload");
        assert_eq!(observed.timeout_ms, 250);
    }

    #[test]
    fn invoke_reports_callback_failure_and_error_view() {
        let error = client(fail_with_error).invoke(request()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "transport callback failed (4): transport \"unit\" failed: backend down"
        );

        let error = client(fail_without_error).invoke(request()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transport \"unit\" failed: internal error")
        );
    }

    #[test]
    fn invoke_rejects_invalid_status_and_null_response_pointers() {
        let error = client(invalid_status).invoke(request()).unwrap_err();
        assert!(error.to_string().contains("invalid status code 99"));

        let error = client(null_headers).invoke(request()).unwrap_err();
        assert!(error.to_string().contains("response headers are NULL"));

        let error = client(null_body).invoke(request()).unwrap_err();
        assert!(error.to_string().contains("response body is NULL"));
    }

    #[test]
    fn invoke_rejects_invalid_utf8_response_headers() {
        let error = client(invalid_utf8_header).invoke(request()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("response header name is not valid UTF-8")
        );
        assert_eq!(
            error.downcast_ref::<CallbackError>().unwrap().status,
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
    }
}
