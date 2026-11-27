//! Optional loadable gRPC exporter implementation for `otel-capi`.
//!
//! The core library encodes OTLP protobuf bytes and calls the registered
//! callback. This exporter wraps those bytes in gRPC frames and sends them
//! with the shared `ureq` HTTP client, backed by the host TLS stack. It does
//! not embed Tokio, tonic, or protobuf decoding.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;
use std::slice;

use otel_capi::abi::{
    find_core_symbol, init_handle, otel_exporter_request_t, otel_exporter_response_t,
    otel_exporter_send_fn, otel_exporter_t, otel_result_t, otel_runtime_t, otel_set_exporter_fn,
    otel_str_t, release_handle,
};
use otel_capi_http_transport::{
    Agent, HeaderMap, HeaderName, HeaderValue, Method, RequestOptions, build_request, new_agent,
    send,
};

static ERR_INVALID: &[u8] = b"gRPC exporter received invalid arguments";
static ERR_EXPORT: &[u8] = b"gRPC exporter export failed";
const EXPORTER_NAME: &[u8] = b"grpc\0";

/// Caller-owned handle for a gRPC exporter instance. Initialize with
/// `OTEL_GRPC_EXPORTER_T_INIT` and release with `otel_grpc_exporter_release`.
#[repr(C)]
pub struct otel_grpc_exporter_t {
    /// Descriptor read by the core runtime. The exporter owns its name.
    pub exporter: otel_exporter_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_grpc_exporter_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_grpc_exporter_options_t {
    /// Set to `sizeof(otel_grpc_exporter_options_t)`.
    pub size: usize,
    /// Reserved for ABI compatibility; ignored because export is blocking.
    pub worker_count: u32,
}

struct GrpcExporter {
    agent: Agent,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_exporter_send_fn = otel_grpc_exporter_send;

#[derive(Copy, Clone)]
enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    fn from_url(url: &str) -> Option<Self> {
        if url.contains("/v1/traces") {
            Some(Self::Traces)
        } else if url.contains("/v1/metrics") {
            Some(Self::Metrics)
        } else if url.contains("/v1/logs") {
            Some(Self::Logs)
        } else {
            None
        }
    }

    fn export_path(self) -> &'static str {
        match self {
            Self::Traces => "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            Self::Metrics => "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export",
            Self::Logs => "/opentelemetry.proto.collector.logs.v1.LogsService/Export",
        }
    }
}

/// Returns the ABI version implemented by this exporter.
#[unsafe(no_mangle)]
pub extern "C" fn otel_grpc_exporter_abi_version() -> u32 {
    1
}

