//! Caller-owned handles holding `Arc`s.
//!
//! A handle is a `#[repr(C)]` struct with a single opaque pointer field. Rust
//! writes into storage provided by C (stack, heap, arena, or a struct field);
//! it never allocates the handle itself. `*_retain` duplicates into another
//! caller-owned slot and `*_release` drops the `Arc` and nulls the slot.

use std::ffi::c_void;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, RwLock};

use opentelemetry::metrics::{
    Counter, Gauge, Histogram, ObservableCounter, ObservableGauge, UpDownCounter,
};
use opentelemetry_sdk::logs::SdkLogger;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, Span as SdkSpan};

use crate::abi::{handle_created, handle_destroyed, live_handle_count};
pub use crate::abi::otel_runtime_t;
use crate::error::{Error, Result, guard, otel_result_t};
use crate::items::ItemTransport;
use crate::transport::Transport;

pub(crate) type ResourceInner = opentelemetry_sdk::Resource;

/// Named transports and the in-memory item transport registered by the caller.
pub(crate) struct RuntimeInner {
    transports: RwLock<Vec<(String, Transport)>>,
    item_transport: RwLock<Option<ItemTransport>>,
}

impl RuntimeInner {
    pub(crate) fn new() -> Self {
        Self {
            transports: RwLock::new(Vec::new()),
            item_transport: RwLock::new(None),
        }
    }

    pub(crate) fn set_transport(&self, name: String, transport: Transport) -> Result<()> {
        let mut slots = self
            .transports
            .write()
            .map_err(|_| Error::internal("transport registry lock was poisoned"))?;
        if let Some(slot) = slots.iter_mut().find(|(key, _)| key == &name) {
            slot.1 = transport;
        } else {
            slots.push((name, transport));
        }
        Ok(())
    }

    pub(crate) fn transport(&self, name: &str) -> Result<Option<Transport>> {
        let slots = self
            .transports
            .read()
            .map_err(|_| Error::internal("transport registry lock was poisoned"))?;
        Ok(slots
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, transport)| *transport))
    }

    pub(crate) fn set_item_transport(&self, transport: ItemTransport) -> Result<()> {
        let mut slot = self.item_transport.write().map_err(|_| {
            Error::internal("item transport lock was poisoned")
        })?;
        *slot = Some(transport);
        Ok(())
    }

    pub(crate) fn item_transport(&self) -> Result<Option<ItemTransport>> {
        let slot = self.item_transport.read().map_err(|_| {
            Error::internal("item transport lock was poisoned")
        })?;
        Ok(*slot)
    }
}

pub(crate) type ContextInner = opentelemetry::Context;
/// Thread-local scope token; the guard itself lives in `context.rs`.
pub(crate) type ContextScopeInner = u64;

pub(crate) struct TracerProviderInner {
    pub(crate) provider: SdkTracerProvider,
    #[allow(dead_code)] // Retained to keep the async runtime alive.
    pub(crate) runtime: Option<Arc<RuntimeInner>>,
}

pub(crate) struct MeterProviderInner {
    pub(crate) provider: SdkMeterProvider,
    #[allow(dead_code)] // Retained to keep the async runtime alive.
    pub(crate) runtime: Option<Arc<RuntimeInner>>,
}

pub(crate) struct LoggerProviderInner {
    pub(crate) provider: opentelemetry_sdk::logs::SdkLoggerProvider,
    #[allow(dead_code)] // Retained to keep the async runtime alive.
    pub(crate) runtime: Option<Arc<RuntimeInner>>,
}
pub(crate) type TracerInner = SdkTracer;
pub(crate) type MeterInner = opentelemetry::metrics::Meter;
pub(crate) type LoggerInner = SdkLogger;
/// Tracks a started span plus whether its last handle release ends it.
pub(crate) struct SpanInner {
    span: Mutex<Option<SdkSpan>>,
    end_on_release: bool,
}

impl SpanInner {
    pub(crate) fn new(span: SdkSpan, end_on_release: bool) -> Self {
        Self {
            span: Mutex::new(Some(span)),
            end_on_release,
        }
    }

    pub(crate) fn lock(
        &self,
    ) -> std::result::Result<SpanGuard<'_>, std::sync::PoisonError<MutexGuard<'_, Option<SdkSpan>>>>
    {
        self.span.lock().map(SpanGuard)
    }

