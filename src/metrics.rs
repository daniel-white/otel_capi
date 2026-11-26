//! Meter provider, meter, and metric instrument APIs.

#[cfg(test)]
use std::env;

use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::sync::Arc;

use opentelemetry::metrics::{AsyncInstrument, MeterProvider as _};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use crate::attributes::from_varargs;
use crate::config::{
    build_error, duration_ms, exporter_kind, resource_from_env, sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, optional_utf8, otel_result_t, utf8};
use crate::handles::{
    CounterF64Inner, CounterU64Inner, GaugeF64Inner, HistogramF64Inner, MeterInner,
    MeterProviderInner, RuntimeInner, UpDownCounterI64Inner, arc, const_slot, init,
    optional_resource, optional_runtime, slot,
};
use crate::handles::{
    otel_counter_f64_t, otel_counter_u64_t, otel_gauge_f64_t, otel_histogram_f64_t,
    otel_meter_provider_t, otel_meter_t, otel_observable_counter_u64_t,
    otel_observable_gauge_f64_t, otel_runtime_t, otel_updown_counter_i64_t,
};

/// Opaque callback context passed back to `otel_observe_*`. It is valid only
/// for the duration of the C callback invocation.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_observe_context_t {
    pub data: *const c_void,
    pub vtable: *const c_void,
}

/// Callback invoked while an observable f64 instrument is collected.
pub type otel_observable_f64_callback_t =
    Option<unsafe extern "C" fn(otel_observe_context_t, *mut c_void)>;

/// Callback invoked while an observable u64 instrument is collected.
pub type otel_observable_u64_callback_t =
    Option<unsafe extern "C" fn(otel_observe_context_t, *mut c_void)>;

struct CCallback<T> {
    callback: unsafe extern "C" fn(otel_observe_context_t, *mut c_void),
    user_data: *mut c_void,
    _marker: PhantomData<fn() -> T>,
}

// SAFETY: the caller keeps `user_data` valid and safe for concurrent access
// until the meter provider is shut down.
unsafe impl<T> Send for CCallback<T> {}
// SAFETY: see `Send` above.
unsafe impl<T> Sync for CCallback<T> {}

impl<T> CCallback<T> {
    fn invoke(&self, instrument: &dyn AsyncInstrument<T>) {
        // SAFETY: the layout is the data and vtable pointers of a trait object.
        let context = unsafe {
            std::mem::transmute::<&dyn AsyncInstrument<T>, otel_observe_context_t>(instrument)
        };
        // SAFETY: the caller supplied a valid callback and user data.
        unsafe { (self.callback)(context, self.user_data) };
    }
}

unsafe fn instrument_from_context<'a, T>(
    context: otel_observe_context_t,
) -> &'a dyn AsyncInstrument<T> {
    // SAFETY: `context` must come from an active `CCallback::invoke` call.
    unsafe { std::mem::transmute::<otel_observe_context_t, &dyn AsyncInstrument<T>>(context) }
}

fn build_provider(
    resource: Option<crate::handles::ResourceInner>,
    runtime: Option<Arc<RuntimeInner>>,
) -> Result<MeterProviderInner> {
    let resource = resource.unwrap_or_else(resource_from_env);
    let mut builder = SdkMeterProvider::builder().with_resource(resource);

    if !sdk_disabled() {
        let interval = duration_ms("OTEL_METRIC_EXPORT_INTERVAL", 60_000)?;
        match exporter_kind("METRICS")? {
            crate::config::ExporterKind::None => {}
            crate::config::ExporterKind::Stdout => {
                builder = builder.with_reader(
                    PeriodicReader::builder(opentelemetry_stdout::MetricExporter::default())
                        .with_interval(interval)
                        .build(),
                );
            }
            crate::config::ExporterKind::Otlp => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "OTLP requires an otel_runtime_t; pass one to otel_meter_provider_new",
                    )
                })?;
                let exporter = {
                    let _guard = runtime.enter();
                    opentelemetry_otlp::MetricExporter::builder().build()
                }
                .map_err(|err| build_error("failed to build OTLP metric exporter", err))?;
                builder = builder.with_reader(
                    PeriodicReader::builder(exporter)
                        .with_interval(interval)
                        .build(),
                );
            }
        }
    }
    Ok(MeterProviderInner {
        provider: builder.build(),
        runtime,
    })
}

