//! Optional loadable stdout exporter for `otel-capi`.
//!
//! The core library encodes OTLP protobuf bytes and calls the registered
//! stdout transport. This transport decodes the batch and prints a short summary,
//! so the core library does not link `opentelemetry-stdout`.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;
use std::slice;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use otel_capi::abi::{
    find_core_symbol, init_handle, otel_result_t, otel_runtime_t, otel_set_transport_fn,
    otel_str_t, otel_transport_request_t, otel_transport_response_t, otel_transport_send_fn,
    otel_transport_t, release_handle,
};
use prost::Message;

static ERR_INVALID: &[u8] = b"stdout transport received invalid arguments";
static ERR_EXPORT: &[u8] = b"stdout transport could not decode OTLP bytes";
const TRANSPORT_NAME: &[u8] = b"stdout\0";

/// Caller-owned handle for a stdout transport instance. Initialize with
/// `OTEL_STDOUT_TRANSPORT_T_INIT` and release with `otel_stdout_transport_release`.
#[repr(C)]
pub struct otel_stdout_transport_t {
    /// Descriptor read by the core runtime. The transport owns its name.
    pub transport: otel_transport_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_stdout_transport_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_stdout_transport_options_t {
    /// Set to `sizeof(otel_stdout_transport_options_t)`.
    pub size: usize,
    /// Include the signal URL in each summary line. Defaults to `true`.
    pub print_url: bool,
}

struct StdoutTransport {
    print_url: bool,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_transport_send_fn = otel_stdout_transport_send;

/// Returns the ABI version implemented by this transport.
#[unsafe(no_mangle)]
pub extern "C" fn otel_stdout_transport_abi_version() -> u32 {
    1
}

/// Creates a transport instance from caller-owned options.
///
/// Writes into `handle`; initialize it with `OTEL_STDOUT_TRANSPORT_T_INIT`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_transport_new(
    handle: *mut otel_stdout_transport_t,
    options: *const otel_stdout_transport_options_t,
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
                send: otel_stdout_transport_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_stdout_transport_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    let status = unsafe {
        init_handle(
            slot,
            StdoutTransport {
                print_url: options.print_url,
            },
        )
    };
    if status != otel_result_t::OTEL_OK {
        return status;
    }
    let user_data = unsafe { ptr::read(slot.cast::<*mut c_void>()) };
    unsafe {
        (*handle).transport.name = otel_str_t {
            ptr: TRANSPORT_NAME.as_ptr().cast(),
            len: TRANSPORT_NAME.len() - 1,
        };
        (*handle).transport.user_data = user_data;
    }
    otel_result_t::OTEL_OK
}

/// Implements `otel_transport_send_fn` by decoding and summarizing one OTLP batch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_transport_send(
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

    let view = unsafe { &*request };
    let response = unsafe { &mut *response };
    let transport = unsafe { &*(user_data.cast::<StdoutTransport>()) };
    if view.url.is_null() || view.body.is_null() {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    }
    // SAFETY: the core library passes a NUL-terminated URL and `body_len` bytes.
    let url = unsafe { CStr::from_ptr(view.url) }.to_string_lossy();
    let body = unsafe { slice::from_raw_parts(view.body, view.body_len) };

    let summary = if url.contains("/v1/traces") {
        match ExportTraceServiceRequest::decode(body) {
            Ok(request) => format!(
                "{} resource span group(s), {} scope group(s)",
                request.resource_spans.len(),
                request
                    .resource_spans
                    .iter()
                    .map(|group| group.scope_spans.len())
                    .sum::<usize>()
            ),
            Err(_) => return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT),
        }
    } else if url.contains("/v1/metrics") {
        match ExportMetricsServiceRequest::decode(body) {
            Ok(request) => format!(
                "{} resource metric group(s), {} metric(s)",
                request.resource_metrics.len(),
                request
                    .resource_metrics
                    .iter()
                    .flat_map(|group| group.scope_metrics.iter())
                    .map(|scope| scope.metrics.len())
                    .sum::<usize>()
            ),
            Err(_) => return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT),
        }
    } else if url.contains("/v1/logs") {
        match ExportLogsServiceRequest::decode(body) {
            Ok(request) => format!(
                "{} resource log group(s), {} log record(s)",
                request.resource_logs.len(),
                request
                    .resource_logs
                    .iter()
                    .flat_map(|group| group.scope_logs.iter())
                    .map(|scope| scope.log_records.len())
                    .sum::<usize>()
            ),
            Err(_) => return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT),
        }
    } else {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    };

    if transport.print_url {
        println!("stdout transport: {url} ({summary})");
    } else {
        println!("stdout transport: {summary}");
    }
    response.status_code = 200;
    otel_result_t::OTEL_OK
}

/// Registers this transport instance on a core runtime using the name `stdout`.
#[unsafe(no_mangle)]
pub extern "C" fn otel_stdout_transport_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_stdout_transport_t,
) -> otel_result_t {
    if runtime.is_null() || handle.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies an initialized handle for this call.
    if unsafe { ptr::read(ptr::addr_of!((*handle).inner)) }.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    let Some(set_transport) =
        find_core_symbol::<otel_set_transport_fn>(c"otel_runtime_set_transport".to_bytes_with_nul())
    else {
        return otel_result_t::OTEL_ERR_INTERNAL;
    };
    // SAFETY: the handle embeds a core-ABI descriptor with this transport's name.
    unsafe { set_transport(runtime, ptr::addr_of!((*handle).transport)) }
}

/// Releases a transport instance created by `otel_stdout_transport_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_transport_release(handle: *mut otel_stdout_transport_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe {
        release_handle::<StdoutTransport>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>())
    };
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