    pub(crate) fn end_on_release(&self) -> bool {
        self.end_on_release
    }
}

pub(crate) struct SpanGuard<'a>(MutexGuard<'a, Option<SdkSpan>>);

impl Deref for SpanGuard<'_> {
    type Target = Option<SdkSpan>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SpanGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub(crate) type CounterU64Inner = Counter<u64>;
pub(crate) type CounterF64Inner = Counter<f64>;
pub(crate) type UpDownCounterI64Inner = UpDownCounter<i64>;
pub(crate) type HistogramF64Inner = Histogram<f64>;
pub(crate) type GaugeF64Inner = Gauge<f64>;
pub(crate) type ObservableGaugeF64Inner = ObservableGauge<f64>;
pub(crate) type ObservableCounterU64Inner = ObservableCounter<u64>;

/// A resource describing the entity producing telemetry.
#[repr(C)]
pub struct otel_resource_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Immutable propagation context carrying trace context and baggage.
#[repr(C)]
pub struct otel_context_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Scope token for a context attached to the current thread.
#[repr(C)]
pub struct otel_context_scope_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create tracers and control trace export.
#[repr(C)]
pub struct otel_tracer_provider_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create meters and control metric export.
#[repr(C)]
pub struct otel_meter_provider_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create loggers and control log export.
#[repr(C)]
pub struct otel_logger_provider_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for starting spans.
#[repr(C)]
pub struct otel_tracer_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for creating metric instruments.
#[repr(C)]
pub struct otel_meter_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for emitting log records.
#[repr(C)]
pub struct otel_logger_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// A started span. Ending is idempotent across retained handles.
#[repr(C)]
pub struct otel_span_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Async gauge that observes independent f64 values from a C callback.
#[repr(C)]
pub struct otel_observable_gauge_f64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Async counter that observes increasing u64 values from a C callback.
#[repr(C)]
pub struct otel_observable_counter_u64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Unsigned 64-bit monotonic counter instrument.
#[repr(C)]
pub struct otel_counter_u64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision monotonic counter instrument.
#[repr(C)]
pub struct otel_counter_f64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Signed 64-bit up/down counter instrument.
#[repr(C)]
pub struct otel_updown_counter_i64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision histogram instrument.
#[repr(C)]
pub struct otel_histogram_f64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision gauge instrument.
#[repr(C)]
pub struct otel_gauge_f64_t {
    /// @private Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Pointer to the opaque pointer field of a caller-provided handle.
#[inline]
pub(crate) fn slot<T>(handle: *mut T) -> *mut *mut c_void {
    handle.cast::<*mut c_void>()
}

#[inline]
pub(crate) fn const_slot<T>(handle: *const T) -> *const *mut c_void {
    handle.cast::<*mut c_void>()
}

/// Returns whether a handle points to a live object without taking a reference.
#[inline]
pub(crate) unsafe fn valid<T: Send + Sync + 'static>(src: *const *mut c_void) -> bool {
    if src.is_null() {
        return false;
    }
    // SAFETY: caller guarantees `src` points to an initialized handle.
    unsafe { !ptr::read(src).is_null() }
}

pub(crate) unsafe fn init<T: Send + Sync + 'static>(out: *mut *mut c_void, value: T) -> Result<()> {
    if out.is_null() {
        return Err(Error::invalid("out must not be NULL"));
    }
    // SAFETY: caller guarantees `out` points to writable, aligned storage.
    unsafe {
        ptr::write(
            out,
            Arc::into_raw(Arc::new(value)).cast_mut().cast::<c_void>(),
        )
    };
    handle_created();
    Ok(())
}

pub(crate) unsafe fn arc<T: Send + Sync + 'static>(src: *const *mut c_void) -> Result<Arc<T>> {
    if src.is_null() {
        return Err(Error::invalid("handle must not be NULL"));
    }
    // SAFETY: caller guarantees `src` points to an initialized handle.
    let raw = unsafe { ptr::read(src) };
    if raw.is_null() {
        return Err(Error::invalid("handle is empty"));
    }
    let raw = raw.cast::<T>();
    // SAFETY: `raw` came from `Arc::into_raw::<T>`.
    unsafe {
        Arc::increment_strong_count(raw);
        Ok(Arc::from_raw(raw))
    }
}

