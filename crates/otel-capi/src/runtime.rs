//! Explicit transport handle created by the C caller.
//!
//! The core library does not own an async runtime. C code creates this handle
//! once with `otel_runtime_new`, registers the transports its exporters need,
//! and passes it to provider constructors. Providers retain their own
//! reference, so the C handle may be released at any time.

use std::mem::size_of;
use std::ptr;

use crate::abi::otel_transport_t;
use crate::error::{Error, guard, otel_result_t};
use crate::handles::{RuntimeInner, arc, const_slot, init, otel_runtime_t, slot, valid};
use crate::items::{ItemTransport, otel_item_transport_t};
use crate::strings::str_slice;
use crate::transport::Transport;

/// Creates a transport handle for OTLP exporters.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_new(handle: *mut otel_runtime_t) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), RuntimeInner::new()) }?;
        Ok(())
    })
}

/// Creates a transport handle with a caller-selected worker-thread count.
///
/// The core library no longer creates runtime threads; the argument is kept for
/// source compatibility. Implementations that need workers (such as the
/// optional gRPC plugin) expose their own thread-count configuration. Writes
/// into caller-owned `handle`; initialize it with `{0}` before calling.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_new_with_threads(
    handle: *mut otel_runtime_t,
    thread_count: u32,
) -> otel_result_t {
    let _ = thread_count;
    guard(|| {
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), RuntimeInner::new()) }?;
        Ok(())
    })
}

/// Returns whether the runtime handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_is_valid(runtime: *const otel_runtime_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(runtime)) }
}

