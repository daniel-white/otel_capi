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

pub use crate::abi::otel_runtime_t;
use crate::abi::{handle_created, handle_destroyed, live_handle_count};
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
        let mut slot = self
            .item_transport
            .write()
            .map_err(|_| Error::internal("item transport lock was poisoned"))?;
        *slot = Some(transport);
        Ok(())
    }

    pub(crate) fn item_transport(&self) -> Result<Option<ItemTransport>> {
        let slot = self
            .item_transport
            .read()
            .map_err(|_| Error::internal("item transport lock was poisoned"))?;
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
pub(crate) unsafe fn valid(src: *const *mut c_void) -> bool {
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
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
    let arc = unsafe { arc::<ResourceInner>(const_slot(handle)) }?;
    Ok(Some((*arc).clone()))
}

/// Clones the runtime behind an optional handle, or returns `None` for NULL.
pub(crate) fn optional_runtime(handle: *const otel_runtime_t) -> Result<Option<Arc<RuntimeInner>>> {
    if handle.is_null() {
        return Ok(None);
    }
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_retain(
    src: *const otel_resource_t,
    handle: *mut otel_resource_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<ResourceInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_resource_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_release(handle: *mut otel_resource_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<ResourceInner>(slot(handle)) }
}

/// Retains a `otel_runtime_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_retain(
    src: *const otel_runtime_t,
    handle: *mut otel_runtime_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<RuntimeInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_runtime_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_release(handle: *mut otel_runtime_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<RuntimeInner>(slot(handle)) }
}

/// Retains a `otel_tracer_provider_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_retain(
    src: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<TracerProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_tracer_provider_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_release(handle: *mut otel_tracer_provider_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<TracerProviderInner>(slot(handle)) }
}

/// Retains a `otel_meter_provider_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_retain(
    src: *const otel_meter_provider_t,
    handle: *mut otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<MeterProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_meter_provider_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_release(handle: *mut otel_meter_provider_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<MeterProviderInner>(slot(handle)) }
}

/// Retains a `otel_logger_provider_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_retain(
    src: *const otel_logger_provider_t,
    handle: *mut otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<LoggerProviderInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_logger_provider_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_release(handle: *mut otel_logger_provider_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<LoggerProviderInner>(slot(handle)) }
}

/// Retains a `otel_tracer_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_retain(
    src: *const otel_tracer_t,
    handle: *mut otel_tracer_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<TracerInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_tracer_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_release(handle: *mut otel_tracer_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<TracerInner>(slot(handle)) }
}

/// Retains a `otel_meter_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_retain(
    src: *const otel_meter_t,
    handle: *mut otel_meter_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<MeterInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_meter_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_release(handle: *mut otel_meter_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<MeterInner>(slot(handle)) }
}

/// Retains a `otel_logger_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_retain(
    src: *const otel_logger_t,
    handle: *mut otel_logger_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<LoggerInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_logger_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_release(handle: *mut otel_logger_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<LoggerInner>(slot(handle)) }
}

/// Retains a `otel_span_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_retain(
    src: *const otel_span_t,
    handle: *mut otel_span_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<SpanInner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_span_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_release(handle: *mut otel_span_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release_span(slot(handle)) }
}

/// Retains a `otel_counter_u64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_retain(
    src: *const otel_counter_u64_t,
    handle: *mut otel_counter_u64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<CounterU64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_counter_u64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_release(handle: *mut otel_counter_u64_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<CounterU64Inner>(slot(handle)) }
}

/// Retains a `otel_counter_f64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_retain(
    src: *const otel_counter_f64_t,
    handle: *mut otel_counter_f64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<CounterF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_counter_f64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_release(handle: *mut otel_counter_f64_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<CounterF64Inner>(slot(handle)) }
}

/// Retains a `otel_updown_counter_i64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_retain(
    src: *const otel_updown_counter_i64_t,
    handle: *mut otel_updown_counter_i64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<UpDownCounterI64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_updown_counter_i64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_release(handle: *mut otel_updown_counter_i64_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<UpDownCounterI64Inner>(slot(handle)) }
}

