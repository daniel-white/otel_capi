//! Caller-owned handles holding `Arc`s.
//!
//! A handle is a `#[repr(C)]` struct with a single opaque pointer field. Rust
//! writes into storage provided by C (stack, heap, arena, or a struct field);
//! it never allocates the handle itself. `*_retain` duplicates into another
//! caller-owned slot and `*_release` drops the `Arc` and nulls the slot.

use std::ffi::{c_void, c_char};
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::runtime::Runtime;

use opentelemetry::metrics::{Counter, Gauge, Histogram, UpDownCounter};
use opentelemetry_sdk::logs::SdkLogger;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, Span as SdkSpan};

use crate::error::{Error, Result, otel_result_t, guard, ok};

pub(crate) type ResourceInner = opentelemetry_sdk::Resource;
pub(crate) type RuntimeInner = Runtime;

pub(crate) struct TracerProviderInner {
    pub(crate) provider: SdkTracerProvider,
    pub(crate) runtime: Option<Arc<Runtime>>,
}

pub(crate) struct MeterProviderInner {
    pub(crate) provider: SdkMeterProvider,
    pub(crate) runtime: Option<Arc<Runtime>>,
}

pub(crate) struct LoggerProviderInner {
    pub(crate) provider: opentelemetry_sdk::logs::SdkLoggerProvider,
    pub(crate) runtime: Option<Arc<Runtime>>,
}
pub(crate) type TracerInner = SdkTracer;
pub(crate) type MeterInner = opentelemetry::metrics::Meter;
pub(crate) type LoggerInner = SdkLogger;
pub(crate) type SpanInner = Mutex<Option<SdkSpan>>;
pub(crate) type CounterU64Inner = Counter<u64>;
pub(crate) type CounterF64Inner = Counter<f64>;
pub(crate) type UpDownCounterI64Inner = UpDownCounter<i64>;
pub(crate) type HistogramF64Inner = Histogram<f64>;
pub(crate) type GaugeF64Inner = Gauge<f64>;

/// A resource describing the entity producing telemetry.
#[repr(C)]
pub struct otel_resource_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Explicit async runtime used by OTLP exporters.
#[repr(C)]
pub struct otel_runtime_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create tracers and control trace export.
#[repr(C)]
pub struct otel_tracer_provider_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create meters and control metric export.
#[repr(C)]
pub struct otel_meter_provider_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Provider used to create loggers and control log export.
#[repr(C)]
pub struct otel_logger_provider_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for starting spans.
#[repr(C)]
pub struct otel_tracer_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for creating metric instruments.
#[repr(C)]
pub struct otel_meter_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Instrumentation scope for emitting log records.
#[repr(C)]
pub struct otel_logger_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// A started span. Ending is idempotent across retained handles.
#[repr(C)]
pub struct otel_span_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Unsigned 64-bit monotonic counter instrument.
#[repr(C)]
pub struct otel_counter_u64_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision monotonic counter instrument.
#[repr(C)]
pub struct otel_counter_f64_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Signed 64-bit up/down counter instrument.
#[repr(C)]
pub struct otel_updown_counter_i64_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision histogram instrument.
#[repr(C)]
pub struct otel_histogram_f64_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
    pub inner: *mut c_void,
}

/// Double-precision gauge instrument.
#[repr(C)]
pub struct otel_gauge_f64_t {
    /// Internal pointer. Treat as opaque; do not read or write it directly.
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

pub(crate) unsafe fn init<T: Send + Sync + 'static>(
    out: *mut *mut c_void,
    value: T,
) -> Result<()> {
    if out.is_null() {
        return Err(Error::invalid("out must not be NULL"));
    }
    // SAFETY: caller guarantees `out` points to writable, aligned storage.
    unsafe { ptr::write(out, Arc::into_raw(Arc::new(value)).cast_mut().cast::<c_void>()) };
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
        // SAFETY: `raw` came from `Arc::into_raw::<T>` and we consumed one count.
        unsafe { drop(Arc::from_raw(raw.cast::<T>())) };
    }
}

/// Clones the resource behind an optional handle, or returns `None` for NULL.
pub(crate) fn optional_resource(
    handle: *const otel_resource_t,
) -> Result<Option<ResourceInner>> {
    if handle.is_null() {
        return Ok(None);
    }
    let arc = unsafe { arc::<ResourceInner>(const_slot(handle)) }?;
    Ok(Some((*arc).clone()))
}

/// Clones the runtime behind an optional handle, or returns `None` for NULL.
pub(crate) fn optional_runtime(handle: *const otel_runtime_t) -> Result<Option<Arc<Runtime>>> {
    if handle.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { arc::<RuntimeInner>(const_slot(handle)) }?))
}

pub(crate) fn copy_string(value: &str, buf: *mut c_char, capacity: usize) -> Result<usize> {
    if buf.is_null() || capacity == 0 {
        return Err(Error::invalid("buf and capacity must be non-zero"));
    }
    let needed = value.len() + 1;
    if capacity < needed {
        return Err(Error::invalid("buffer is too small"));
    }
    // SAFETY: caller guarantees `buf` can hold `needed` bytes.
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr().cast::<c_char>(), buf, value.len());
        *buf.add(value.len()) = 0;
    }
    Ok(needed)
}

pub(crate) fn report(result: Result<()>) -> otel_result_t {
    match result {
        Ok(()) => ok(),
        Err(err) => err.report(),
    }
}

/// Retains a `otel_resource_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_retain(
    src: *const otel_resource_t,
    out: *mut otel_resource_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<ResourceInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_runtime_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<RuntimeInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<TracerProviderInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_meter_provider_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<MeterProviderInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_logger_provider_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<LoggerProviderInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_tracer_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<TracerInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_meter_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<MeterInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_logger_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<LoggerInner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_span_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<SpanInner>(src.cast(), slot(out)) }?; Ok(()) })
}

/// Releases one reference to a `otel_span_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_release(handle: *mut otel_span_t) {
    unsafe { release::<SpanInner>(slot(handle)) }
}

/// Retains a `otel_counter_u64_t` into caller-provided storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_retain(
    src: *const otel_counter_u64_t,
    out: *mut otel_counter_u64_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<CounterU64Inner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_counter_f64_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<CounterF64Inner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_updown_counter_i64_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<UpDownCounterI64Inner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_histogram_f64_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<HistogramF64Inner>(src.cast(), slot(out)) }?; Ok(()) })
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
    out: *mut otel_gauge_f64_t,
) -> otel_result_t {
    guard(|| { unsafe { retain::<GaugeF64Inner>(src.cast(), slot(out)) }?; Ok(()) })
}

/// Releases one reference to a `otel_gauge_f64_t` and clears the slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_release(handle: *mut otel_gauge_f64_t) {
    unsafe { release::<GaugeF64Inner>(slot(handle)) }
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