/// Creates an exporter instance from caller-owned options.
///
/// `worker_count` is retained for ABI compatibility but ignored. HTTPS uses
/// the host's TLS implementation and trust store through `native-tls`.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_exporter_new(
    handle: *mut otel_grpc_exporter_t,
    options: *const otel_grpc_exporter_options_t,
) -> otel_result_t {
    if handle.is_null() || options.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    unsafe {
        ptr::write(
            ptr::addr_of_mut!((*handle).exporter),
            otel_exporter_t {
                size: size_of::<otel_exporter_t>(),
                name: otel_str_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                send: otel_grpc_exporter_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_grpc_exporter_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    let exporter = GrpcExporter {
        agent: new_agent(false, 0),
    };
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    // SAFETY: The caller provides writable, aligned handle storage; the core helper owns the pointer transfer.
    let status = unsafe { init_handle(slot, exporter) };
    if status != otel_result_t::OTEL_OK {
        return status;
    }
    // SAFETY: The handle is initialized and remains alive for the duration of this call.
    let user_data = unsafe { ptr::read(slot.cast::<*mut c_void>()) };
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        (*handle).exporter.name = otel_str_t {
            ptr: EXPORTER_NAME.as_ptr().cast(),
            len: EXPORTER_NAME.len() - 1,
        };
        (*handle).exporter.user_data = user_data;
    }
    otel_result_t::OTEL_OK
}

/// Implements `otel_exporter_send_fn` with raw gRPC framing over the shared
/// HTTP client.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_exporter_send(
    request: *const otel_exporter_request_t,
    response: *mut otel_exporter_response_t,
    user_data: *mut c_void,
) -> otel_result_t {
    if request.is_null() || response.is_null() || user_data.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    // SAFETY: the caller supplies valid views for the duration of this call.
    unsafe {
        *response = otel_exporter_response_t {
            status_code: 0,
            body: ptr::null(),
            body_len: 0,
            headers: ptr::null(),
            header_count: 0,
            error: otel_str_t {
                ptr: ptr::null(),
                len: 0,
            },
        };
    }

    // SAFETY: The caller guarantees the pointer references an initialized, aligned value for this call.
    let view = unsafe { &*request };
    // SAFETY: The caller guarantees the pointer references an initialized, aligned value for this call.
    let response = unsafe { &mut *response };
    // SAFETY: The caller guarantees the pointer references an initialized, aligned value for this call.
    let exporter = unsafe { &*(user_data.cast::<GrpcExporter>()) };
    if view.url.is_null() || view.body.is_null() || view.body_len > u32::MAX as usize {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    }

    // SAFETY: the core library passes a NUL-terminated URL.
    let url = unsafe { CStr::from_ptr(view.url) }.to_string_lossy();
    let signal = match Signal::from_url(&url) {
        Some(signal) => signal,
        None => {
            return set_error(
                response,
                ERR_INVALID,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
            );
        }
    };
    let endpoint = url.split("/v1/").next().unwrap_or(&url);
    if endpoint.is_empty() {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    }
    let url = format!("{}{}", endpoint.trim_end_matches('/'), signal.export_path());

    let mut extra_headers = vec![
        (
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/grpc"),
        ),
        (
            HeaderName::from_static("te"),
            HeaderValue::from_static("trailers"),
        ),
    ];
    if view.timeout_ms > 0 {
        let timeout = format!("{}n", view.timeout_ms.saturating_mul(1_000_000));
        let value = match HeaderValue::from_str(&timeout) {
            Ok(value) => value,
            Err(_) => {
                return set_error(
                    response,
                    ERR_INVALID,
                    otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
                );
            }
        };
        extra_headers.push((HeaderName::from_static("grpc-timeout"), value));
    }
    let skip_headers = ["content-type", "content-length", "te", "grpc-timeout"];

    // SAFETY: the core library guarantees `body_len` readable bytes.
    let body = unsafe { slice::from_raw_parts(view.body, view.body_len) };
    let mut framed = Vec::with_capacity(5 + body.len());
    framed.push(0);
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(body);
    let options = RequestOptions {
        extra_headers: &extra_headers,
        skip_headers: &skip_headers,
        body: Some(framed),
    };
    let request = match build_request(view, Method::POST, &url, &options) {
        Ok(request) => request,
        Err(_) => {
            return set_error(
                response,
                ERR_INVALID,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
            );
        }
    };
    let reply = match send(&exporter.agent, request, view.timeout_ms) {
        Ok(reply) => reply,
        Err(_) => return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT),
    };
    let status_code = reply.status_code();
    let grpc_status = grpc_status(reply.headers());
    let message = if (200..300).contains(&status_code) && grpc_status == Some(0) {
        match grpc_message(reply.body()) {
            Ok(message) => message.to_vec(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };
    // The core expects a protobuf response body, not the gRPC frame. The
    // transport copies the body into thread-local storage so the view stays
    // valid after this callback returns.
    reply.write_into_with_body(&message, response);
    if grpc_status.is_some_and(|status| status != 0) {
        return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT);
    }

    otel_result_t::OTEL_OK
}

/// Registers this exporter instance on a core runtime using the name `grpc`.
///
/// The core setter is resolved from the process so the loadable library does
/// not need a link-time dependency on the core shared library.
/// duration of the call.
///
/// # Safety
/// Both pointers must be non-null and reference initialized storage for the
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_exporter_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_grpc_exporter_t,
) -> otel_result_t {
    if runtime.is_null() || handle.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies an initialized handle for this call.
    if unsafe { ptr::read(ptr::addr_of!((*handle).inner)) }.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    let Some(set_exporter) =
        find_core_symbol::<otel_set_exporter_fn>(c"otel_runtime_set_exporter".to_bytes_with_nul())
    else {
        return otel_result_t::OTEL_ERR_INTERNAL;
    };
    // SAFETY: the handle embeds a core-ABI descriptor with this exporter's name.
    unsafe { set_exporter(runtime, ptr::addr_of!((*handle).exporter)) }
}

