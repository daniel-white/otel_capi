//! Optional loadable HTTP exporter for `otel-capi`.
//!
//! The core library only defines the callback contract. This library uses the
//! shared blocking HTTP client backed by the host TLS stack.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr;

use otel_capi::abi::{
    find_core_symbol, init_handle, otel_exporter_request_t, otel_exporter_response_t,
    otel_exporter_send_fn, otel_exporter_t, otel_result_t, otel_runtime_t, otel_set_exporter_fn,
    otel_str_t, release_handle,
};
use otel_capi_http_transport::{Method, RequestOptions, build_request, new_agent, send};

static ERR_INVALID: &[u8] = b"HTTP exporter received invalid arguments";
static ERR_EXPORT: &[u8] = b"HTTP exporter request failed";
const EXPORTER_NAME: &[u8] = b"http\0";

/// Caller-owned handle for an HTTP exporter instance. Initialize with
/// `OTEL_HTTP_EXPORTER_T_INIT` and release with `otel_http_exporter_release`.
#[repr(C)]
pub struct otel_http_exporter_t {
    /// Descriptor read by the core runtime. The exporter owns its name.
    pub exporter: otel_exporter_t,
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Options accepted by `otel_http_exporter_new`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_http_exporter_options_t {
    /// Set to `sizeof(otel_http_exporter_options_t)`.
    pub size: usize,
    /// Whether redirects are followed. Defaults to `true`.
    pub follow_redirects: bool,
    /// Client connection timeout in milliseconds; zero uses the request timeout.
    pub connect_timeout_ms: u64,
}

struct HttpExporter {
    agent: otel_capi_http_transport::Agent,
}

// Compile-time proof that the exported callback matches the core ABI.
const _: otel_exporter_send_fn = otel_http_exporter_send;

/// Returns the ABI version implemented by this exporter.
#[unsafe(no_mangle)]
pub extern "C" fn otel_http_exporter_abi_version() -> u32 {
    1
}

/// Creates an exporter instance from caller-owned options.
///
/// HTTPS uses the host's TLS implementation and trust store through
/// `native-tls`.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_exporter_new(
    handle: *mut otel_http_exporter_t,
    options: *const otel_http_exporter_options_t,
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
                send: otel_http_exporter_send,
                user_data: ptr::null_mut(),
            },
        );
    }
    // SAFETY: the caller supplies an initialized options struct for this call.
    let options = unsafe { *options };
    if options.size != size_of::<otel_http_exporter_options_t>() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }

    let agent = new_agent(options.follow_redirects, options.connect_timeout_ms);
    // SAFETY: the caller supplies writable, aligned caller-owned storage.
    let slot = unsafe { ptr::addr_of_mut!((*handle).inner).cast::<c_void>() };
    // SAFETY: The caller provides writable, aligned handle storage; the core helper owns the pointer transfer.
    let status = unsafe { init_handle(slot, HttpExporter { agent }) };
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

/// Implements `otel_exporter_send_fn` with the shared HTTP client.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_exporter_send(
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
    let exporter = unsafe { &*(user_data.cast::<HttpExporter>()) };
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
    let reply = match send(&exporter.agent, request, view.timeout_ms) {
        Ok(reply) => reply,
        Err(_) => return set_error(response, ERR_EXPORT, otel_result_t::OTEL_ERR_EXPORT),
    };
    reply.write_into(response);
    otel_result_t::OTEL_OK
}

/// Registers this exporter instance on a core runtime using the name `http`.
///
/// The core setter is resolved from the process so the loadable library does
/// not need a link-time dependency on the core shared library.
/// duration of the call.
///
/// # Safety
/// Both pointers must be non-null and reference initialized storage for the
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_exporter_register(
    runtime: *mut otel_runtime_t,
    handle: *const otel_http_exporter_t,
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
    handle: *const otel_http_exporter_t,
    set_exporter: otel_set_exporter_fn,
) -> otel_result_t {
    // SAFETY: the caller validated the runtime and handle, and the handle
    // embeds a core-ABI descriptor with this exporter's name.
    unsafe { set_exporter(runtime, ptr::addr_of!((*handle).exporter)) }
}

