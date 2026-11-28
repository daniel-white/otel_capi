//! Optional loadable stdout exporter for `otel-capi`.
//!
//! The core library encodes OTLP protobuf bytes and calls the registered
//! stdout exporter. This exporter decodes the batch and prints a short summary,
//! so the core library does not link `opentelemetry-stdout`.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;
use std::slice;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use otel_capi::abi::{
    find_core_symbol, init_handle, otel_exporter_request_t, otel_exporter_response_t,
    otel_exporter_send_fn, otel_exporter_t, otel_result_t, otel_runtime_t, otel_set_exporter_fn,
    otel_str_t, release_handle,
};
use prost::Message;

static ERR_INVALID: &[u8] = b"stdout exporter received invalid arguments";
static ERR_EXPORT: &[u8] = b"stdout exporter could not decode OTLP bytes";
const EXPORTER_NAME: &[u8] = b"stdout\0";

/// Caller-owned handle for a stdout exporter instance. Initialize with
/// `OTEL_STDOUT_EXPORTER_T_INIT` and release with `otel_stdout_exporter_release`.
#[repr(C)]
pub struct otel_stdout_exporter_t {
    /// Descriptor read by the core runtime. The exporter owns its name.
    pub exporter: otel_exporter_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_stdout_exporter_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_stdout_exporter_options_t {
    /// Set to `sizeof(otel_stdout_exporter_options_t)`.
    pub size: usize,
    /// Include the signal URL in each summary line. Defaults to `true`.
    pub print_url: bool,
}

struct StdoutExporter {
    print_url: bool,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_exporter_send_fn = otel_stdout_exporter_send;

/// Returns the ABI version implemented by this exporter.
#[unsafe(no_mangle)]
pub extern "C" fn otel_stdout_exporter_abi_version() -> u32 {
    1
}

/// Creates an exporter instance from caller-owned options.
///
/// Writes into `handle`; initialize it with `OTEL_STDOUT_EXPORTER_T_INIT`.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_exporter_new(
    handle: *mut otel_stdout_exporter_t,
    options: *const otel_stdout_exporter_options_t,
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
                send: otel_stdout_exporter_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_stdout_exporter_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    let status = unsafe {
        init_handle(
            slot,
            StdoutExporter {
                print_url: options.print_url,
            },
        )
    };
    if status != otel_result_t::OTEL_OK {
        return status;
    }
    // SAFETY: The caller guarantees the pointer targets valid, aligned storage for this operation.
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

/// Implements `otel_exporter_send_fn` by decoding and summarizing one OTLP batch.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_exporter_send(
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
    let exporter = unsafe { &*(user_data.cast::<StdoutExporter>()) };
    if view.url.is_null() || view.body.is_null() {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    }
    // SAFETY: the core library passes a NUL-terminated URL and `body_len` bytes.
    let url = unsafe { CStr::from_ptr(view.url) }.to_string_lossy();
    // SAFETY: The caller guarantees the pointer and length describe readable, initialized bytes for this call.
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

    if exporter.print_url {
        println!("stdout exporter: {url} ({summary})");
    } else {
        println!("stdout exporter: {summary}");
    }
    response.status_code = 200;
    otel_result_t::OTEL_OK
}

/// Registers this exporter instance on a core runtime using the name `stdout`.
///
/// The core setter is resolved from the process so the loadable library does
/// not need a link-time dependency on the core shared library.
/// duration of the call.
///
/// # Safety
/// Both pointers must be non-null and reference initialized storage for the
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_exporter_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_stdout_exporter_t,
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
    register_with_setter(runtime, handle, set_exporter)
}

fn register_with_setter(
    runtime: *mut otel_runtime_t,
    handle: *const otel_stdout_exporter_t,
    set_exporter: otel_set_exporter_fn,
) -> otel_result_t {
    // SAFETY: the caller validated the runtime and handle, and the handle
    // embeds a core-ABI descriptor with this exporter's name.
    unsafe { set_exporter(runtime, ptr::addr_of!((*handle).exporter)) }
}