/// # Safety
/// `unit` and `description` must be NULL or NUL-terminated strings.
unsafe fn instrument_options(
    unit: *const c_char,
    description: *const c_char,
) -> Result<(Option<String>, Option<String>)> {
    Ok(
        (unsafe { optional_utf8(unit, "instrument unit")? }, unsafe {
            optional_utf8(description, "instrument description")?
        }),
    )
}

/// Creates a meter provider. `resource` and `runtime` may be NULL when the
/// selected exporter does not need them.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_new(
    handle: *mut otel_meter_provider_t,
    resource: *const crate::handles::otel_resource_t,
    runtime: *const otel_runtime_t,
) -> otel_result_t {
    guard(|| {
        let resource = optional_resource(resource)?;
        let runtime = optional_runtime(runtime)?;
        unsafe { init(slot(handle), build_provider(resource, runtime)?) }?;
        Ok(())
    })
}

/// Flushes all pending metric data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_force_flush(
    provider: *const otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush metrics", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its readers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_shutdown(
    provider: *const otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down meter provider", err))?;
        Ok(())
    })
}

/// Creates a meter for an instrumentation scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_meter(
    provider: *const otel_meter_provider_t,
    handle: *mut otel_meter_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        let name = utf8(unsafe { c_str(name, "meter name")? }, "meter name")?;
        // Meter scope names are retained by the provider for its lifetime.
        let name: &'static str = Box::leak(name.into_boxed_str());
        unsafe { init(slot(handle), provider.provider.meter(name)) }?;
        Ok(())
    })
}

/// Creates an unsigned 64-bit counter. `unit` and `description` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_u64(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_u64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(unsafe { c_str(name, "counter name")? }, "counter name")?;
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.u64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision counter. `unit` and `description` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(unsafe { c_str(name, "counter name")? }, "counter name")?;
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a signed 64-bit up/down counter. Metadata may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_updown_counter_i64(
    meter: *const otel_meter_t,
    handle: *mut otel_updown_counter_i64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            unsafe { c_str(name, "up/down counter name")? },
            "up/down counter name",
        )?;
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.i64_up_down_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision histogram. Metadata may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_histogram_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_histogram_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(unsafe { c_str(name, "histogram name")? }, "histogram name")?;
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_histogram(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision gauge. Metadata may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_gauge_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_gauge_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(unsafe { c_str(name, "gauge name")? }, "gauge name")?;
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_gauge(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates an observable f64 gauge. `user_data` is passed to `callback` on
/// every collection and must stay valid and thread-safe until the meter
/// provider is shut down.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_observable_gauge_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_observable_gauge_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
    callback: otel_observable_f64_callback_t,
    user_data: *mut c_void,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            unsafe { c_str(name, "observable gauge name")? },
            "observable gauge name",
        )?;
        let (unit, description) = unsafe { instrument_options(unit, description)? };
        let callback = callback.ok_or_else(|| Error::invalid("callback must not be NULL"))?;
        let wrapper = Arc::new(CCallback::<f64> {
            callback,
            user_data,
            _marker: PhantomData,
        });
        let mut builder = meter.f64_observable_gauge(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        let builder = builder.with_callback(move |observer| wrapper.invoke(observer));
        unsafe { init(slot(handle), builder.build()) }
    })
}

/// Creates an observable u64 counter. `user_data` is passed to `callback` on
/// every collection and must stay valid and thread-safe until the meter
/// provider is shut down.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_observable_counter_u64(
    meter: *const otel_meter_t,
    handle: *mut otel_observable_counter_u64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
    callback: otel_observable_u64_callback_t,
    user_data: *mut c_void,
) -> otel_result_t {
    guard(|| {
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            unsafe { c_str(name, "observable counter name")? },
            "observable counter name",
        )?;
        let (unit, description) = unsafe { instrument_options(unit, description)? };
        let callback = callback.ok_or_else(|| Error::invalid("callback must not be NULL"))?;
        let wrapper = Arc::new(CCallback::<u64> {
            callback,
            user_data,
            _marker: PhantomData,
        });
        let mut builder = meter.u64_observable_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        let builder = builder.with_callback(move |observer| wrapper.invoke(observer));
        unsafe { init(slot(handle), builder.build()) }
    })
}