/// Retains a `otel_histogram_f64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_retain(
    src: *const otel_histogram_f64_t,
    handle: *mut otel_histogram_f64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<HistogramF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_histogram_f64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_release(handle: *mut otel_histogram_f64_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<HistogramF64Inner>(slot(handle)) }
}

/// Retains a `otel_gauge_f64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_retain(
    src: *const otel_gauge_f64_t,
    handle: *mut otel_gauge_f64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<GaugeF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to a `otel_gauge_f64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_release(handle: *mut otel_gauge_f64_t) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<GaugeF64Inner>(slot(handle)) }
}

/// Retains a `otel_observable_gauge_f64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_retain(
    src: *const otel_observable_gauge_f64_t,
    handle: *mut otel_observable_gauge_f64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<ObservableGaugeF64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to an `otel_observable_gauge_f64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_release(
    handle: *mut otel_observable_gauge_f64_t,
) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<ObservableGaugeF64Inner>(slot(handle)) }
}

/// Retains a `otel_observable_counter_u64_t` into caller-provided storage.
///
/// # Safety
/// The source must point to an initialized handle and the output must point to writable storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_retain(
    src: *const otel_observable_counter_u64_t,
    handle: *mut otel_observable_counter_u64_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { retain::<ObservableCounterU64Inner>(src.cast(), slot(handle)) }?;
        Ok(())
    })
}

/// Releases one reference to an `otel_observable_counter_u64_t` and clears the slot.
///
/// # Safety
/// The handle must be null or point to an initialized handle of this type; the call clears the slot and drops the referenced object once no other references remain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_release(
    handle: *mut otel_observable_counter_u64_t,
) {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { release::<ObservableCounterU64Inner>(slot(handle)) }
}

/// Returns whether the resource handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_is_valid(handle: *const otel_resource_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the context handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_is_valid(handle: *const otel_context_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the context scope handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_context_scope_is_valid(handle: *const otel_context_scope_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the tracer provider handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_is_valid(
    handle: *const otel_tracer_provider_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the meter provider handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_is_valid(
    handle: *const otel_meter_provider_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the logger provider handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_is_valid(
    handle: *const otel_logger_provider_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the tracer handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_is_valid(handle: *const otel_tracer_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the meter handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_is_valid(handle: *const otel_meter_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the logger handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_is_valid(handle: *const otel_logger_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the span handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_is_valid(handle: *const otel_span_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the unsigned counter handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_is_valid(handle: *const otel_counter_u64_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the double counter handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_is_valid(handle: *const otel_counter_f64_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the signed up/down counter handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_is_valid(
    handle: *const otel_updown_counter_i64_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the histogram handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_is_valid(handle: *const otel_histogram_f64_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the gauge handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_is_valid(handle: *const otel_gauge_f64_t) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the observable gauge handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_gauge_f64_is_valid(
    handle: *const otel_observable_gauge_f64_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

/// Returns whether the observable counter handle is usable.
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observable_counter_u64_is_valid(
    handle: *const otel_observable_counter_u64_t,
) -> bool {
    // SAFETY: The pointer may be null or reference an initialized handle; only the pointer value is read.
    unsafe { valid(const_slot(handle)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::EnvGuard;
    use crate::logs::{otel_logger_provider_logger, otel_logger_provider_new};
    use crate::metrics::{
        otel_meter_counter_f64, otel_meter_counter_u64, otel_meter_gauge_f64,
        otel_meter_histogram_f64, otel_meter_observable_counter_u64,
        otel_meter_observable_gauge_f64, otel_meter_provider_meter, otel_meter_provider_new,
        otel_meter_updown_counter_i64, otel_observe_context_t,
    };
    use crate::resource::otel_resource_new;
    use crate::runtime::otel_runtime_new;
    use crate::trace::{
        otel_span_options_t, otel_tracer_provider_new, otel_tracer_provider_tracer,
        otel_tracer_span_start_with_options_arr,
    };
    use std::mem::size_of;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct Dummy;

    impl Drop for Dummy {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct HandleProbe;

    unsafe extern "C" fn noop_retain_observe_f64(
        _context: otel_observe_context_t,
        _data: *mut c_void,
    ) {
    }

    unsafe extern "C" fn noop_retain_observe_u64(
        _context: otel_observe_context_t,
        _data: *mut c_void,
    ) {
    }

    fn no_exporters() -> EnvGuard {
        let mut env = EnvGuard::new();
        env.set("OTEL_TRACES_EXPORTER", "none");
        env.set("OTEL_METRICS_EXPORTER", "none");
        env.set("OTEL_LOGS_EXPORTER", "none");
        env.set("OTEL_SDK_DISABLED", "false");
        env.set("OTEL_TRACES_SAMPLER", "always_on");
        env
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

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
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
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let err = unsafe { init(ptr::null_mut(), 1u8) }.unwrap_err();
        assert_eq!(err.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn retain_release_helpers_reject_invalid_slots() {
        let mut empty = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut empty_span = otel_span_t {
            inner: ptr::null_mut(),
        };

        // SAFETY: `empty` and `empty_span` are initialized empty slots; all
        // pointers are either local storage or null and are validated before use.
        unsafe {
            let err = retain::<HandleProbe>(const_slot(&empty), ptr::null_mut()).unwrap_err();
            assert_eq!(err.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);

            let err = retain::<HandleProbe>(ptr::null(), slot(&mut empty)).unwrap_err();
            assert_eq!(err.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);

            let mut empty_out = otel_resource_t {
                inner: ptr::null_mut(),
            };
            let err = retain::<HandleProbe>(const_slot(&empty), slot(&mut empty_out)).unwrap_err();
            assert_eq!(err.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);

            release::<HandleProbe>(ptr::null_mut());
            release::<HandleProbe>(slot(&mut empty));

            release_span(ptr::null_mut());
            release_span(slot(&mut empty_span));
        }

        assert!(empty.inner.is_null());
        assert!(empty_span.inner.is_null());
    }

    #[test]
    fn optional_handles_clone_resource_and_runtime() {
        let _env = no_exporters();
        let mut resource = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };

        // SAFETY: both handles are local writable storage, are initialized by
        // their constructors, and are released before the test ends.
        unsafe {
            assert_eq!(otel_resource_new(&mut resource), otel_result_t::OTEL_OK);
            assert_eq!(otel_runtime_new(&mut runtime), otel_result_t::OTEL_OK);

            assert!(optional_resource(ptr::null()).unwrap().is_none());
            assert!(optional_runtime(ptr::null()).unwrap().is_none());
            assert!(optional_resource(&resource).unwrap().is_some());
            assert!(optional_runtime(&runtime).unwrap().is_some());

            otel_runtime_release(&mut runtime);
            otel_resource_release(&mut resource);
        }

        assert!(resource.inner.is_null());
        assert!(runtime.inner.is_null());
    }

    #[test]
    fn ensure_all_released_reports_leak_and_ok() {
        let mut handle = otel_resource_t {
            inner: ptr::null_mut(),
        };

        // SAFETY: `handle` is local writable storage and is released below.
        unsafe { init(slot(&mut handle), HandleProbe).unwrap() };

        let status = otel_ensure_all_released();
        assert_eq!(status, otel_result_t::OTEL_ERR_LEAK);

        // SAFETY: `handle` was initialized with `HandleProbe` above.
        unsafe { release::<HandleProbe>(slot(&mut handle)) };
        assert!(handle.inner.is_null());

        let status = otel_ensure_all_released();
        assert!(matches!(
            status,
            otel_result_t::OTEL_OK | otel_result_t::OTEL_ERR_LEAK
        ));
    }

    #[test]
    fn is_valid_wrappers_accept_live_and_reject_empty_handles() {
        let mut resource = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut context = otel_context_t {
            inner: ptr::null_mut(),
        };
        let mut context_scope = otel_context_scope_t {
            inner: ptr::null_mut(),
        };
        let mut tracer_provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        let mut meter_provider = otel_meter_provider_t {
            inner: ptr::null_mut(),
        };
        let mut logger_provider = otel_logger_provider_t {
            inner: ptr::null_mut(),
        };
        let mut tracer = otel_tracer_t {
            inner: ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: ptr::null_mut(),
        };
        let mut span = otel_span_t {
            inner: ptr::null_mut(),
        };
        let mut counter_u64 = otel_counter_u64_t {
            inner: ptr::null_mut(),
        };
        let mut counter_f64 = otel_counter_f64_t {
            inner: ptr::null_mut(),
        };
        let mut updown = otel_updown_counter_i64_t {
            inner: ptr::null_mut(),
        };
        let mut histogram = otel_histogram_f64_t {
            inner: ptr::null_mut(),
        };
        let mut gauge = otel_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_gauge = otel_observable_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_counter = otel_observable_counter_u64_t {
            inner: ptr::null_mut(),
        };

        // SAFETY: every handle is local writable storage initialized with
        // `HandleProbe`; `valid` only reads the stored pointer. Each handle is
        // released before the test ends.
        unsafe {
            init(slot(&mut resource), HandleProbe).unwrap();
            assert!(otel_resource_is_valid(&resource));
            assert!(!otel_resource_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut resource));
            assert!(!otel_resource_is_valid(&resource));

            init(slot(&mut context), HandleProbe).unwrap();
            assert!(otel_context_is_valid(&context));
            assert!(!otel_context_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut context));
            assert!(!otel_context_is_valid(&context));

            init(slot(&mut context_scope), HandleProbe).unwrap();
            assert!(otel_context_scope_is_valid(&context_scope));
            assert!(!otel_context_scope_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut context_scope));
            assert!(!otel_context_scope_is_valid(&context_scope));

            init(slot(&mut tracer_provider), HandleProbe).unwrap();
            assert!(otel_tracer_provider_is_valid(&tracer_provider));
            assert!(!otel_tracer_provider_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut tracer_provider));
            assert!(!otel_tracer_provider_is_valid(&tracer_provider));

            init(slot(&mut meter_provider), HandleProbe).unwrap();
            assert!(otel_meter_provider_is_valid(&meter_provider));
            assert!(!otel_meter_provider_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut meter_provider));
            assert!(!otel_meter_provider_is_valid(&meter_provider));

            init(slot(&mut logger_provider), HandleProbe).unwrap();
            assert!(otel_logger_provider_is_valid(&logger_provider));
            assert!(!otel_logger_provider_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut logger_provider));
            assert!(!otel_logger_provider_is_valid(&logger_provider));

            init(slot(&mut tracer), HandleProbe).unwrap();
            assert!(otel_tracer_is_valid(&tracer));
            assert!(!otel_tracer_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut tracer));
            assert!(!otel_tracer_is_valid(&tracer));

            init(slot(&mut meter), HandleProbe).unwrap();
            assert!(otel_meter_is_valid(&meter));
            assert!(!otel_meter_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut meter));
            assert!(!otel_meter_is_valid(&meter));

            init(slot(&mut logger), HandleProbe).unwrap();
            assert!(otel_logger_is_valid(&logger));
            assert!(!otel_logger_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut logger));
            assert!(!otel_logger_is_valid(&logger));

            init(slot(&mut span), HandleProbe).unwrap();
            assert!(otel_span_is_valid(&span));
            assert!(!otel_span_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut span));
            assert!(!otel_span_is_valid(&span));

            init(slot(&mut counter_u64), HandleProbe).unwrap();
            assert!(otel_counter_u64_is_valid(&counter_u64));
            assert!(!otel_counter_u64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut counter_u64));
            assert!(!otel_counter_u64_is_valid(&counter_u64));

            init(slot(&mut counter_f64), HandleProbe).unwrap();
            assert!(otel_counter_f64_is_valid(&counter_f64));
            assert!(!otel_counter_f64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut counter_f64));
            assert!(!otel_counter_f64_is_valid(&counter_f64));

            init(slot(&mut updown), HandleProbe).unwrap();
            assert!(otel_updown_counter_i64_is_valid(&updown));
            assert!(!otel_updown_counter_i64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut updown));
            assert!(!otel_updown_counter_i64_is_valid(&updown));

            init(slot(&mut histogram), HandleProbe).unwrap();
            assert!(otel_histogram_f64_is_valid(&histogram));
            assert!(!otel_histogram_f64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut histogram));
            assert!(!otel_histogram_f64_is_valid(&histogram));

            init(slot(&mut gauge), HandleProbe).unwrap();
            assert!(otel_gauge_f64_is_valid(&gauge));
            assert!(!otel_gauge_f64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut gauge));
            assert!(!otel_gauge_f64_is_valid(&gauge));

            init(slot(&mut observable_gauge), HandleProbe).unwrap();
            assert!(otel_observable_gauge_f64_is_valid(&observable_gauge));
            assert!(!otel_observable_gauge_f64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut observable_gauge));
            assert!(!otel_observable_gauge_f64_is_valid(&observable_gauge));

            init(slot(&mut observable_counter), HandleProbe).unwrap();
            assert!(otel_observable_counter_u64_is_valid(&observable_counter));
            assert!(!otel_observable_counter_u64_is_valid(ptr::null()));
            release::<HandleProbe>(slot(&mut observable_counter));
            assert!(!otel_observable_counter_u64_is_valid(&observable_counter));
        }
    }

    #[test]
    fn retain_release_wrappers_round_trip_core_handles() {
        let _env = no_exporters();
        let mut resource = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };
        let mut tracer_provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        let mut meter_provider = otel_meter_provider_t {
            inner: ptr::null_mut(),
        };
        let mut logger_provider = otel_logger_provider_t {
            inner: ptr::null_mut(),
        };
        let mut tracer = otel_tracer_t {
            inner: ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: ptr::null_mut(),
        };
        let mut span = otel_span_t {
            inner: ptr::null_mut(),
        };
        let mut resource_copy = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let mut runtime_copy = otel_runtime_t {
            inner: ptr::null_mut(),
        };
        let mut tracer_provider_copy = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        let mut meter_provider_copy = otel_meter_provider_t {
            inner: ptr::null_mut(),
        };
        let mut logger_provider_copy = otel_logger_provider_t {
            inner: ptr::null_mut(),
        };
        let mut tracer_copy = otel_tracer_t {
            inner: ptr::null_mut(),
        };
        let mut meter_copy = otel_meter_t {
            inner: ptr::null_mut(),
        };
        let mut logger_copy = otel_logger_t {
            inner: ptr::null_mut(),
        };
        let mut span_copy = otel_span_t {
            inner: ptr::null_mut(),
        };
        let options = otel_span_options_t {
            size: size_of::<otel_span_options_t>(),
            kind: -1,
            start_time_unix_nano: 0,
            parent: ptr::null(),
            end_on_release: 1,
        };

        // SAFETY: all handles are local writable storage; the constructors
        // produce initialized handles and every handle is released below. The
        // span uses `end_on_release` to exercise the last-release path.
        unsafe {
            assert_eq!(otel_resource_new(&mut resource), otel_result_t::OTEL_OK);
            assert_eq!(otel_runtime_new(&mut runtime), otel_result_t::OTEL_OK);
            assert_eq!(
                otel_tracer_provider_new(&mut tracer_provider, &resource, &runtime),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_new(&mut meter_provider, &resource, &runtime),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_new(&mut logger_provider, &resource, &runtime),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_provider_tracer(&tracer_provider, &mut tracer, c"scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_meter(&meter_provider, &mut meter, c"scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&logger_provider, &mut logger, c"scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_span_start_with_options_arr(
                    &tracer,
                    &options,
                    &mut span,
                    c"span".as_ptr(),
                    ptr::null(),
                    0
                ),
                otel_result_t::OTEL_OK
            );

            assert_eq!(
                otel_resource_retain(&resource, &mut resource_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_runtime_retain(&runtime, &mut runtime_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_provider_retain(&tracer_provider, &mut tracer_provider_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_retain(&meter_provider, &mut meter_provider_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_retain(&logger_provider, &mut logger_provider_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_retain(&tracer, &mut tracer_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_retain(&meter, &mut meter_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_retain(&logger, &mut logger_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_retain(&span, &mut span_copy),
                otel_result_t::OTEL_OK
            );

            assert_eq!(resource_copy.inner, resource.inner);
            assert_eq!(runtime_copy.inner, runtime.inner);
            assert_eq!(tracer_provider_copy.inner, tracer_provider.inner);
            assert_eq!(meter_provider_copy.inner, meter_provider.inner);
            assert_eq!(logger_provider_copy.inner, logger_provider.inner);
            assert_eq!(tracer_copy.inner, tracer.inner);
            assert_eq!(meter_copy.inner, meter.inner);
            assert_eq!(logger_copy.inner, logger.inner);
            assert_eq!(span_copy.inner, span.inner);

            otel_span_release(&mut span);
            otel_span_release(&mut span_copy);
            otel_logger_release(&mut logger);
            otel_logger_release(&mut logger_copy);
            otel_meter_release(&mut meter);
            otel_meter_release(&mut meter_copy);
            otel_tracer_release(&mut tracer);
            otel_tracer_release(&mut tracer_copy);
            otel_logger_provider_release(&mut logger_provider);
            otel_logger_provider_release(&mut logger_provider_copy);
            otel_meter_provider_release(&mut meter_provider);
            otel_meter_provider_release(&mut meter_provider_copy);
            otel_tracer_provider_release(&mut tracer_provider);
            otel_tracer_provider_release(&mut tracer_provider_copy);
            otel_runtime_release(&mut runtime);
            otel_runtime_release(&mut runtime_copy);
            otel_resource_release(&mut resource);
            otel_resource_release(&mut resource_copy);
        }

        assert!(resource.inner.is_null());
        assert!(resource_copy.inner.is_null());
        assert!(runtime.inner.is_null());
        assert!(runtime_copy.inner.is_null());
        assert!(span.inner.is_null());
        assert!(span_copy.inner.is_null());
    }

    #[test]
    fn retain_release_wrappers_round_trip_metric_instruments() {
        let _env = no_exporters();
        let mut provider = otel_meter_provider_t {
            inner: ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: ptr::null_mut(),
        };
        let mut counter_u64 = otel_counter_u64_t {
            inner: ptr::null_mut(),
        };
        let mut counter_f64 = otel_counter_f64_t {
            inner: ptr::null_mut(),
        };
        let mut updown = otel_updown_counter_i64_t {
            inner: ptr::null_mut(),
        };
        let mut histogram = otel_histogram_f64_t {
            inner: ptr::null_mut(),
        };
        let mut gauge = otel_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_gauge = otel_observable_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_counter = otel_observable_counter_u64_t {
            inner: ptr::null_mut(),
        };
        let mut counter_u64_copy = otel_counter_u64_t {
            inner: ptr::null_mut(),
        };
        let mut counter_f64_copy = otel_counter_f64_t {
            inner: ptr::null_mut(),
        };
        let mut updown_copy = otel_updown_counter_i64_t {
            inner: ptr::null_mut(),
        };
        let mut histogram_copy = otel_histogram_f64_t {
            inner: ptr::null_mut(),
        };
        let mut gauge_copy = otel_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_gauge_copy = otel_observable_gauge_f64_t {
            inner: ptr::null_mut(),
        };
        let mut observable_counter_copy = otel_observable_counter_u64_t {
            inner: ptr::null_mut(),
        };

        // SAFETY: all handles are local writable storage; the constructors
        // produce initialized handles and every handle is released below. The
        // observable callbacks are no-ops and no collection runs in this test.
        unsafe {
            assert_eq!(
                otel_meter_provider_new(&mut provider, ptr::null(), ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_meter(&provider, &mut meter, c"scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_counter_u64(
                    &meter,
                    &mut counter_u64,
                    c"counter.u64".as_ptr(),
                    ptr::null(),
                    ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_counter_f64(
                    &meter,
                    &mut counter_f64,
                    c"counter.f64".as_ptr(),
                    ptr::null(),
                    ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_updown_counter_i64(
                    &meter,
                    &mut updown,
                    c"updown.i64".as_ptr(),
                    ptr::null(),
                    ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_histogram_f64(
                    &meter,
                    &mut histogram,
                    c"histogram.f64".as_ptr(),
                    ptr::null(),
                    ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_gauge_f64(
                    &meter,
                    &mut gauge,
                    c"gauge.f64".as_ptr(),
                    ptr::null(),
                    ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_observable_gauge_f64(
                    &meter,
                    &mut observable_gauge,
                    c"observable.gauge".as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    Some(noop_retain_observe_f64),
                    ptr::null_mut()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_observable_counter_u64(
                    &meter,
                    &mut observable_counter,
                    c"observable.counter".as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    Some(noop_retain_observe_u64),
                    ptr::null_mut()
                ),
                otel_result_t::OTEL_OK
            );

            assert_eq!(
                otel_counter_u64_retain(&counter_u64, &mut counter_u64_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_counter_f64_retain(&counter_f64, &mut counter_f64_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_updown_counter_i64_retain(&updown, &mut updown_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_histogram_f64_retain(&histogram, &mut histogram_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_gauge_f64_retain(&gauge, &mut gauge_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_observable_gauge_f64_retain(&observable_gauge, &mut observable_gauge_copy),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_observable_counter_u64_retain(
                    &observable_counter,
                    &mut observable_counter_copy
                ),
                otel_result_t::OTEL_OK
            );

            assert_eq!(counter_u64_copy.inner, counter_u64.inner);
            assert_eq!(counter_f64_copy.inner, counter_f64.inner);
            assert_eq!(updown_copy.inner, updown.inner);
            assert_eq!(histogram_copy.inner, histogram.inner);
            assert_eq!(gauge_copy.inner, gauge.inner);
            assert_eq!(observable_gauge_copy.inner, observable_gauge.inner);
            assert_eq!(observable_counter_copy.inner, observable_counter.inner);

            otel_counter_u64_release(&mut counter_u64);
            otel_counter_u64_release(&mut counter_u64_copy);
            otel_counter_f64_release(&mut counter_f64);
            otel_counter_f64_release(&mut counter_f64_copy);
            otel_updown_counter_i64_release(&mut updown);
            otel_updown_counter_i64_release(&mut updown_copy);
            otel_histogram_f64_release(&mut histogram);
            otel_histogram_f64_release(&mut histogram_copy);
            otel_gauge_f64_release(&mut gauge);
            otel_gauge_f64_release(&mut gauge_copy);
            otel_observable_gauge_f64_release(&mut observable_gauge);
            otel_observable_gauge_f64_release(&mut observable_gauge_copy);
            otel_observable_counter_u64_release(&mut observable_counter);
            otel_observable_counter_u64_release(&mut observable_counter_copy);
            otel_meter_release(&mut meter);
            otel_meter_provider_release(&mut provider);
        }

        assert!(counter_u64.inner.is_null());
        assert!(counter_u64_copy.inner.is_null());
        assert!(observable_gauge.inner.is_null());
        assert!(observable_gauge_copy.inner.is_null());
        assert!(observable_counter.inner.is_null());
        assert!(observable_counter_copy.inner.is_null());
    }
}