/// Releases an exporter instance created by `otel_stdout_exporter_new`.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_stdout_exporter_release(handle: *mut otel_stdout_exporter_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe {
        release_handle::<StdoutExporter>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>())
    };
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
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::metrics::v1::{Metric, ResourceMetrics, ScopeMetrics};
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use std::ffi::CStr;
    use std::mem::size_of;
    use std::ptr::NonNull;
    use std::slice;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cstr(bytes: &[u8]) -> &CStr {
        CStr::from_bytes_with_nul(bytes).unwrap()
    }

    fn empty_handle() -> otel_stdout_exporter_t {
        otel_stdout_exporter_t {
            exporter: otel_exporter_t {
                size: size_of::<otel_exporter_t>(),
                name: otel_str_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                send: otel_stdout_exporter_send,
                user_data: ptr::null_mut(),
            },
            inner: ptr::null_mut(),
        }
    }

    fn new_handle(print_url: bool) -> otel_stdout_exporter_t {
        let mut handle = empty_handle();
        let options = otel_stdout_exporter_options_t {
            size: size_of::<otel_stdout_exporter_options_t>(),
            print_url,
        };
        // SAFETY: `handle` is writable, aligned caller-owned storage and
        // `options` points to an initialized options struct.
        let status = unsafe { otel_stdout_exporter_new(&mut handle, &options) };
        assert_eq!(status, otel_result_t::OTEL_OK);
        handle
    }

    fn release(handle: &mut otel_stdout_exporter_t) {
        // SAFETY: `handle` was initialized by `otel_stdout_exporter_new`.
        unsafe { otel_stdout_exporter_release(handle) };
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

    fn send(
        handle: &otel_stdout_exporter_t,
        view: &otel_exporter_request_t,
        response: &mut otel_exporter_response_t,
    ) -> otel_result_t {
        // SAFETY: `view` and `response` are initialized and all borrowed
        // buffers stay alive for the call; `user_data` belongs to a live handle.
        unsafe { otel_stdout_exporter_send(view, response, handle.exporter.user_data) }
    }

    fn error_bytes(response: &otel_exporter_response_t) -> &[u8] {
        // SAFETY: the callback filled `error` with a static byte string whose
        // pointer and length stay valid for the test.
        unsafe { slice::from_raw_parts(response.error.ptr.cast::<u8>(), response.error.len) }
    }

    #[test]
    fn abi_version_is_one() {
        assert_eq!(otel_stdout_exporter_abi_version(), 1);
    }

    #[test]
    fn new_rejects_null_arguments() {
        let mut handle = empty_handle();
        let options = otel_stdout_exporter_options_t {
            size: size_of::<otel_stdout_exporter_options_t>(),
            print_url: false,
        };

        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_stdout_exporter_new(ptr::null_mut(), &options) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: null options are rejected before they are dereferenced.
            unsafe { otel_stdout_exporter_new(&mut handle, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_rejects_invalid_options_size() {
        let mut handle = empty_handle();
        let options = otel_stdout_exporter_options_t {
            size: 1,
            print_url: false,
        };

        // SAFETY: the handle is writable storage and the options struct is initialized.
        let status = unsafe { otel_stdout_exporter_new(&mut handle, &options) };

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_initializes_descriptor_and_release_clears_it() {
        let mut handle = new_handle(false);

        assert_eq!(handle.exporter.size, size_of::<otel_exporter_t>());
        // SAFETY: `name` points into the exporter's static `stdout\0` byte string.
        let name = unsafe {
            slice::from_raw_parts(
                handle.exporter.name.ptr.cast::<u8>(),
                handle.exporter.name.len,
            )
        };
        assert_eq!(name, b"stdout".as_slice());
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
        unsafe { otel_stdout_exporter_release(ptr::null_mut()) };
    }

    #[test]
    fn send_rejects_null_arguments() {
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"http://localhost/v1/traces\0"), b"body");
        let dangling = NonNull::<u8>::dangling().as_ptr().cast::<c_void>();

        assert_eq!(
            // SAFETY: a null request is rejected before it is dereferenced.
            unsafe { otel_stdout_exporter_send(ptr::null(), &mut response, dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null response is rejected before it is dereferenced.
            unsafe { otel_stdout_exporter_send(&view, ptr::null_mut(), dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null user_data is rejected before it is dereferenced.
            unsafe { otel_stdout_exporter_send(&view, &mut response, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_rejects_null_url_and_body() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let body = ExportTraceServiceRequest::default().encode_to_vec();
        let mut null_url = request_view(cstr(b"http://localhost/v1/traces\0"), &body);
        null_url.url = ptr::null();
        let mut null_body = request_view(cstr(b"http://localhost/v1/traces\0"), &body);
        null_body.body = ptr::null();

        for view in [&null_url, &null_body] {
            let status = send(&handle, view, &mut response);
            assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
            assert_eq!(error_bytes(&response), ERR_INVALID);
        }

        release(&mut handle);
    }

    #[test]
    fn send_rejects_unknown_url() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"http://localhost/v1/unknown\0"), b"body");

        let status = send(&handle, &view, &mut response);

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert_eq!(error_bytes(&response), ERR_INVALID);
        release(&mut handle);
    }

    #[test]
    fn send_rejects_malformed_protobuf() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let urls = [
            cstr(b"http://localhost/v1/traces\0"),
            cstr(b"http://localhost/v1/metrics\0"),
            cstr(b"http://localhost/v1/logs\0"),
        ];

        for url in urls {
            let view = request_view(url, b"\xff\xff\xff");
            let status = send(&handle, &view, &mut response);
            assert_eq!(status, otel_result_t::OTEL_ERR_EXPORT);
            assert_eq!(error_bytes(&response), ERR_EXPORT);
        }

        release(&mut handle);
    }

    #[test]
    fn send_accepts_empty_traces() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let body = ExportTraceServiceRequest::default().encode_to_vec();
        let view = request_view(cstr(b"http://localhost/v1/traces\0"), &body);

        let status = send(&handle, &view, &mut response);

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.error.len, 0);
        release(&mut handle);
    }

    #[test]
    fn send_accepts_empty_metrics() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let body = ExportMetricsServiceRequest::default().encode_to_vec();
        let view = request_view(cstr(b"http://localhost/v1/metrics\0"), &body);

        let status = send(&handle, &view, &mut response);

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.error.len, 0);
        release(&mut handle);
    }

    #[test]
    fn send_accepts_empty_logs() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let body = ExportLogsServiceRequest::default().encode_to_vec();
        let view = request_view(cstr(b"http://localhost/v1/logs\0"), &body);

        let status = send(&handle, &view, &mut response);

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.error.len, 0);
        release(&mut handle);
    }

    #[test]
    fn send_accepts_populated_batches() {
        let mut handle = new_handle(false);
        let mut response = otel_exporter_response_t::EMPTY;
        let traces = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span::default()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec();
        let metrics = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric::default()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec();
        let logs = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord::default()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec();

        for (url, body) in [
            (cstr(b"http://localhost/v1/traces\0"), traces.as_slice()),
            (cstr(b"http://localhost/v1/metrics\0"), metrics.as_slice()),
            (cstr(b"http://localhost/v1/logs\0"), logs.as_slice()),
        ] {
            let view = request_view(url, body);
            assert_eq!(send(&handle, &view, &mut response), otel_result_t::OTEL_OK);
            assert_eq!(response.status_code, 200);
        }

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
            unsafe { otel_stdout_exporter_register(ptr::null_mut(), &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_stdout_exporter_register(&mut runtime, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: the handle has no inner pointer, so no core symbol is called.
            unsafe { otel_stdout_exporter_register(&mut runtime, &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_prints_url_when_enabled() {
        let mut handle = new_handle(true);
        let mut response = otel_exporter_response_t::EMPTY;
        let body = ExportTraceServiceRequest::default().encode_to_vec();
        let view = request_view(cstr(b"http://localhost/v1/traces\0"), &body);

        let status = send(&handle, &view, &mut response);

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.error.len, 0);
        release(&mut handle);
    }

    #[test]
    fn register_passes_initialized_handle_to_setter() {
        static SETTER_CALLS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn setter(
            runtime: *mut otel_runtime_t,
            exporter: *const otel_exporter_t,
        ) -> otel_result_t {
            assert!(!runtime.is_null());
            assert!(!exporter.is_null());
            SETTER_CALLS.fetch_add(1, Ordering::SeqCst);
            otel_result_t::OTEL_OK
        }

        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };
        let mut handle = new_handle(false);
        SETTER_CALLS.store(0, Ordering::SeqCst);

        assert_eq!(
            register_with_setter(&mut runtime, &handle, setter),
            otel_result_t::OTEL_OK
        );
        assert_eq!(SETTER_CALLS.load(Ordering::SeqCst), 1);

        release(&mut handle);
    }
}