pub(crate) unsafe fn retain<T: Send + Sync + 'static>(
    src: *const *mut c_void,
    out: *mut *mut c_void,
) -> Result<()> {
    if out.is_null() {
        return Err(Error::invalid("out must not be NULL"));
    }
    let value: Arc<T> = unsafe { arc::<T>(src)? };
    // SAFETY: caller guarantees `out` points to writable, aligned storage.
    unsafe { ptr::write(out, Arc::into_raw(value).cast_mut().cast::<c_void>()) };
    handle_created();
    Ok(())
}

pub(crate) unsafe fn release<T: Send + Sync + 'static>(handle: *mut *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller guarantees `handle` points to an initialized handle.
    let raw = unsafe { ptr::read(handle) };
    // SAFETY: nulling the slot makes double-release a no-op.
    unsafe { ptr::write(handle, ptr::null_mut()) };
    if !raw.is_null() {
        handle_destroyed();
        // SAFETY: `raw` came from `Arc::into_raw::<T>` and we consumed one count.
        unsafe { drop(Arc::from_raw(raw.cast::<T>())) };
    }
}

/// Releases a span handle, ending the span first when it was started with
/// `end_on_release` and this is the last live handle.
pub(crate) unsafe fn release_span(handle: *mut *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller guarantees `handle` points to an initialized handle.
    let raw = unsafe { ptr::read(handle) };
    // SAFETY: nulling the slot makes double-release a no-op.
    unsafe { ptr::write(handle, ptr::null_mut()) };
    if raw.is_null() {
        return;
    }
    // SAFETY: `raw` came from `Arc::into_raw::<SpanInner>`.
    let inner = unsafe { Arc::from_raw(raw.cast::<SpanInner>()) };
    if inner.end_on_release() && Arc::strong_count(&inner) == 1 {
        crate::trace::end_span_once(&inner);
    }
    handle_destroyed();
    drop(inner);
}

/// Clones the resource behind an optional handle, or returns `None` for NULL.
pub(crate) fn optional_resource(handle: *const otel_resource_t) -> Result<Option<ResourceInner>> {
    if handle.is_null() {
        return Ok(None);
    }
    let arc = unsafe { arc::<ResourceInner>(const_slot(handle)) }?;
    Ok(Some((*arc).clone()))
}

/// Clones the runtime behind an optional handle, or returns `None` for NULL.
pub(crate) fn optional_runtime(handle: *const otel_runtime_t) -> Result<Option<Arc<RuntimeInner>>> {
    if handle.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { arc::<RuntimeInner>(const_slot(handle)) }?))
}

/// Returns the number of live handles created by this library.
#[unsafe(no_mangle)]
pub extern "C" fn otel_live_handle_count() -> usize {
    live_handle_count()
}

/// Returns `OTEL_OK` when every handle created by this library has been
/// released. Otherwise it reports `OTEL_ERR_LEAK` with the live-handle count
/// in the thread-local error message.
#[unsafe(no_mangle)]
pub extern "C" fn otel_ensure_all_released() -> otel_result_t {
    let live = live_handle_count();
    if live == 0 {
        otel_result_t::OTEL_OK
    } else {
        crate::error::set_error(
            otel_result_t::OTEL_ERR_LEAK,
            &format!("{live} handle(s) are still live"),
        )
    }
}

/// Retains a `otel_resource_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_retain(
    src: *const otel_resource_t,
    handle: *mut otel_resource_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<ResourceInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_resource_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_release(handle: *mut otel_resource_t) {
    unsafe { release::<ResourceInner>(slot(handle)) }
}

/// Retains a `otel_runtime_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_retain(
    src: *const otel_runtime_t,
    handle: *mut otel_runtime_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<RuntimeInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_runtime_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_release(handle: *mut otel_runtime_t) {
    unsafe { release::<RuntimeInner>(slot(handle)) }
}

/// Retains a `otel_tracer_provider_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_retain(
    src: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<TracerProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_tracer_provider_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_release(handle: *mut otel_tracer_provider_t) {
    unsafe { release::<TracerProviderInner>(slot(handle)) }
}