/// Releases an exporter instance created by `otel_grpc_exporter_new`.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_exporter_release(handle: *mut otel_grpc_exporter_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe { release_handle::<GrpcExporter>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>()) };
}

fn grpc_status(headers: &HeaderMap) -> Option<u16> {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

#[derive(Debug, Clone, Copy)]
enum GrpcFrameError {
    MissingPrefix,
    InvalidCompression,
    Truncated,
}

fn grpc_message(body: &[u8]) -> Result<&[u8], GrpcFrameError> {
    let (&compressed, length) = body.split_first().ok_or(GrpcFrameError::MissingPrefix)?;
    if compressed != 0 {
        return Err(GrpcFrameError::InvalidCompression);
    }
    if length.len() < 4 {
        return Err(GrpcFrameError::Truncated);
    }
    let len = u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
    let message = &length[4..];
    if message.len() < len {
        return Err(GrpcFrameError::Truncated);
    }
    Ok(&message[..len])
}

fn set_error(
    response: &mut otel_exporter_response_t,
    message: &'static [u8],
    status: otel_result_t,
) -> otel_result_t {
    response.error = otel_str_t {
        ptr: message.as_ptr().cast(),
        len: message.len(),
    };
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use otel_capi::abi::otel_exporter_header_t;
    use otel_capi_http_transport::ureq;
    use std::ffi::{CStr, CString};
    use std::mem::size_of;
    use std::ptr::NonNull;
    use std::slice;
    use ureq::config::Config;
    use ureq::http::Uri;
    use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
    };

    fn cstr(bytes: &[u8]) -> &CStr {
        CStr::from_bytes_with_nul(bytes).unwrap()
    }

    fn empty_handle() -> otel_grpc_exporter_t {
        otel_grpc_exporter_t {
            exporter: otel_exporter_t {
                size: size_of::<otel_exporter_t>(),
                name: otel_str_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                send: otel_grpc_exporter_send,
                user_data: ptr::null_mut(),
            },
            inner: ptr::null_mut(),
        }
    }

    fn new_handle() -> otel_grpc_exporter_t {
        let mut handle = empty_handle();
        let options = otel_grpc_exporter_options_t {
            size: size_of::<otel_grpc_exporter_options_t>(),
            worker_count: 1,
        };
        // SAFETY: `handle` is writable, aligned caller-owned storage and
        // `options` points to an initialized options struct.
        let status = unsafe { otel_grpc_exporter_new(&mut handle, &options) };
        assert_eq!(status, otel_result_t::OTEL_OK);
        handle
    }

    fn release(handle: &mut otel_grpc_exporter_t) {
        // SAFETY: `handle` was initialized by `otel_grpc_exporter_new`.
        unsafe { otel_grpc_exporter_release(handle) };
    }

    fn request_view(url: &CStr, body: &[u8]) -> otel_exporter_request_t {
        otel_exporter_request_t {
            method: cstr(b"POST\0").as_ptr(),
            url: url.as_ptr(),
            headers: ptr::null(),
            header_count: 0,
            body: body.as_ptr(),
            body_len: body.len(),
            timeout_ms: 0,
        }
    }

    fn error_bytes(response: &otel_exporter_response_t) -> &[u8] {
        // SAFETY: the callback filled `error` with a static byte string whose
        // pointer and length stay valid for the test.
        unsafe { slice::from_raw_parts(response.error.ptr.cast::<u8>(), response.error.len) }
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
                return Ok(false);
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

    fn framed(message: &[u8]) -> Vec<u8> {
        let mut frame = vec![0];
        frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
        frame.extend_from_slice(message);
        frame
    }

    #[cfg(all(not(miri), unix))]
    unsafe extern "C" {
        fn otel_runtime_new(handle: *mut otel_runtime_t) -> otel_result_t;
        fn otel_runtime_release(handle: *mut otel_runtime_t);
    }

    #[cfg(all(not(miri), unix))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn otel_runtime_set_exporter(
        _runtime: *mut otel_runtime_t,
        _exporter: *const otel_exporter_t,
    ) -> otel_result_t {
        otel_result_t::OTEL_OK
    }

    #[cfg(all(not(miri), unix))]
    #[used]
    static KEEP_TEST_SETTER: otel_set_exporter_fn = otel_runtime_set_exporter;

    #[test]
    fn signal_from_url_detects_each_export_path() {
        assert!(matches!(
            Signal::from_url("https://collector:4317/v1/traces"),
            Some(Signal::Traces)
        ));
        assert!(matches!(
            Signal::from_url("https://collector:4317/v1/metrics"),
            Some(Signal::Metrics)
        ));
        assert!(matches!(
            Signal::from_url("https://collector:4317/v1/logs"),
            Some(Signal::Logs)
        ));
        assert!(Signal::from_url("https://collector:4317/v1/unknown").is_none());
        assert!(Signal::from_url("").is_none());

        assert_eq!(
            Signal::Traces.export_path(),
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export"
        );
        assert_eq!(
            Signal::Metrics.export_path(),
            "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export"
        );
        assert_eq!(
            Signal::Logs.export_path(),
            "/opentelemetry.proto.collector.logs.v1.LogsService/Export"
        );
    }

    #[test]
    fn grpc_status_reads_valid_header_values() {
        let mut headers = HeaderMap::new();
        assert_eq!(grpc_status(&headers), None);

        headers.insert("grpc-status", HeaderValue::from_static("0"));
        assert_eq!(grpc_status(&headers), Some(0));
        headers.insert("grpc-status", HeaderValue::from_static("7"));
        assert_eq!(grpc_status(&headers), Some(7));
        headers.insert("grpc-status", HeaderValue::from_static("not-a-number"));
        assert_eq!(grpc_status(&headers), None);
        headers.insert("grpc-status", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(grpc_status(&headers), None);
    }

    #[test]
    fn grpc_message_parses_frames_and_rejects_bad_ones() {
        assert_eq!(
            grpc_message(&[0, 0, 0, 0, 3, b'a', b'b', b'c']).unwrap(),
            b"abc"
        );
        assert_eq!(
            grpc_message(&[0, 0, 0, 0, 3, b'a', b'b', b'c', b'd']).unwrap(),
            b"abc"
        );
        assert_eq!(grpc_message(&[0, 0, 0, 0, 0]).unwrap(), b"");

        assert!(matches!(
            grpc_message(&[]),
            Err(GrpcFrameError::MissingPrefix)
        ));
        assert!(matches!(
            grpc_message(&[1, 0, 0, 0, 0]),
            Err(GrpcFrameError::InvalidCompression)
        ));
        assert!(matches!(
            grpc_message(&[0, 0, 0]),
            Err(GrpcFrameError::Truncated)
        ));
        assert!(matches!(
            grpc_message(&[0, 0, 0, 0, 4, b'a']),
            Err(GrpcFrameError::Truncated)
        ));
    }

    #[test]
    fn abi_version_is_one() {
        assert_eq!(otel_grpc_exporter_abi_version(), 1);
    }

    #[test]
    fn new_rejects_null_arguments() {
        let mut handle = empty_handle();
        let options = otel_grpc_exporter_options_t {
            size: size_of::<otel_grpc_exporter_options_t>(),
            worker_count: 0,
        };

        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_new(ptr::null_mut(), &options) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: null options are rejected before they are dereferenced.
            unsafe { otel_grpc_exporter_new(&mut handle, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_rejects_invalid_options_size() {
        let mut handle = empty_handle();
        let options = otel_grpc_exporter_options_t {
            size: 1,
            worker_count: 0,
        };

        // SAFETY: the handle is writable storage and the options struct is initialized.
        let status = unsafe { otel_grpc_exporter_new(&mut handle, &options) };

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_initializes_descriptor_and_release_clears_it() {
        let mut handle = new_handle();

        assert_eq!(handle.exporter.size, size_of::<otel_exporter_t>());
        // SAFETY: `name` points into the exporter's static `grpc\0` byte string.
        let name = unsafe {
            slice::from_raw_parts(
                handle.exporter.name.ptr.cast::<u8>(),
                handle.exporter.name.len,
            )
        };
        assert_eq!(name, b"grpc".as_slice());
        let mut response = otel_exporter_response_t::EMPTY;
        assert_eq!(
            // SAFETY: a NULL request is rejected by the callback before it is dereferenced.
            unsafe {
                (handle.exporter.send)(ptr::null(), &mut response, handle.exporter.user_data)
            },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(!handle.inner.is_null());
        assert_eq!(handle.exporter.user_data, handle.inner);

        release(&mut handle);
        assert!(handle.inner.is_null());
        release(&mut handle);
        // SAFETY: null handle is explicitly a no-op.
        unsafe { otel_grpc_exporter_release(ptr::null_mut()) };
    }

    #[test]
    fn send_rejects_null_arguments() {
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"https://localhost:4317/v1/traces\0"), b"body");
        let dangling = NonNull::<u8>::dangling().as_ptr().cast::<c_void>();

        assert_eq!(
            // SAFETY: a null request is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_send(ptr::null(), &mut response, dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null response is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_send(&view, ptr::null_mut(), dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null user_data is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_send(&view, &mut response, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_rejects_invalid_url_and_endpoint() {
        let mut handle = new_handle();
        let mut response = otel_exporter_response_t::EMPTY;
        let bad_utf8 = CString::new([0xffu8, 0xfe]).unwrap();
        let unknown_signal = request_view(cstr(b"https://localhost:4317/v1/unknown\0"), b"body");
        let mut null_url = request_view(cstr(b"https://localhost:4317/v1/traces\0"), b"body");
        null_url.url = ptr::null();
        let empty_endpoint = request_view(cstr(b"/v1/traces\0"), b"body");
        let invalid_url = request_view(&bad_utf8, b"body");

        for view in [&invalid_url, &unknown_signal, &null_url, &empty_endpoint] {
            // SAFETY: all views are initialized, `response` is writable, and
            // `user_data` belongs to the live handle held for this test.
            let status =
                unsafe { otel_grpc_exporter_send(view, &mut response, handle.exporter.user_data) };
            assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
            assert_eq!(error_bytes(&response), ERR_INVALID);
        }

        release(&mut handle);
    }

    #[test]
    fn send_rejects_invalid_body() {
        let mut handle = new_handle();
        let mut response = otel_exporter_response_t::EMPTY;
        let url = cstr(b"https://localhost:4317/v1/traces\0");
        let mut null_body = request_view(url, b"body");
        null_body.body = ptr::null();
        let mut oversized = request_view(url, b"body");
        oversized.body_len = u32::MAX as usize + 1;

        for view in [&null_body, &oversized] {
            // SAFETY: both views are rejected before the body is read;
            // `user_data` belongs to the live handle held for this test.
            let status =
                unsafe { otel_grpc_exporter_send(view, &mut response, handle.exporter.user_data) };
            assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
            assert_eq!(error_bytes(&response), ERR_INVALID);
        }

        release(&mut handle);
    }

    #[test]
    fn send_rejects_bad_core_header_with_timeout() {
        let mut handle = new_handle();
        let mut response = otel_exporter_response_t::EMPTY;
        let bad_name = CString::new("bad name").unwrap();
        let value = CString::new("value").unwrap();
        let headers = [otel_exporter_header_t {
            name: otel_str_t {
                ptr: bad_name.as_ptr(),
                len: bad_name.as_bytes().len(),
            },
            value: otel_str_t {
                ptr: value.as_ptr(),
                len: value.as_bytes().len(),
            },
        }];
        let mut view = request_view(cstr(b"https://localhost:4317/v1/traces\0"), b"body");
        view.headers = headers.as_ptr();
        view.header_count = 1;
        view.timeout_ms = 5000;

        // SAFETY: the request and response are initialized, all borrowed views
        // stay alive, and `user_data` belongs to the live handle. The bad core
        // header makes request construction fail before any network I/O.
        let status =
            unsafe { otel_grpc_exporter_send(&view, &mut response, handle.exporter.user_data) };

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert_eq!(error_bytes(&response), ERR_INVALID);
        release(&mut handle);
    }

    #[test]
    fn register_rejects_null_arguments_and_uninitialized_handle() {
        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };
        let handle = empty_handle();

        assert_eq!(
            // SAFETY: a null runtime is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_register(ptr::null_mut(), &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_grpc_exporter_register(&mut runtime, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: the handle has no inner pointer, so no core symbol is called.
            unsafe { otel_grpc_exporter_register(&mut runtime, &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_returns_decoded_grpc_message() {
        let reply = http_reply(
            &[("content-type", "application/grpc"), ("grpc-status", "0")],
            &framed(b"decoded"),
        );
        let exporter = GrpcExporter {
            agent: mock_agent(reply, false),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let mut view = request_view(cstr(b"http://localhost:4317/v1/traces\0"), b"payload");
        view.timeout_ms = 60_000;

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `GrpcExporter`.
        let status = unsafe {
            otel_grpc_exporter_send(
                &view,
                &mut response,
                (&exporter as *const GrpcExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.body_len, 7);
        // SAFETY: the exporter copied the decoded message into thread-local
        // storage that outlives the callback and this local `exporter`.
        let body = unsafe { slice::from_raw_parts(response.body, response.body_len) };
        assert_eq!(body, b"decoded");
        assert_eq!(response.error.len, 0);
    }

    #[test]
    fn send_accepts_reply_without_frame() {
        let exporter = GrpcExporter {
            agent: mock_agent(http_reply(&[("grpc-status", "0")], b"\x01"), false),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let mut view = request_view(cstr(b"http://localhost:4317/v1/traces\0"), b"payload");
        view.timeout_ms = 60_000;

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `GrpcExporter`.
        let status = unsafe {
            otel_grpc_exporter_send(
                &view,
                &mut response,
                (&exporter as *const GrpcExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.body_len, 0);
    }

    #[test]
    fn send_reports_nonzero_grpc_status() {
        let exporter = GrpcExporter {
            agent: mock_agent(http_reply(&[("grpc-status", "7")], b""), false),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"http://localhost:4317/v1/traces\0"), b"payload");

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `GrpcExporter`.
        let status = unsafe {
            otel_grpc_exporter_send(
                &view,
                &mut response,
                (&exporter as *const GrpcExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_ERR_EXPORT);
        assert_eq!(error_bytes(&response), ERR_EXPORT);
    }

    #[test]
    fn send_reports_exporter_failure() {
        let exporter = GrpcExporter {
            agent: mock_agent(Vec::new(), true),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"http://localhost:4317/v1/traces\0"), b"payload");

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `GrpcExporter`.
        let status = unsafe {
            otel_grpc_exporter_send(
                &view,
                &mut response,
                (&exporter as *const GrpcExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_ERR_EXPORT);
        assert_eq!(error_bytes(&response), ERR_EXPORT);
    }

    #[cfg(all(not(miri), unix))]
    #[test]
    fn register_uses_core_setter_for_initialized_handle() {
        let mut handle = new_handle();
        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };

        assert_eq!(
            // SAFETY: `runtime` is writable, aligned storage for this handle type.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );

        assert_eq!(
            // SAFETY: both handles are initialized and stay alive for this call.
            unsafe { otel_grpc_exporter_register(&mut runtime, &handle) },
            otel_result_t::OTEL_OK
        );

        // SAFETY: `runtime` is initialized and this drops our reference exactly once.
        unsafe { otel_runtime_release(&mut runtime) };
        release(&mut handle);
    }
}
