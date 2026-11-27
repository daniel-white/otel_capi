//! Optional loadable HTTP transport for `otel-capi`.
//!
//! The core library only defines the callback contract. This library uses the
//! shared blocking HTTP client backed by the host TLS stack.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;

use otel_capi::abi::{
    find_core_symbol, init_handle, otel_result_t, otel_runtime_t, otel_set_transport_fn,
    otel_str_t, otel_transport_request_t, otel_transport_response_t, otel_transport_send_fn,
    otel_transport_t, release_handle,
};
use otel_capi_http_client::{Method, RequestOptions, build_request, new_agent, send};

static ERR_INVALID: &[u8] = b"HTTP transport received invalid arguments";
static ERR_EXPORT: &[u8] = b"HTTP transport request failed";
const TRANSPORT_NAME: &[u8] = b"http\0";

/// Caller-owned handle for an HTTP transport instance. Initialize with
/// `OTEL_HTTP_TRANSPORT_T_INIT` and release with `otel_http_transport_release`.
#[repr(C)]
pub struct otel_http_transport_t {
    /// Descriptor read by the core runtime. The transport owns its name.
    pub transport: otel_transport_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_http_transport_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_http_transport_options_t {
    /// Set to `sizeof(otel_http_transport_options_t)`.
    pub size: usize,
    /// Whether redirects are followed. Defaults to `true`.
    pub follow_redirects: bool,
    /// Client connection timeout in milliseconds; zero uses the request timeout.
    pub connect_timeout_ms: u64,
}

struct HttpTransport {
    agent: otel_capi_http_client::Agent,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_transport_send_fn = otel_http_transport_send;

/// Returns the ABI version implemented by this transport.
#[unsafe(no_mangle)]
pub extern "C" fn otel_http_transport_abi_version() -> u32 {
    1
}

/// Creates a transport instance from caller-owned options.
///
/// HTTPS uses the host's TLS implementation and trust store through
/// `native-tls`.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_transport_new(
    handle: *mut otel_http_transport_t,
    options: *const otel_http_transport_options_t,
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
                send: otel_http_transport_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_http_transport_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    let agent = new_agent(options.follow_redirects, options.connect_timeout_ms);
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    // SAFETY: The caller provides writable, aligned handle storage; the core helper owns the pointer transfer.
    let status = unsafe { init_handle(slot, HttpTransport { agent }) };
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

/// Implements `otel_transport_send_fn` with the shared HTTP client.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_transport_send(
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
    let transport = unsafe { &*(user_data.cast::<HttpTransport>()) };
    if view.method.is_null() || view.url.is_null() {
        return set_error(
            response,
            ERR_INVALID,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
        );
    }

    // SAFETY: the core library passes NUL-terminated strings.
    let method_bytes = unsafe { CStr::from_ptr(view.method) }.to_bytes();
    let method = match Method::from_bytes(method_bytes) {
        Ok(method) => method,
        Err(_) => {
            return set_error(
                response,
                ERR_INVALID,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
            );
        }
    };
    // SAFETY: The caller guarantees the pointer is valid and the declared length or NUL terminator is accurate.
    let url = match unsafe { CStr::from_ptr(view.url) }.to_str() {
        Ok(url) => url,
        Err(_) => {
            return set_error(
                response,
                ERR_INVALID,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT,
            );
        }
    };
    let request = match build_request(view, method, url, &RequestOptions::default()) {
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
    reply.write_into(response);
    otel_result_t::OTEL_OK
}

/// Registers this transport instance on a core runtime using the name `http`.
///
/// The core setter is resolved from the process so the loadable library does
/// not need a link-time dependency on the core shared library.
/// duration of the call.
///
/// # Safety
/// Both pointers must be non-null and reference initialized storage for the
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_transport_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_http_transport_t,
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

/// Releases a transport instance created by `otel_http_transport_new`.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_transport_release(handle: *mut otel_http_transport_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe { release_handle::<HttpTransport>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>()) };
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
