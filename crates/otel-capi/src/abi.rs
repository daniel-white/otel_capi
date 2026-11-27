//! Shared C ABI and allocation logic for the core library and loadable transports.
//!
//! This module depends only on `std` and `libc`. Transport crates depend on
//! `otel-capi` with the `abi` feature enabled so they use the exact same layouts
//! and allocation accounting without importing the SDK or duplicating any
//! `otel_*` definitions.

use std::ffi::{c_char, c_void};
use std::mem::transmute_copy;
use std::ptr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Status returned by every fallible `otel-capi` function.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_result_t {
    OTEL_OK = 0,
    OTEL_ERR_INVALID_ARGUMENT = 1,
    OTEL_ERR_INVALID_UTF8 = 2,
    OTEL_ERR_CONFIG = 3,
    OTEL_ERR_EXPORT = 4,
    OTEL_ERR_SHUTDOWN = 5,
    OTEL_ERR_INTERNAL = 6,
    OTEL_ERR_PANIC = 7,
    /// Handles are still live at an explicit cleanup check.
    OTEL_ERR_LEAK = 8,
}

/// A borrowed UTF-8 byte range. Unlike `const char *`, this does not require a
/// NUL terminator and can therefore represent embedded bytes safely.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_str_t {
    pub ptr: *const c_char,
    pub len: usize,
}

/// Explicit transport handle created by the C caller. Initialize it with
/// `{0}` before passing it to `otel_runtime_new`.
#[repr(C)]
pub struct otel_runtime_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Descriptor embedded in each transport handle. The core runtime reads the
/// transport's name, callback, and state from this struct instead of taking a
/// protocol name as a separate argument.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_transport_t {
    /// Set to `sizeof(otel_transport_t)`.
    pub size: usize,
    /// Name owned by the transport crate.
    pub name: otel_str_t,
    /// Callback implementing `otel_transport_send_fn`.
    pub send: otel_transport_send_fn,
    /// Opaque state owned by the transport.
    pub user_data: *mut c_void,
}

/// One header view passed to a transport callback.
///
/// Both fields are borrowed and valid only for the duration of the callback.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_transport_header_t {
    pub name: otel_str_t,
    pub value: otel_str_t,
}

/// A request passed to a transport callback.
///
/// Every pointer is borrowed and valid only for the duration of the callback.
/// `timeout_ms` is configured with `OTEL_EXPORTER_OTLP_*_TIMEOUT`; the transport
/// is responsible for honoring it.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_transport_request_t {
    pub method: *const c_char,
    pub url: *const c_char,
    pub headers: *const otel_transport_header_t,
    pub header_count: usize,
    pub body: *const u8,
    pub body_len: usize,
    pub timeout_ms: u64,
}

/// A response filled in by a transport callback.
///
/// On `OTEL_OK`, set `status_code` and optionally `body` and `headers`. The
/// core library copies all response data before the callback returns. On
/// failure, return a non-`OTEL_OK` status and optionally set `error`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_transport_response_t {
    pub status_code: u16,
    pub body: *const u8,
    pub body_len: usize,
    pub headers: *const otel_transport_header_t,
    pub header_count: usize,
    pub error: otel_str_t,
}

