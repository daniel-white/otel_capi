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

use crate::error::{Error, Result, c_str, guard, otel_result_t, utf8};
use crate::handles::{
    ContextInner, ContextScopeInner, arc, const_slot, init, otel_context_scope_t,
    otel_context_t, otel_span_t, release, slot,
};
use crate::util::{copy_to_buffer, optional_str};

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

fn copy_optional(value: &str, buf: *mut c_char, capacity: usize) -> Result<()> {
    if buf.is_null() && capacity == 0 {
        return Ok(());
    }
    copy_to_buffer(value, buf, capacity)
}

/// Creates an empty context in caller-owned storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_new(handle: *mut otel_context_t) -> otel_result_t {
    guard(|| {
        unsafe { init(slot(handle), Context::new()) }?;
        Ok(())
    })
}

/// Copies the context attached to the calling thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_current(handle: *mut otel_context_t) -> otel_result_t {
    guard(|| {
        unsafe { init(slot(handle), Context::current()) }?;
        Ok(())
    })
}

/// Builds a context whose active span is a clone of `span`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_from_span(
    span: *const otel_span_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        let span_context = unsafe { crate::trace::span_context(span)? };
        unsafe { init(slot(handle), Context::new().with_remote_span_context(span_context)) }?;
        Ok(())
    })
}

/// Retains a context into caller-owned storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_retain(
    src: *const otel_context_t,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        unsafe { crate::handles::retain::<ContextInner>(const_slot(src), slot(handle)) }
    })
}

/// Releases one reference to a context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_release(handle: *mut otel_context_t) {
    unsafe { release::<ContextInner>(slot(handle)) };
}

/// Extracts W3C `traceparent`, `tracestate`, and `baggage` headers. NULL
/// headers are ignored. The result is independent of the current context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_extract(
    traceparent: *const c_char,
    tracestate: *const c_char,
    baggage: *const c_char,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        let mut carrier = HashMap::new();
        for (name, value) in [
            ("traceparent", traceparent),
            ("tracestate", tracestate),
            ("baggage", baggage),
        ] {
            if let Some(value) = unsafe { optional_str(value, name)? } {
                carrier.insert(name.to_string(), value.to_string());
            }
        }
        let context = propagator().extract_with_context(&Context::new(), &carrier);
        unsafe { init(slot(handle), context) }?;
        Ok(())
    })
}

/// Injects W3C headers from `context` into caller-provided buffers. Pass NULL
/// with capacity zero for headers you do not want.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_inject(
    context: *const otel_context_t,
    traceparent: *mut c_char,
    traceparent_capacity: usize,
    tracestate: *mut c_char,
    tracestate_capacity: usize,
    baggage: *mut c_char,
    baggage_capacity: usize,
) -> otel_result_t {
    guard(|| {
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let mut carrier = HashMap::new();
        propagator().inject_context(&context, &mut carrier);
        for (key, buf, capacity) in [
            ("traceparent", traceparent, traceparent_capacity),
            ("tracestate", tracestate, tracestate_capacity),
            ("baggage", baggage, baggage_capacity),
        ] {
            if let Some(value) = carrier.get(key) {
                copy_optional(value, buf, capacity)?;
            }
        }
        Ok(())
    })
}

/// Returns a copy of `context` with one baggage member added or replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_set_baggage(
    context: *const otel_context_t,
    key: *const c_char,
    value: *const c_char,
    handle: *mut otel_context_t,
) -> otel_result_t {
    guard(|| {
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let key = utf8(unsafe { c_str(key, "baggage key")? }, "baggage key")?;
        let value = utf8(unsafe { c_str(value, "baggage value")? }, "baggage value")?;
        let mut pairs: Vec<KeyValue> = context
            .baggage()
            .iter()
            .map(|(key, (value, _))| KeyValue::new(key.clone(), value.clone()))
            .collect();
        pairs.retain(|pair| pair.key.as_str() != key);
        pairs.push(KeyValue::new(Key::new(key), value));
        let baggage: Baggage = pairs.into_iter().collect();
        let next = context.with_baggage(baggage);
        unsafe { init(slot(handle), next) }?;
        Ok(())
    })
}

/// Copies a baggage value into `buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_get_baggage(
    context: *const otel_context_t,
    key: *const c_char,
    buf: *mut c_char,
    capacity: usize,
) -> otel_result_t {
    guard(|| {
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let key = utf8(unsafe { c_str(key, "baggage key")? }, "baggage key")?;
        let value = context
            .baggage()
            .get(&key)
            .ok_or_else(|| Error::invalid("baggage key is not present"))?;
        copy_to_buffer(value.as_ref(), buf, capacity)
    })
}

/// Attaches `context` to the calling thread until `otel_context_detach`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_attach(
    context: *const otel_context_t,
    handle: *mut otel_context_scope_t,
) -> otel_result_t {
    guard(|| {
        if handle.is_null() {
            return Err(Error::invalid("scope handle must not be NULL"));
        }
        let context = unsafe { arc::<ContextInner>(const_slot(context)) }?;
        let token = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
        unsafe { init(slot(handle), token) }?;
        ATTACHED.with(|attached| {
            attached.borrow_mut().insert(token, context.as_ref().clone().attach());
        });
        Ok(())
    })
}

/// Detaches the context scope from the thread that attached it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_detach(handle: *mut otel_context_scope_t) -> otel_result_t {
    guard(|| {
        if handle.is_null() {
            return Err(Error::invalid("scope handle must not be NULL"));
        }
        let token = unsafe { arc::<ContextScopeInner>(const_slot(handle)) }?;
        ATTACHED.with(|attached| {
            attached.borrow_mut().remove(&*token);
        });
        unsafe { release::<ContextScopeInner>(slot(handle)) };
        Ok(())
    })
}

/// Detaches the scope if it is still attached, then releases the handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_scope_release(handle: *mut otel_context_scope_t) {
    let _ = unsafe { otel_context_detach(handle) };
}