/// Retains a `otel_meter_provider_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_retain(
    src: *const otel_meter_provider_t,
    handle: *mut otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<MeterProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_meter_provider_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_release(handle: *mut otel_meter_provider_t) {
    unsafe { release::<MeterProviderInner>(slot(handle)) }
}

/// Retains a `otel_logger_provider_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_retain(
    src: *const otel_logger_provider_t,
    handle: *mut otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<LoggerProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_logger_provider_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_release(handle: *mut otel_logger_provider_t) {
    unsafe { release::<LoggerProviderInner>(slot(handle)) }
}

/// Retains a `otel_tracer_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_retain(
    src: *const otel_tracer_t,
    handle: *mut otel_tracer_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<TracerInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_tracer_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_release(handle: *mut otel_tracer_t) {
    unsafe { release::<TracerInner>(slot(handle)) }
}

/// Retains a `otel_meter_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_retain(
    src: *const otel_meter_t,
    handle: *mut otel_meter_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<MeterInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_meter_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_release(handle: *mut otel_meter_t) {
    unsafe { release::<MeterInner>(slot(handle)) }
}

/// Retains a `otel_logger_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_retain(
    src: *const otel_logger_t,
    handle: *mut otel_logger_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<LoggerInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_logger_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_release(handle: *mut otel_logger_t) {
    unsafe { release::<LoggerInner>(slot(handle)) }
}

/// Retains a `otel_span_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_retain(
    src: *const otel_span_t,
    handle: *mut otel_span_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<SpanInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_span_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_release(handle: *mut otel_span_t) {
    unsafe { release_span(slot(handle)) }
}

/// Retains a `otel_counter_u64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_retain(
    src: *const otel_counter_u64_t,
    handle: *mut otel_counter_u64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<CounterU64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_counter_u64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_release(handle: *mut otel_counter_u64_t) {
    unsafe { release::<CounterU64Inner>(slot(handle)) }
}

/// Retains a `otel_counter_f64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_retain(
    src: *const otel_counter_f64_t,
    handle: *mut otel_counter_f64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<CounterF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_counter_f64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_release(handle: *mut otel_counter_f64_t) {
    unsafe { release::<CounterF64Inner>(slot(handle)) }
}

/// Retains a `otel_updown_counter_i64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_retain(
    src: *const otel_updown_counter_i64_t,
    handle: *mut otel_updown_counter_i64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<UpDownCounterI64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_updown_counter_i64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_release(handle: *mut otel_updown_counter_i64_t) {
    unsafe { release::<UpDownCounterI64Inner>(slot(handle)) }
}

/// Retains a `otel_histogram_f64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_retain(
    src: *const otel_histogram_f64_t,
    handle: *mut otel_histogram_f64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<HistogramF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_histogram_f64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_release(handle: *mut otel_histogram_f64_t) {
    unsafe { release::<HistogramF64Inner>(slot(handle)) }
}

/// Retains a `otel_gauge_f64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_retain(
    src: *const otel_gauge_f64_t,
    handle: *mut otel_gauge_f64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<GaugeF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_gauge_f64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_release(handle: *mut otel_gauge_f64_t) {
    unsafe { release::<GaugeF64Inner>(slot(handle)) }
}

