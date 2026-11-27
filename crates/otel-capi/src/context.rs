//! Propagation context injection, extraction, and current-context scopes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::Context;
use opentelemetry::Key;
use opentelemetry::KeyValue;
use opentelemetry::baggage::{Baggage, BaggageExt};
use opentelemetry::context::ContextGuard;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};

use crate::error::{Error, c_str, guard, otel_result_t, utf8};
use crate::handles::{
    ContextInner, ContextScopeInner, arc, const_slot, init, otel_context_scope_t, otel_context_t,
    otel_span_t, release, slot,
};
use crate::propagation::{
    buffer_utf8, copy_into_buffer, optional_buffer_utf8, otel_baggage_t, otel_baggage_value_t,
    otel_traceparent_t, otel_tracestate_t,
};
use crate::strings::{otel_str_t, owned_c_string};

thread_local! {
    /// Guards must be dropped on the thread that attached them.
    static ATTACHED: RefCell<HashMap<u64, ContextGuard>> = RefCell::new(HashMap::new());
}

static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

fn propagator() -> &'static TextMapCompositePropagator {
    static PROPAGATOR: OnceLock<TextMapCompositePropagator> = OnceLock::new();
    PROPAGATOR.get_or_init(|| {
        TextMapCompositePropagator::new(vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ])
    })
}

/// Creates an empty context in caller-owned storage.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_new(handle: *mut otel_context_t) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), Context::new()) }?;
        Ok(())
    })
}

/// Copies the context attached to the calling thread.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_current(handle: *mut otel_context_t) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), Context::current()) }?;
        Ok(())
    })
}

/// Builds a context whose active span is a clone of `span`.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_from_span(
    span: *const otel_span_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let span_context = unsafe { crate::trace::span_context(span)? };
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe {
            init(
                slot(handle),
                Context::new().with_remote_span_context(span_context),
            )
        }?;
        Ok(())
    })
}

/// Retains a context into caller-owned storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_retain(
    src: *const otel_context_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    guard(|| unsafe { crate::handles::retain::<ContextInner>(const_slot(src), slot(handle)) })
}

/// Releases one reference to a context.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_release(handle: *mut otel_context_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<ContextInner>(slot(handle)) };
}

/// Extracts W3C `traceparent`, `tracestate`, and `baggage` headers from typed
/// buffers. NULL headers are ignored. The result is independent of the current
/// context.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_extract(
    traceparent: *const otel_traceparent_t,
    tracestate: *const otel_tracestate_t,
    baggage: *const otel_baggage_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        let mut carrier = HashMap::new();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        if let Some(value) = unsafe { optional_buffer_utf8(traceparent)? } {
            carrier.insert("traceparent".to_string(), value);
        }
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        if let Some(value) = unsafe { optional_buffer_utf8(tracestate)? } {
            carrier.insert("tracestate".to_string(), value);
        }
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        if let Some(value) = unsafe { optional_buffer_utf8(baggage)? } {
            carrier.insert("baggage".to_string(), value);
        }
        let context = propagator().extract_with_context(&Context::new(), &carrier);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), context) }?;
        Ok(())
    })
}

/// Injects W3C headers from `context` into typed buffers. Pass NULL for a
/// header you do not want.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_inject(
    context: *const otel_context_t,
    traceparent: *mut otel_traceparent_t,
    tracestate: *mut otel_tracestate_t,
    baggage: *mut otel_baggage_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let mut carrier = HashMap::new();
        propagator().inject_context(&context, &mut carrier);
        if !traceparent.is_null()
            && let Some(value) = carrier.get("traceparent")
        {
            // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
            unsafe { copy_into_buffer(value, traceparent) }?;
        }
        if !tracestate.is_null()
            && let Some(value) = carrier.get("tracestate")
        {
            // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
            unsafe { copy_into_buffer(value, tracestate) }?;
        }
        if !baggage.is_null()
            && let Some(value) = carrier.get("baggage")
        {
            // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
            unsafe { copy_into_buffer(value, baggage) }?;
        }
        Ok(())
    })
}

/// Returns a copy of `context` with one baggage member added or replaced.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_set_baggage(
    context: *const otel_context_t,
    key: *const c_char,
    value: *const otel_baggage_value_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let key = utf8(unsafe { c_str(key, "baggage key")? }, "baggage key")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let value = unsafe { buffer_utf8(value)? };
        let mut pairs: Vec<KeyValue> = context
            .baggage()
            .iter()
            .map(|(key, (value, _))| KeyValue::new(key.clone(), value.clone()))
            .collect();
        pairs.retain(|pair| pair.key.as_str() != key);
        pairs.push(KeyValue::new(Key::new(key), value));
        let baggage: Baggage = pairs.into_iter().collect();
        let next = context.with_baggage(baggage);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), next) }?;
        Ok(())
    })
}

/// Copies a baggage value into typed storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_get_baggage(
    context: *const otel_context_t,
    key: *const c_char,
    value: *mut otel_baggage_value_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let key = utf8(unsafe { c_str(key, "baggage key")? }, "baggage key")?;
        let baggage_value = context
            .baggage()
            .get(&key)
            .ok_or_else(|| Error::invalid("baggage key is not present"))?;
        // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
        unsafe { copy_into_buffer(baggage_value.as_ref(), value) }
    })
}

/// Length-aware variant of [`otel_context_set_baggage`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_set_baggage_n(
    context: *const otel_context_t,
    key: otel_str_t,
    value: *const otel_baggage_value_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
    let key = unsafe { owned_c_string(key, "baggage key") }
        .and_then(|key| key.ok_or_else(|| Error::invalid("baggage key must not be NULL")));
    match key {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        Ok(key) => unsafe { otel_context_set_baggage(context, key.as_ptr(), value, handle) },
        Err(err) => err.report(),
    }
}

/// Length-aware variant of [`otel_context_get_baggage`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_get_baggage_n(
    context: *const otel_context_t,
    key: otel_str_t,
    value: *mut otel_baggage_value_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
    let key = unsafe { owned_c_string(key, "baggage key") }
        .and_then(|key| key.ok_or_else(|| Error::invalid("baggage key must not be NULL")));
    match key {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        Ok(key) => unsafe { otel_context_get_baggage(context, key.as_ptr(), value) },
        Err(err) => err.report(),
    }
}

/// Attaches `context` to the calling thread until `otel_context_detach`.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_attach(
    context: *const otel_context_t,
    handle: *mut otel_context_scope_t,
) -> otel_result_t {
    guard(|| {
        if handle.is_null() {
            return Err(Error::invalid("scope handle must not be NULL"));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let token = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), token) }?;
        ATTACHED.with(|attached| {
            attached
                .borrow_mut()
                .insert(token, context.as_ref().clone().attach());
        });
        Ok(())
    })
}

/// Detaches the context scope from the thread that attached it.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_detach(handle: *mut otel_context_scope_t) -> otel_result_t {
    guard(|| {
        if handle.is_null() {
            return Err(Error::invalid("scope handle must not be NULL"));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let token = unsafe { arc::<ContextScopeInner>(const_slot(handle)) }?;
        ATTACHED.with(|attached| {
            attached.borrow_mut().remove(&*token);
        });
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { release::<ContextScopeInner>(slot(handle)) };
        Ok(())
    })
}

/// Detaches the scope if it is still attached, then releases the handle.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_scope_release(handle: *mut otel_context_scope_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    let _ = unsafe { otel_context_detach(handle) };
}