/// Records an f64 observation from inside an observable callback. The
/// context is valid only for the duration of that callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_f64(
    context: otel_observe_context_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        if context.data.is_null() || context.vtable.is_null() {
            return Err(Error::invalid("observation context is invalid or expired"));
        }
        let attributes = unsafe { from_varargs(attributes) }?;
        let instrument = unsafe { instrument_from_context::<f64>(context) };
        instrument.observe(value, &attributes);
        Ok(())
    })
}

/// Records a u64 observation from inside an observable callback. The context
/// is valid only for the duration of that callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_u64(
    context: otel_observe_context_t,
    value: u64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        if context.data.is_null() || context.vtable.is_null() {
            return Err(Error::invalid("observation context is invalid or expired"));
        }
        let attributes = unsafe { from_varargs(attributes) }?;
        let instrument = unsafe { instrument_from_context::<u64>(context) };
        instrument.observe(value, &attributes);
        Ok(())
    })
}

/// Adds to an unsigned 64-bit counter with optional attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_add(
    counter: *const otel_counter_u64_t,
    value: u64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let counter = unsafe { arc::<CounterU64Inner>(const_slot(counter)) }?;
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Adds to a double-precision counter with optional attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_add(
    counter: *const otel_counter_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let counter = unsafe { arc::<CounterF64Inner>(const_slot(counter)) }?;
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Adds to a signed 64-bit up/down counter with optional attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_add(
    counter: *const otel_updown_counter_i64_t,
    value: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let counter = unsafe { arc::<UpDownCounterI64Inner>(const_slot(counter)) }?;
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Records a value in a histogram with optional attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_record(
    histogram: *const otel_histogram_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let histogram = unsafe { arc::<HistogramF64Inner>(const_slot(histogram)) }?;
        let attributes = unsafe { from_varargs(attributes) }?;
        histogram.record(value, &attributes);
        Ok(())
    })
}

/// Records a value in a gauge with optional attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_record(
    gauge: *const otel_gauge_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let gauge = unsafe { arc::<GaugeF64Inner>(const_slot(gauge)) }?;
        let attributes = unsafe { from_varargs(attributes) }?;
        gauge.record(value, &attributes);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::{
        otel_counter_u64_release, otel_gauge_f64_release, otel_meter_provider_release,
        otel_meter_release,
    };

    #[test]
    fn no_exporter_metric_instruments() {
        unsafe { env::set_var("OTEL_METRICS_EXPORTER", "none") };
        let mut provider = otel_meter_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: std::ptr::null_mut(),
        };
        let mut counter = otel_counter_u64_t {
            inner: std::ptr::null_mut(),
        };
        let mut gauge = otel_gauge_f64_t {
            inner: std::ptr::null_mut(),
        };
        unsafe {
            assert_eq!(
                otel_meter_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_meter(&provider, &mut meter, c"test".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_counter_u64(
                    &meter,
                    &mut counter,
                    c"requests".as_ptr(),
                    c"1".as_ptr(),
                    std::ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_counter_u64_add(
                    &counter,
                    3,
                    std::ptr::null::<crate::attributes::otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_gauge_f64(
                    &meter,
                    &mut gauge,
                    c"temperature".as_ptr(),
                    std::ptr::null(),
                    std::ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_gauge_f64_record(
                    &gauge,
                    21.5,
                    std::ptr::null::<crate::attributes::otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_gauge_f64_release(&mut gauge);
            otel_counter_u64_release(&mut counter);
            otel_meter_release(&mut meter);
            otel_meter_provider_release(&mut provider);
            env::remove_var("OTEL_METRICS_EXPORTER");
        }
    }
}