/// Retains a `otel_observable_gauge_f64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_retain(
    src: *const otel_observable_gauge_f64_t,
    handle: *mut otel_observable_gauge_f64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<ObservableGaugeF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to an `otel_observable_gauge_f64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_release(
    handle: *mut otel_observable_gauge_f64_t,
) {
    unsafe { release::<ObservableGaugeF64Inner>(slot(handle)) }
}

/// Retains a `otel_observable_counter_u64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_retain(
    src: *const otel_observable_counter_u64_t,
    handle: *mut otel_observable_counter_u64_t,
) -> otel_result_t {
    guard(|| {
        unsafe { retain::<ObservableCounterU64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to an `otel_observable_counter_u64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_release(
    handle: *mut otel_observable_counter_u64_t,
) {
    unsafe { release::<ObservableCounterU64Inner>(slot(handle)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct Dummy;

    impl Drop for Dummy {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn retain_release_manages_strong_count() {
        DROPS.store(0, Ordering::SeqCst);
        let mut a = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut b = otel_resource_t {
            inner: ptr::null_mut(),
        };

        unsafe {
            init(slot(&mut a), Dummy).unwrap();
            retain::<Dummy>(const_slot(&a), slot(&mut b)).unwrap();
            let borrowed: Arc<Dummy> = arc::<Dummy>(const_slot(&a)).unwrap();
            assert_eq!(Arc::strong_count(&borrowed), 3);
            drop(borrowed);

            release::<Dummy>(slot(&mut a));
            assert_eq!(DROPS.load(Ordering::SeqCst), 0);
            assert!(a.inner.is_null());

            release::<Dummy>(slot(&mut b));
            assert_eq!(DROPS.load(Ordering::SeqCst), 1);
            assert!(b.inner.is_null());

            release::<Dummy>(slot(&mut b));
            assert_eq!(DROPS.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn init_rejects_null_out() {
        let err = unsafe { init(ptr::null_mut(), 1u8) }.unwrap_err();
        assert_eq!(err.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
    }
}

/// Returns whether the resource handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_is_valid(handle: *const otel_resource_t) -> bool {
    unsafe { valid::<ResourceInner>(const_slot(handle)) }
}

/// Returns whether the context handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_is_valid(handle: *const otel_context_t) -> bool {
    unsafe { valid::<ContextInner>(const_slot(handle)) }
}

/// Returns whether the context scope handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_scope_is_valid(handle: *const otel_context_scope_t) -> bool {
    unsafe { valid::<ContextScopeInner>(const_slot(handle)) }
}

/// Returns whether the tracer provider handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_is_valid(
    handle: *const otel_tracer_provider_t,
) -> bool {
    unsafe { valid::<TracerProviderInner>(const_slot(handle)) }
}

/// Returns whether the meter provider handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_is_valid(
    handle: *const otel_meter_provider_t,
) -> bool {
    unsafe { valid::<MeterProviderInner>(const_slot(handle)) }
}

/// Returns whether the logger provider handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_is_valid(
    handle: *const otel_logger_provider_t,
) -> bool {
    unsafe { valid::<LoggerProviderInner>(const_slot(handle)) }
}

/// Returns whether the tracer handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_is_valid(handle: *const otel_tracer_t) -> bool {
    unsafe { valid::<TracerInner>(const_slot(handle)) }
}

/// Returns whether the meter handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_is_valid(handle: *const otel_meter_t) -> bool {
    unsafe { valid::<MeterInner>(const_slot(handle)) }
}

/// Returns whether the logger handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_is_valid(handle: *const otel_logger_t) -> bool {
    unsafe { valid::<LoggerInner>(const_slot(handle)) }
}

/// Returns whether the span handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_is_valid(handle: *const otel_span_t) -> bool {
    unsafe { valid::<SpanInner>(const_slot(handle)) }
}

/// Returns whether the unsigned counter handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_is_valid(handle: *const otel_counter_u64_t) -> bool {
    unsafe { valid::<CounterU64Inner>(const_slot(handle)) }
}

/// Returns whether the double counter handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_is_valid(handle: *const otel_counter_f64_t) -> bool {
    unsafe { valid::<CounterF64Inner>(const_slot(handle)) }
}

/// Returns whether the signed up/down counter handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_is_valid(
    handle: *const otel_updown_counter_i64_t,
) -> bool {
    unsafe { valid::<UpDownCounterI64Inner>(const_slot(handle)) }
}

/// Returns whether the histogram handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_is_valid(handle: *const otel_histogram_f64_t) -> bool {
    unsafe { valid::<HistogramF64Inner>(const_slot(handle)) }
}

/// Returns whether the gauge handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_is_valid(handle: *const otel_gauge_f64_t) -> bool {
    unsafe { valid::<GaugeF64Inner>(const_slot(handle)) }
}

/// Returns whether the observable gauge handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_is_valid(
    handle: *const otel_observable_gauge_f64_t,
) -> bool {
    unsafe { valid::<ObservableGaugeF64Inner>(const_slot(handle)) }
}

/// Returns whether the observable counter handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_is_valid(
    handle: *const otel_observable_counter_u64_t,
) -> bool {
    unsafe { valid::<ObservableCounterU64Inner>(const_slot(handle)) }
}
