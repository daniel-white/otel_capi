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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::{SpanInner, otel_span_release};
    use crate::propagation::{
        otel_baggage_from_str, otel_baggage_str, otel_baggage_value_from_str,
        otel_baggage_value_str, otel_traceparent_from_str, otel_traceparent_str,
        otel_tracestate_from_str, otel_tracestate_str,
    };
    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
    use opentelemetry_sdk::trace::{SdkTracerProvider, Span as SdkSpan};
    use std::ffi::{CStr, c_char};
    use std::ptr;

    fn empty_context() -> otel_context_t {
        otel_context_t {
            inner: ptr::null_mut(),
        }
    }

    fn empty_scope() -> otel_context_scope_t {
        otel_context_scope_t {
            inner: ptr::null_mut(),
        }
    }

    fn baggage_value(text: &CStr) -> otel_baggage_value_t {
        let mut value = otel_baggage_value_t { value: [0; 257] };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_baggage_value_from_str(text.as_ptr(), &mut value) },
            otel_result_t::OTEL_OK
        );
        value
    }

    fn text(ptr: *const c_char) -> String {
        assert!(!ptr.is_null());
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned()
    }

    #[test]
    fn new_current_retain_and_release() {
        let mut source = empty_context();
        let mut retained = empty_context();
        let mut current = empty_context();

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_retain(ptr::null(), &mut retained) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut source) },
            otel_result_t::OTEL_OK
        );
        assert!(!source.inner.is_null());
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_retain(&source, &mut retained) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(source.inner, retained.inner);
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_retain(&source, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_current(&mut current) },
            otel_result_t::OTEL_OK
        );
        assert!(!current.inner.is_null());

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe {
            otel_context_release(&mut source);
            otel_context_release(&mut retained);
            otel_context_release(&mut current);
        }
        assert!(source.inner.is_null());
        assert!(retained.inner.is_null());
        assert!(current.inner.is_null());
    }

    #[test]
    fn baggage_set_get_replace_and_length_aware_variants() {
        let mut base = empty_context();
        let one = baggage_value(c"one");
        let two = baggage_value(c"two");
        let mut first = empty_context();
        let mut replaced = empty_context();
        let mut via_view = empty_context();
        let mut output = otel_baggage_value_t { value: [0; 257] };

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut base) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage(&base, c"key".as_ptr(), &one, &mut first) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage(&first, c"key".as_ptr(), &mut output) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_value_str(&output) }),
            "one"
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage(&first, c"key".as_ptr(), &two, &mut replaced) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage(&replaced, c"key".as_ptr(), &mut output) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_value_str(&output) }),
            "two"
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage(&replaced, c"absent".as_ptr(), &mut output) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let key = otel_str_t {
            ptr: c"n-key".as_ptr(),
            len: 5,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage_n(&base, key, &one, &mut via_view) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage_n(&via_view, key, &mut output) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_value_str(&output) }),
            "one"
        );

        let null_key = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage_n(&base, null_key, &one, &mut first) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage_n(&via_view, null_key, &mut output) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe {
            otel_context_release(&mut base);
            otel_context_release(&mut first);
            otel_context_release(&mut replaced);
            otel_context_release(&mut via_view);
        }
    }

    #[test]
    fn attach_and_detach_change_current_context() {
        let mut base = empty_context();
        let mut tagged = empty_context();
        let mut scope = empty_scope();
        let value = baggage_value(c"attached");
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut base) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage(&base, c"key".as_ptr(), &value, &mut tagged) },
            otel_result_t::OTEL_OK
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_attach(ptr::null(), &mut scope) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_attach(&tagged, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_attach(&tagged, &mut scope) },
            otel_result_t::OTEL_OK
        );
        assert!(!scope.inner.is_null());

        let mut current = empty_context();
        let mut output = otel_baggage_value_t { value: [0; 257] };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_current(&mut current) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage(&current, c"key".as_ptr(), &mut output) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_value_str(&output) }),
            "attached"
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut current) };

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_detach(ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_detach(&mut scope) },
            otel_result_t::OTEL_OK
        );
        assert!(scope.inner.is_null());
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_detach(&mut scope) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut after = empty_context();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_current(&mut after) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_get_baggage(&after, c"key".as_ptr(), &mut output) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut after) };

        // Scope release detaches an attached scope before releasing the handle.
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_attach(&tagged, &mut scope) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_scope_release(&mut scope) };
        assert!(scope.inner.is_null());
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_scope_release(&mut scope) };

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe {
            otel_context_release(&mut base);
            otel_context_release(&mut tagged);
        }
    }

    #[test]
    fn extract_and_inject_w3c_headers() {
        const TRACEPARENT: &CStr = c"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut traceparent = otel_traceparent_t { value: [0; 56] };
        let mut tracestate = otel_tracestate_t { value: [0; 513] };
        let mut baggage = otel_baggage_t { value: [0; 1025] };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_traceparent_from_str(TRACEPARENT.as_ptr(), &mut traceparent) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_tracestate_from_str(c"vendor=value".as_ptr(), &mut tracestate) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_baggage_from_str(c"key=value".as_ptr(), &mut baggage) },
            otel_result_t::OTEL_OK
        );

        let mut context = empty_context();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_extract(&traceparent, &tracestate, &baggage, &mut context) },
            otel_result_t::OTEL_OK
        );

        let mut tp_out = otel_traceparent_t { value: [0; 56] };
        let mut ts_out = otel_tracestate_t { value: [0; 513] };
        let mut bag_out = otel_baggage_t { value: [0; 1025] };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_inject(&context, &mut tp_out, &mut ts_out, &mut bag_out) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_traceparent_str(&tp_out) }),
            TRACEPARENT.to_str().unwrap()
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_tracestate_str(&ts_out) }),
            "vendor=value"
        );
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_str(&bag_out) }).contains("key=value")
        );

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe {
                otel_context_inject(&context, ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
            },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe {
                otel_context_inject(ptr::null(), &mut tp_out, ptr::null_mut(), ptr::null_mut())
            },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let mut empty = empty_context();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_extract(ptr::null(), ptr::null(), ptr::null(), &mut empty) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut empty) };

        let invalid = otel_traceparent_t { value: [0; 56] };
        let mut unused = empty_context();
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_extract(&invalid, ptr::null(), ptr::null(), &mut unused) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(unused.inner.is_null());
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe {
                otel_context_extract(&traceparent, ptr::null(), ptr::null(), ptr::null_mut())
            },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut context) };
    }

    #[test]
    fn current_rejects_null_output() {
        assert_eq!(
            // SAFETY: NULL output is rejected before it is dereferenced.
            unsafe { otel_context_current(ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn from_span_copies_span_context_and_validates_arguments() {
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("context-tests");
        let sdk_span: SdkSpan = tracer.start("parent");
        let expected = sdk_span.span_context().clone();
        let mut span = otel_span_t {
            inner: ptr::null_mut(),
        };
        let mut context = empty_context();
        let mut traceparent = otel_traceparent_t { value: [0; 56] };

        assert!(
            // SAFETY: `span` is writable handle storage and the SDK span owns its state.
            unsafe { init(slot(&mut span), SpanInner::new(sdk_span, false)) }.is_ok()
        );
        assert_eq!(
            // SAFETY: a NULL span is rejected before it is dereferenced.
            unsafe { otel_context_from_span(ptr::null(), &mut context) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: `span` is initialized, while NULL output is rejected by `init`.
            unsafe { otel_context_from_span(&span, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: `span` and `context` are initialized handle slots.
            unsafe { otel_context_from_span(&span, &mut context) },
            otel_result_t::OTEL_OK
        );
        assert!(!context.inner.is_null());

        assert_eq!(
            // SAFETY: `context` is initialized and `traceparent` is writable typed storage.
            unsafe {
                otel_context_inject(&context, &mut traceparent, ptr::null_mut(), ptr::null_mut())
            },
            otel_result_t::OTEL_OK
        );
        let injected = text(
            // SAFETY: `traceparent` was filled above and stays alive for this call.
            unsafe { otel_traceparent_str(&traceparent) },
        );
        assert!(injected.contains(&expected.trace_id().to_string()));
        assert!(injected.contains(&expected.span_id().to_string()));

        assert_eq!(
            // SAFETY: `span` is initialized, while NULL output is rejected by `init`.
            unsafe { otel_context_from_span(&span, ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        // SAFETY: both handles were initialized above and are released exactly once.
        unsafe {
            otel_span_release(&mut span);
            otel_context_release(&mut context);
        }
    }

    #[test]
    fn inject_baggage_only_context_and_reject_null_source() {
        let mut base = empty_context();
        let mut tagged = empty_context();
        let value = baggage_value(c"injected");
        let mut traceparent = otel_traceparent_t { value: [0; 56] };
        let mut baggage = otel_baggage_t { value: [0; 1025] };

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut base) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_set_baggage(&base, c"key".as_ptr(), &value, &mut tagged) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: `tagged` is initialized and both destinations stay alive for the call.
            unsafe {
                otel_context_inject(&tagged, &mut traceparent, ptr::null_mut(), &mut baggage)
            },
            otel_result_t::OTEL_OK
        );
        assert!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_baggage_str(&baggage) }).contains("key=injected")
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            text(unsafe { otel_traceparent_str(&traceparent) }),
            ""
        );
        assert_eq!(
            // SAFETY: a NULL source handle is rejected before it is dereferenced.
            unsafe {
                otel_context_inject(ptr::null(), &mut traceparent, ptr::null_mut(), &mut baggage)
            },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe {
            otel_context_release(&mut base);
            otel_context_release(&mut tagged);
        }
    }

    #[test]
    fn baggage_length_aware_variants_report_invalid_key_and_null_value() {
        let invalid_bytes = [0xffu8];
        let invalid_key = otel_str_t {
            ptr: invalid_bytes.as_ptr().cast(),
            len: invalid_bytes.len(),
        };
        let valid_key = otel_str_t {
            ptr: c"key".as_ptr(),
            len: 3,
        };
        let nul_bytes = b"a\0b";
        let nul_key = otel_str_t {
            ptr: nul_bytes.as_ptr().cast(),
            len: nul_bytes.len(),
        };
        let value = baggage_value(c"v");
        let mut base = empty_context();
        let mut out = empty_context();
        let mut baggage_out = otel_baggage_value_t { value: [0; 257] };

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut base) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: the view describes one readable invalid UTF-8 byte.
            unsafe { otel_context_set_baggage_n(&base, invalid_key, &value, &mut out) },
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
        assert_eq!(
            // SAFETY: the view describes one readable invalid UTF-8 byte.
            unsafe { otel_context_get_baggage_n(&base, invalid_key, &mut baggage_out) },
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
        assert_eq!(
            // SAFETY: the view describes readable UTF-8 bytes containing an embedded NUL.
            unsafe { otel_context_set_baggage_n(&base, nul_key, &value, &mut out) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: the view describes readable UTF-8 bytes containing an embedded NUL.
            unsafe { otel_context_get_baggage_n(&base, nul_key, &mut baggage_out) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(out.inner.is_null());
        assert_eq!(
            // SAFETY: NULL baggage value is rejected before it is dereferenced.
            unsafe { otel_context_set_baggage_n(&base, valid_key, ptr::null(), &mut out) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut base) };
    }

    #[test]
    fn scope_release_accepts_null_and_prevents_double_detach() {
        let mut base = empty_context();
        let mut scope = empty_scope();

        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_new(&mut base) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: a NULL scope handle is documented as a no-op.
        unsafe { otel_context_scope_release(ptr::null_mut()) };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_context_attach(&base, &mut scope) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: the scope is initialized and attached on this thread.
        unsafe { otel_context_scope_release(&mut scope) };
        assert!(scope.inner.is_null());
        assert_eq!(
            // SAFETY: the scope was released and the empty handle is rejected.
            unsafe { otel_context_detach(&mut scope) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        // SAFETY: releasing an already empty scope is a no-op.
        unsafe { otel_context_scope_release(&mut scope) };

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_context_release(&mut base) };
    }
}