/// Releases an exporter instance created by `otel_http_exporter_new`.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_http_exporter_release(handle: *mut otel_http_exporter_t) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller supplies an initialized handle from `init_handle`.
    unsafe { release_handle::<HttpExporter>(ptr::addr_of_mut!((*handle).inner).cast::<c_void>()) };
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
    use otel_capi_http_transport::ureq;
    use std::ffi::{CStr, CString};
    use std::mem::size_of;
    use std::ptr::NonNull;
    use std::slice;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use ureq::config::Config;
    use ureq::http::Uri;
    use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
    };

    fn cstr(bytes: &[u8]) -> &CStr {
        CStr::from_bytes_with_nul(bytes).unwrap()
    }

    fn empty_handle() -> otel_http_exporter_t {
        otel_http_exporter_t {
            exporter: otel_exporter_t {
                size: size_of::<otel_exporter_t>(),
                name: otel_str_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                send: otel_http_exporter_send,
                user_data: ptr::null_mut(),
            },
            inner: ptr::null_mut(),
        }
    }

    fn new_handle() -> otel_http_exporter_t {
        let mut handle = empty_handle();
        let options = otel_http_exporter_options_t {
            size: size_of::<otel_http_exporter_options_t>(),
            follow_redirects: false,
            connect_timeout_ms: 0,
        };
        // SAFETY: `handle` is writable, aligned caller-owned storage and
        // `options` points to an initialized options struct.
        let status = unsafe { otel_http_exporter_new(&mut handle, &options) };
        assert_eq!(status, otel_result_t::OTEL_OK);
        handle
    }

    fn release(handle: &mut otel_http_exporter_t) {
        // SAFETY: `handle` was initialized by `otel_http_exporter_new`.
        unsafe { otel_http_exporter_release(handle) };
    }

    fn request_view(method: &CStr, url: &CStr) -> otel_exporter_request_t {
        otel_exporter_request_t {
            method: method.as_ptr(),
            url: url.as_ptr(),
            headers: ptr::null(),
            header_count: 0,
            body: b"body".as_ptr(),
            body_len: 4,
            timeout_ms: 60_000,
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

    fn mock_agent(reply: Vec<u8>, fail: bool) -> otel_capi_http_transport::Agent {
        let config = otel_capi_http_transport::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        otel_capi_http_transport::Agent::with_parts(
            config,
            MockConnector { reply, fail },
            MockResolver,
        )
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
    fn abi_version_is_one() {
        assert_eq!(otel_http_exporter_abi_version(), 1);
    }

    #[test]
    fn new_rejects_null_arguments() {
        let mut handle = empty_handle();
        let options = otel_http_exporter_options_t {
            size: size_of::<otel_http_exporter_options_t>(),
            follow_redirects: false,
            connect_timeout_ms: 0,
        };

        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_http_exporter_new(ptr::null_mut(), &options) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: null options are rejected before they are dereferenced.
            unsafe { otel_http_exporter_new(&mut handle, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_rejects_invalid_options_size() {
        let mut handle = empty_handle();
        let options = otel_http_exporter_options_t {
            size: 1,
            follow_redirects: false,
            connect_timeout_ms: 0,
        };

        // SAFETY: the handle is writable storage and the options struct is initialized.
        let status = unsafe { otel_http_exporter_new(&mut handle, &options) };

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert!(handle.inner.is_null());
    }

    #[test]
    fn new_initializes_descriptor_and_release_clears_it() {
        let mut handle = new_handle();

        assert_eq!(handle.exporter.size, size_of::<otel_exporter_t>());
        // SAFETY: `name` points into the exporter's static `http\0` byte string.
        let name = unsafe {
            slice::from_raw_parts(
                handle.exporter.name.ptr.cast::<u8>(),
                handle.exporter.name.len,
            )
        };
        assert_eq!(name, b"http".as_slice());
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
        unsafe { otel_http_exporter_release(ptr::null_mut()) };
    }

    #[test]
    fn send_rejects_null_arguments() {
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"POST\0"), cstr(b"http://localhost/v1/traces\0"));
        let dangling = NonNull::<u8>::dangling().as_ptr().cast::<c_void>();

        assert_eq!(
            // SAFETY: a null request is rejected before it is dereferenced.
            unsafe { otel_http_exporter_send(ptr::null(), &mut response, dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null response is rejected before it is dereferenced.
            unsafe { otel_http_exporter_send(&view, ptr::null_mut(), dangling) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null user_data is rejected before it is dereferenced.
            unsafe { otel_http_exporter_send(&view, &mut response, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_rejects_invalid_method_and_url() {
        let mut handle = new_handle();
        let mut response = otel_exporter_response_t::EMPTY;
        let url = cstr(b"http://localhost/v1/traces\0");
        let invalid_method = request_view(cstr(b"NOT A METHOD\0"), url);
        let bad_utf8 = CString::new([0xffu8, 0xfe]).unwrap();
        let invalid_url = request_view(cstr(b"POST\0"), &bad_utf8);
        let mut null_method = request_view(cstr(b"POST\0"), url);
        null_method.method = ptr::null();
        let mut null_url = request_view(cstr(b"POST\0"), url);
        null_url.url = ptr::null();

        for view in [&invalid_method, &invalid_url, &null_method, &null_url] {
            // SAFETY: all views are initialized, `response` is writable, and
            // `user_data` belongs to the live handle held for this test.
            let status =
                unsafe { otel_http_exporter_send(view, &mut response, handle.exporter.user_data) };
            assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
            assert_eq!(error_bytes(&response), ERR_INVALID);
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
            unsafe { otel_http_exporter_register(ptr::null_mut(), &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: a null handle is rejected before it is dereferenced.
            unsafe { otel_http_exporter_register(&mut runtime, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: the handle has no inner pointer, so no core symbol is called.
            unsafe { otel_http_exporter_register(&mut runtime, &handle) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn send_returns_http_response() {
        let exporter = HttpExporter {
            agent: mock_agent(
                http_reply(&[("content-type", "application/x-protobuf")], b"reply"),
                false,
            ),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"POST\0"), cstr(b"http://localhost:4318/v1/traces\0"));

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `HttpExporter`.
        let status = unsafe {
            otel_http_exporter_send(
                &view,
                &mut response,
                (&exporter as *const HttpExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_OK);
        assert_eq!(response.status_code, 200);
        assert_eq!(response.body_len, 5);
        assert_eq!(response.error.len, 0);
    }

    #[test]
    fn send_reports_build_request_failure() {
        let exporter = HttpExporter {
            agent: mock_agent(Vec::new(), false),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let url = cstr(b"http://localhost:4318/v1/traces\0");
        let mut view = request_view(cstr(b"POST\0"), url);
        view.body = ptr::null();

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `HttpExporter`.
        let status = unsafe {
            otel_http_exporter_send(
                &view,
                &mut response,
                (&exporter as *const HttpExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert_eq!(error_bytes(&response), ERR_INVALID);
    }

    #[test]
    fn send_reports_exporter_failure() {
        let exporter = HttpExporter {
            agent: mock_agent(Vec::new(), true),
        };
        let mut response = otel_exporter_response_t::EMPTY;
        let view = request_view(cstr(b"POST\0"), cstr(b"http://localhost:4318/v1/traces\0"));

        // SAFETY: `view` and `response` are initialized and stay alive for this
        // call; `user_data` points to a live `HttpExporter`.
        let status = unsafe {
            otel_http_exporter_send(
                &view,
                &mut response,
                (&exporter as *const HttpExporter)
                    .cast::<c_void>()
                    .cast_mut(),
            )
        };

        assert_eq!(status, otel_result_t::OTEL_ERR_EXPORT);
        assert_eq!(error_bytes(&response), ERR_EXPORT);
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
        let mut handle = new_handle();
        SETTER_CALLS.store(0, Ordering::SeqCst);

        assert_eq!(
            register_with_setter(&mut runtime, &handle, setter),
            otel_result_t::OTEL_OK
        );
        assert_eq!(SETTER_CALLS.load(Ordering::SeqCst), 1);

        release(&mut handle);
    }
}
