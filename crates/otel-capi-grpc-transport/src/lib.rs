//! Optional loadable gRPC transport implementation for `otel-capi`.
//!
//! The core library encodes OTLP protobuf bytes and calls the registered
//! callback. This transport wraps those bytes in gRPC frames and sends them
//! with the shared `ureq` HTTP client, backed by the host TLS stack. It does
//! not embed Tokio, tonic, or protobuf decoding.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;
use std::slice;

use otel_capi::abi::{
    find_core_symbol, init_handle, otel_result_t, otel_runtime_t, otel_set_transport_fn,
    otel_str_t, otel_transport_request_t, otel_transport_response_t, otel_transport_send_fn,
    otel_transport_t, release_handle,
};
use otel_capi_http_client::{
    Agent, HeaderMap, HeaderName, HeaderValue, Method, RequestOptions, build_request, new_agent,
    send,
};

static ERR_INVALID: &[u8] = b"gRPC transport received invalid arguments";
static ERR_EXPORT: &[u8] = b"gRPC transport export failed";
const TRANSPORT_NAME: &[u8] = b"grpc\0";

/// Caller-owned handle for a gRPC transport instance. Initialize with
/// `OTEL_GRPC_TRANSPORT_T_INIT` and release with `otel_grpc_transport_release`.
#[repr(C)]
pub struct otel_grpc_transport_t {
    /// Descriptor read by the core runtime. The transport owns its name.
    pub transport: otel_transport_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_grpc_transport_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_grpc_transport_options_t {
    /// Set to `sizeof(otel_grpc_transport_options_t)`.
    pub size: usize,
    /// Reserved for ABI compatibility; ignored because export is blocking.
    pub worker_count: u32,
}

struct GrpcTransport {
    agent: Agent,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_transport_send_fn = otel_grpc_transport_send;

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

/// Returns the ABI version implemented by this transport.
#[unsafe(no_mangle)]
pub extern "C" fn otel_grpc_transport_abi_version() -> u32 {
    1
}

/// Creates a transport instance from caller-owned options.
///
/// `worker_count` is retained for ABI compatibility but ignored. HTTPS uses
/// the host's TLS implementation and trust store through `native-tls`.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_transport_new(
    handle: *mut otel_grpc_transport_t,
    options: *const otel_grpc_transport_options_t,
) -> otel_result_t {
    if handle.is_null() || options.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    unsafe {
        ptr::write(
            ptr::addr_of_mut!((*handle).transport),
            otel_transport_t {
                size: size_of::<otel_transport_t>(),
                name: otel_str_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                send: otel_grpc_transport_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_grpc_transport_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    let transport = GrpcTransport {
        agent: new_agent(false, 0),
    };
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    // SAFETY: The caller provides writable, aligned handle storage; the core helper owns the pointer transfer.
    let status = unsafe { init_handle(slot, transport) };
    if status != otel_result_t::OTEL_OK {
        return status;
    }
    // SAFETY: The handle is initialized and remains alive for the duration of this call.
    let user_data = unsafe { ptr::read(slot.cast::<*mut c_void>()) };
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        (*handle).transport.name = otel_str_t {
            ptr: TRANSPORT_NAME.as_ptr().cast(),
            len: TRANSPORT_NAME.len() - 1,
        };
        (*handle).transport.user_data = user_data;
    }
    otel_result_t::OTEL_OK
}

/// Implements `otel_transport_send_fn` with raw gRPC framing over the shared
/// HTTP client.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_transport_send(
    request: *const otel_transport_request_t,
    response: *mut otel_transport_response_t,
    user_data: *mut c_void,
) -> otel_result_t {
    if request.is_null() || response.is_null() || user_data.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    // SAFETY: the caller supplies valid views for the duration of this call.
    unsafe {
        *response = otel_transport_response_t {
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
    let transport = unsafe { &*(user_data.cast::<GrpcTransport>()) };
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
    let reply = match send(&transport.agent, request, view.timeout_ms) {
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
    reply.write_into(response);
    // The core expects a protobuf response body, not the gRPC frame.
    if message.is_empty() {
        response.body = ptr::null();
        response.body_len = 0;
    } else {
        response.body = message.as_ptr();
        response.body_len = message.len();
    }
    if grpc_status.is_some_and(|status| status != 0) {
        return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT);
    }

    // `reply` remains alive until the core library copies the response.
    otel_result_t::OTEL_OK
}

/// Registers this transport instance on a core runtime using the name `grpc`.
///
/// The core setter is resolved from the process so the loadable library does
/// not need a link-time dependency on the core shared library.
/// duration of the call.
///
/// # Safety
/// Both pointers must be non-null and reference initialized storage for the
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_transport_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_grpc_transport_t,
) -> otel_result_t {
    if runtime.is_null() || handle.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies an initialized handle for this call.
    if unsafe { ptr::read(ptr::addr_of!((*handle).inner)) }.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    let Some(set_transport) = find_core_symbol::<otel_set_transport_fn>(
        c"otel_runtime_set_transport".to_bytes_with_nul(),
    ) else {
        return otel_result_t::OTEL_ERR_INTERNAL;
    };
    // SAFETY: the handle embeds a core-ABI descriptor with this transport's name.
    unsafe { set_transport(runtime, ptr::addr_of!((*handle).transport)) }
}

/// Releases a transport instance created by `otel_grpc_transport_new`.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_grpc_transport_release(handle: *mut otel_grpc_transport_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe { release_handle::<GrpcTransport>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>()) };
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
    response: &mut otel_transport_response_t,
    message: &'static [u8],
    status: otel_result_t,
) -> otel_result_t {
    response.error = otel_str_t {
        ptr: message.as_ptr().cast(),
        len: message.len(),
    };
    status
}
