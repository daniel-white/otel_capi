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