/// Registers a transport descriptor used by OTLP exporters.
///
/// The descriptor is normally embedded in a transport handle created by one of
/// the loadable transport libraries, so the core never has to be told the
/// transport's name separately. Applications providing their own callback can
/// fill an `otel_transport_t` with `OTEL_TRANSPORT_INIT`. Call this before
/// creating providers. The callback may run concurrently on OpenTelemetry
/// background threads, so it and `user_data` must be thread-safe. `user_data`
/// is borrowed until the runtime is released; no pointers from the request are
/// retained after the callback returns.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_set_transport(
    runtime: *mut otel_runtime_t,
    transport: *const otel_transport_t,
) -> otel_result_t {
    guard(|| {
        if transport.is_null() {
            return Err(Error::invalid("transport descriptor must not be NULL"));
        }
        // SAFETY: the caller supplies a stable, aligned descriptor for this call.
        let value = unsafe { ptr::read(transport) };
        if value.size != size_of::<otel_transport_t>() {
            return Err(Error::invalid("unexpected otel_transport_t size"));
        }
        if value.send as usize == 0 {
            return Err(Error::invalid("transport callback must not be NULL"));
        }
        if value.name.ptr.is_null() || value.name.len == 0 {
            return Err(Error::invalid("transport name must not be empty"));
        }
        // SAFETY: the descriptor supplies a valid borrowed view for this call.
        let name = unsafe { str_slice(value.name, "transport name") }?;
        if name.is_empty() {
            return Err(Error::invalid("transport name must not be empty"));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let runtime = unsafe { arc::<RuntimeInner>(slot(runtime)) }?;
        runtime.set_transport(name.to_owned(), Transport::new(value.send, value.user_data))?;
        Ok(())
    })
}

/// Registers in-memory item callbacks used by the `item` exporter.
///
/// Call this before creating providers. Typed callbacks are invoked once per
/// span, log record, or metric stream; the encoded callback is invoked once per
/// OTLP batch. Callbacks may run concurrently on OpenTelemetry background
/// threads, so they and `user_data` must be thread-safe. The transport struct
/// is copied; `user_data` is borrowed until the runtime is released.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_set_item_transport(
    runtime: *mut otel_runtime_t,
    transport: *const otel_item_transport_t,
) -> otel_result_t {
    guard(|| {
        if transport.is_null() {
            return Err(Error::invalid("item transport must not be NULL"));
        }
        // SAFETY: the caller supplies a stable, aligned struct for this call.
        let value = unsafe { *transport };
        value.validate()?;
        if value.on_span.is_none()
            && value.on_log.is_none()
            && value.on_metric.is_none()
            && value.on_encoded.is_none()
        {
            return Err(Error::invalid(
                "item transport must register at least one callback",
            ));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let runtime = unsafe { arc::<RuntimeInner>(slot(runtime)) }?;
        runtime.set_item_transport(ItemTransport::from(value))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{otel_transport_request_t, otel_transport_response_t, otel_transport_send_fn};
    use crate::handles::otel_runtime_release;
    use crate::items::otel_encoded_item_t;
    use crate::strings::otel_str_t;
    use std::ffi::{c_char, c_void};

    fn empty_runtime() -> otel_runtime_t {
        otel_runtime_t {
            inner: ptr::null_mut(),
        }
    }

    fn named(bytes: &[u8]) -> otel_str_t {
        otel_str_t {
            ptr: bytes.as_ptr().cast::<c_char>(),
            len: bytes.len(),
        }
    }

    unsafe extern "C" fn test_send(
        _request: *const otel_transport_request_t,
        _response: *mut otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn test_encoded(_item: *const otel_encoded_item_t, _user_data: *mut c_void) {}

    fn descriptor(name: otel_str_t, send: otel_transport_send_fn) -> otel_transport_t {
        otel_transport_t {
            size: size_of::<otel_transport_t>(),
            name,
            send,
            user_data: ptr::null_mut(),
        }
    }

    #[test]
    fn runtime_new_and_valid_tracks_handle_lifetime() {
        let mut runtime = empty_runtime();
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            !unsafe { otel_runtime_is_valid(ptr::null()) }
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new(ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );
        assert!(!runtime.inner.is_null());
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_is_valid(&runtime) }
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };
        assert!(runtime.inner.is_null());
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            !unsafe { otel_runtime_is_valid(&runtime) }
        );
    }

    #[test]
    fn runtime_new_with_threads_still_creates_a_handle_without_workers() {
        let mut runtime = empty_runtime();
        // The thread count argument is retained for source compatibility only;
        // this implementation never starts a worker thread.
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new_with_threads(&mut runtime, 0) },
            otel_result_t::OTEL_OK
        );
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_is_valid(&runtime) }
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };

        let mut runtime = empty_runtime();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new_with_threads(&mut runtime, 4) },
            otel_result_t::OTEL_OK
        );
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_is_valid(&runtime) }
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn set_transport_rejects_bad_descriptors() {
        let mut runtime = empty_runtime();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );

        let valid = descriptor(named(b"test"), test_send);
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(ptr::null_mut(), &valid) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut bad = valid;
        bad.size = 0;
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut bad = valid;
        bad.name = named(b"");
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut bad = valid;
        bad.name = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let invalid = [0xffu8];
        let mut bad = valid;
        bad.name = named(&invalid);
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn set_transport_registers_callback_and_user_data() {
        let mut runtime = empty_runtime();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );

        let mut user_data = 7u32;
        let mut transport = descriptor(named(b"test"), test_send);
        transport.user_data = (&mut user_data as *mut u32).cast::<c_void>();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &transport) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let registered = unsafe { arc::<RuntimeInner>(const_slot(&runtime)) }.unwrap();
        let found = registered.transport("test").unwrap().unwrap();
        assert_eq!(found.user_data, transport.user_data);
        let request = otel_transport_request_t {
            method: ptr::null(),
            url: ptr::null(),
            headers: ptr::null(),
            header_count: 0,
            body: ptr::null(),
            body_len: 0,
            timeout_ms: 0,
        };
        let mut response = otel_transport_response_t::EMPTY;
        assert_eq!(
            // SAFETY: the local views are initialized and the registered callback
            // only returns a status without dereferencing them.
            unsafe { (found.send)(&request, &mut response, found.user_data) },
            otel_result_t::OTEL_OK
        );
        drop(registered);

        // Registering the same name again replaces the previous callback.
        let mut replacement = descriptor(named(b"test"), test_send);
        replacement.user_data = ptr::null_mut();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_transport(&mut runtime, &replacement) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let registered = unsafe { arc::<RuntimeInner>(const_slot(&runtime)) }.unwrap();
        assert!(
            registered
                .transport("test")
                .unwrap()
                .unwrap()
                .user_data
                .is_null()
        );
        drop(registered);

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn set_item_transport_rejects_bad_descriptors_and_registers_callback() {
        let mut runtime = empty_runtime();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );

        let valid = otel_item_transport_t {
            size: size_of::<otel_item_transport_t>(),
            on_span: None,
            on_log: None,
            on_metric: None,
            on_encoded: Some(test_encoded),
            user_data: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_item_transport(ptr::null_mut(), &valid) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_item_transport(&mut runtime, ptr::null()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut bad = valid;
        bad.size = 0;
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_item_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut bad = valid;
        bad.on_encoded = None;
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_item_transport(&mut runtime, &bad) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_runtime_set_item_transport(&mut runtime, &valid) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let registered = unsafe { arc::<RuntimeInner>(const_slot(&runtime)) }.unwrap();
        let found = registered.item_transport().unwrap().unwrap();
        assert!(found.on_encoded.is_some());
        assert!(found.on_span.is_none());
        drop(registered);

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_runtime_release(&mut runtime) };
    }
}