impl otel_transport_response_t {
    /// A zeroed response, ready for a transport callback to fill in.
    pub const EMPTY: Self = Self {
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

/// Sends one encoded OTLP request.
///
/// The callback may be invoked concurrently from OpenTelemetry background
/// threads, so it and `user_data` must be safe to use concurrently. It must not
/// retain any pointer from `request`; response pointers are copied before the
/// callback returns. Non-success HTTP statuses are valid responses.
pub type otel_transport_send_fn = unsafe extern "C" fn(
    request: *const otel_transport_request_t,
    response: *mut otel_transport_response_t,
    user_data: *mut c_void,
) -> otel_result_t;

/// Registers a transport descriptor on the core runtime.
pub type otel_set_transport_fn = unsafe extern "C" fn(
    runtime: *mut otel_runtime_t,
    transport: *const otel_transport_t,
) -> otel_result_t;

type NotifyFn = unsafe extern "C" fn();

struct Tracker {
    created: NotifyFn,
    destroyed: NotifyFn,
}

static LIVE_HANDLES: AtomicUsize = AtomicUsize::new(0);
static TRACKER: OnceLock<Option<Tracker>> = OnceLock::new();

/// Records one allocated caller-owned handle.
pub fn handle_created() {
    LIVE_HANDLES.fetch_add(1, Ordering::SeqCst);
}

/// Records one released caller-owned handle.
pub fn handle_destroyed() {
    LIVE_HANDLES.fetch_sub(1, Ordering::SeqCst);
}

/// Number of caller-owned handles currently live.
pub fn live_handle_count() -> usize {
    LIVE_HANDLES.load(Ordering::SeqCst)
}

#[cfg(unix)]
fn lookup_tracker() -> Option<Tracker> {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        let created = libc::dlsym(libc::RTLD_DEFAULT, c"otel_handle_created".as_ptr());
        let destroyed = libc::dlsym(libc::RTLD_DEFAULT, c"otel_handle_destroyed".as_ptr());
        if created.is_null() || destroyed.is_null() {
            return None;
        }
        Some(Tracker {
            created: transmute_copy(&created),
            destroyed: transmute_copy(&destroyed),
        })
    }
}

#[cfg(not(unix))]
fn lookup_tracker() -> Option<Tracker> {
    None
}

#[cfg(unix)]
fn lookup_symbol<T: Copy>(name: &[u8]) -> Option<T> {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast());
        (!symbol.is_null()).then(|| transmute_copy(&symbol))
    }
}

#[cfg(not(unix))]
fn lookup_symbol<T: Copy>(_name: &[u8]) -> Option<T> {
    None
}

/// Finds an exported core function from a loadable transport.
pub fn find_core_symbol<T: Copy>(name: &[u8]) -> Option<T> {
    lookup_symbol(name)
}

fn notify_created() {
    if let Some(tracker) = TRACKER.get_or_init(lookup_tracker) {
        // SAFETY: the symbol was exported by `otel-capi` and has this ABI.
        unsafe { (tracker.created)() };
    } else {
        handle_created();
    }
}

fn notify_destroyed() {
    if let Some(tracker) = TRACKER.get_or_init(lookup_tracker) {
        // SAFETY: the symbol was exported by `otel-capi` and has this ABI.
        unsafe { (tracker.destroyed)() };
    } else {
        handle_destroyed();
    }
}

/// Allocates a handle into caller-owned storage and records it for cleanup checks.
///
/// # Safety
/// `slot` must point to writable, aligned storage for one pointer.
pub unsafe fn init_handle<T: Send + Sync + 'static>(slot: *mut c_void, value: T) -> otel_result_t {
    if slot.is_null() {
        return otel_result_t::OTEL_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the caller guarantees writable, aligned storage.
    unsafe {
        ptr::write(slot.cast::<*mut c_void>(), ptr::null_mut());
        ptr::write(
            slot.cast::<*mut c_void>(),
            Arc::into_raw(Arc::new(value)).cast_mut().cast::<c_void>(),
        );
    }
    notify_created();
    otel_result_t::OTEL_OK
}

/// Releases a handle allocated by `init_handle` and records the cleanup.
///
/// # Safety
/// `slot` must point to an initialized handle from `init_handle`.
pub unsafe fn release_handle<T: Send + Sync + 'static>(slot: *mut c_void) {
    if slot.is_null() {
        return;
    }
    let slot = slot.cast::<*mut c_void>();
    // SAFETY: the caller supplies an initialized handle.
    let raw = unsafe { ptr::read(slot) };
    // SAFETY: nulling the slot makes double-release a no-op.
    unsafe { ptr::write(slot, ptr::null_mut()) };
    if raw.is_null() {
        return;
    }
    // SAFETY: `raw` came from `init_handle::<T>`.
    unsafe { drop(Arc::from_raw(raw.cast::<T>())) };
    notify_destroyed();
}
