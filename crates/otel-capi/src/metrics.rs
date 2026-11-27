//! Meter provider, meter, and metric instrument APIs.

use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::sync::Arc;

use opentelemetry::metrics::{AsyncInstrument, MeterProvider as _};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config;
use crate::config::{
    OtlpProtocol, build_error, duration_ms, exporter_kind, otlp_protocol, resource_from_env,
    sdk_disabled, sdk_error,
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
use crate::item_export::ItemMetricExporter;
use crate::strings::{optional_str_slice, otel_str_t, str_slice};
use crate::transport::callback_client;
use crate::util::interned;

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

fn build_otlp_exporter(runtime: &Arc<RuntimeInner>) -> Result<opentelemetry_otlp::MetricExporter> {
    let protocol = otlp_protocol("METRICS")?;
    let builder = opentelemetry_otlp::MetricExporter::builder();
    let exporter = match protocol {
        OtlpProtocol::Grpc => builder
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_http_client(callback_client(
                runtime,
                config::OTLP_GRPC_TRANSPORT,
                "METRICS",
            )?)
            .build(),
        OtlpProtocol::HttpBinary | OtlpProtocol::HttpJson => builder
            .with_http()
            .with_protocol(protocol.into())
            .with_http_client(callback_client(
                runtime,
                config::OTLP_HTTP_TRANSPORT,
                "METRICS",
            )?)
            .build(),
    };
    exporter.map_err(|err| build_error("failed to build OTLP metric exporter", err))
}

fn build_stdout_exporter(
    runtime: &Arc<RuntimeInner>,
) -> Result<opentelemetry_otlp::MetricExporter> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_http_client(callback_client(
            runtime,
            config::STDOUT_TRANSPORT,
            "METRICS",
        )?)
        .build();
    exporter.map_err(|err| build_error("failed to build stdout metric exporter", err))
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
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "stdout requires an otel_runtime_t; pass one to otel_meter_provider_new",
                    )
                })?;
                let exporter = build_stdout_exporter(runtime)?;
                builder = builder.with_reader(
                    PeriodicReader::builder(exporter)
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
                let exporter = build_otlp_exporter(runtime)?;
                builder = builder.with_reader(
                    PeriodicReader::builder(exporter)
                        .with_interval(interval)
                        .build(),
                );
            }
            crate::config::ExporterKind::Item => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires an otel_runtime_t; pass one to otel_meter_provider_new",
                    )
                })?;
                let transport = runtime.item_transport()?.ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires otel_runtime_set_item_transport",
                    )
                })?;
                builder = builder.with_reader(
                    PeriodicReader::builder(ItemMetricExporter::new(transport))
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
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        (unsafe { optional_utf8(unit, "instrument unit")? }, unsafe {
            optional_utf8(description, "instrument description")?
        }),
    )
}

fn instrument_options_n(
    unit: otel_str_t,
    description: otel_str_t,
) -> Result<(Option<String>, Option<String>)> {
    Ok((
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        unsafe { optional_str_slice(unit, "instrument unit")? }.map(str::to_owned),
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        unsafe { optional_str_slice(description, "instrument description")? }.map(str::to_owned),
    ))
}

/// Length-aware variant of [`otel_meter_counter_u64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_u64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_u64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "counter name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
        let mut builder = meter.u64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_counter_f64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_f64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_f64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "counter name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
        let mut builder = meter.f64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_updown_counter_i64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_updown_counter_i64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_updown_counter_i64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "up/down counter name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
        let mut builder = meter.i64_up_down_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_histogram_f64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_histogram_f64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_histogram_f64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "histogram name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
        let mut builder = meter.f64_histogram(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_gauge_f64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_gauge_f64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_gauge_f64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "gauge name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
        let mut builder = meter.f64_gauge(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_observable_gauge_f64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_observable_gauge_f64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_observable_gauge_f64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
    callback: otel_observable_f64_callback_t,
    user_data: *mut c_void,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "observable gauge name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
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
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }
    })
}

/// Length-aware variant of [`otel_meter_observable_counter_u64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_observable_counter_u64_n(
    meter: *const otel_meter_t,
    handle: *mut otel_observable_counter_u64_t,
    name: otel_str_t,
    unit: otel_str_t,
    description: otel_str_t,
    callback: otel_observable_u64_callback_t,
    user_data: *mut c_void,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "observable counter name")? }.to_owned();
        let (unit, description) = instrument_options_n(unit, description)?;
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
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }
    })
}

/// Creates a meter provider. `resource` and `runtime` may be NULL when the
/// selected exporter does not need them.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_new(
    handle: *mut otel_meter_provider_t,
    resource: *const crate::handles::otel_resource_t,
    runtime: *const otel_runtime_t,
) -> otel_result_t {
    guard(|| {
        let resource = optional_resource(resource)?;
        let runtime = optional_runtime(runtime)?;
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), build_provider(resource, runtime)?) }?;
        Ok(())
    })
}

/// Flushes all pending metric data.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_force_flush(
    provider: *const otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush metrics", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its readers.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_shutdown(
    provider: *const otel_meter_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down meter provider", err))?;
        Ok(())
    })
}

/// Creates a meter for an instrumentation scope.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_meter(
    provider: *const otel_meter_provider_t,
    handle: *mut otel_meter_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "meter name")? }, "meter name")?;
        // Meter scope names are retained by the provider for its lifetime.
        let name = interned(name);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.meter(name)) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_meter_provider_meter`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_provider_meter_n(
    provider: *const otel_meter_provider_t,
    handle: *mut otel_meter_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<MeterProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "meter name")? }.to_owned();
        // Meter scope names are retained by the provider for its lifetime.
        let name = interned(name);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.meter(name)) }?;
        Ok(())
    })
}

/// Creates an unsigned 64-bit counter. `unit` and `description` may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_u64(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_u64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "counter name")? }, "counter name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.u64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision counter. `unit` and `description` may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_counter_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_counter_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "counter name")? }, "counter name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a signed 64-bit up/down counter. Metadata may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_updown_counter_i64(
    meter: *const otel_meter_t,
    handle: *mut otel_updown_counter_i64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { c_str(name, "up/down counter name")? },
            "up/down counter name",
        )?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.i64_up_down_counter(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision histogram. Metadata may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_histogram_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_histogram_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "histogram name")? }, "histogram name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_histogram(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates a double-precision gauge. Metadata may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_meter_gauge_f64(
    meter: *const otel_meter_t,
    handle: *mut otel_gauge_f64_t,
    name: *const c_char,
    unit: *const c_char,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "gauge name")? }, "gauge name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (unit, description) = unsafe { instrument_options(unit, description) }?;
        let mut builder = meter.f64_gauge(name);
        if let Some(unit) = unit {
            builder = builder.with_unit(unit);
        }
        if let Some(description) = description {
            builder = builder.with_description(description);
        }
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }?;
        Ok(())
    })
}

/// Creates an observable f64 gauge. `user_data` is passed to `callback` on
/// every collection and must stay valid and thread-safe until the meter
/// provider is shut down.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { c_str(name, "observable gauge name")? },
            "observable gauge name",
        )?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }
    })
}

/// Creates an observable u64 counter. `user_data` is passed to `callback` on
/// every collection and must stay valid and thread-safe until the meter
/// provider is shut down.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let meter = unsafe { arc::<MeterInner>(const_slot(meter)) }?;
        let name = utf8(
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { c_str(name, "observable counter name")? },
            "observable counter name",
        )?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), builder.build()) }
    })
}

unsafe fn observe_with_attributes<T>(
    context: otel_observe_context_t,
    value: T,
    attributes: Vec<opentelemetry::KeyValue>,
) -> Result<()> {
    if context.data.is_null() || context.vtable.is_null() {
        return Err(Error::invalid("observation context is invalid or expired"));
    }
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    let instrument = unsafe { instrument_from_context::<T>(context) };
    instrument.observe(value, &attributes);
    Ok(())
}

/// Records an f64 observation from inside an observable callback. The
/// context is valid only for the duration of that callback.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_f64(
    context: otel_observe_context_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { observe_with_attributes(context, value, attributes) }
    })
}

/// Array-based variant of [`otel_observe_f64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_f64_arr(
    context: otel_observe_context_t,
    value: f64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { observe_with_attributes(context, value, attributes) }
    })
}

/// Records a u64 observation from inside an observable callback. The context
/// is valid only for the duration of that callback.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_u64(
    context: otel_observe_context_t,
    value: u64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { observe_with_attributes(context, value, attributes) }
    })
}

/// Array-based variant of [`otel_observe_u64`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_observe_u64_arr(
    context: otel_observe_context_t,
    value: u64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { observe_with_attributes(context, value, attributes) }
    })
}

/// Adds to an unsigned 64-bit counter with optional attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_add(
    counter: *const otel_counter_u64_t,
    value: u64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<CounterU64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Array-based variant of [`otel_counter_u64_add`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_u64_add_arr(
    counter: *const otel_counter_u64_t,
    value: u64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<CounterU64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Adds to a double-precision counter with optional attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_add(
    counter: *const otel_counter_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<CounterF64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Array-based variant of [`otel_counter_f64_add`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_counter_f64_add_arr(
    counter: *const otel_counter_f64_t,
    value: f64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<CounterF64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Adds to a signed 64-bit up/down counter with optional attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_add(
    counter: *const otel_updown_counter_i64_t,
    value: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<UpDownCounterI64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Array-based variant of [`otel_updown_counter_i64_add`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_updown_counter_i64_add_arr(
    counter: *const otel_updown_counter_i64_t,
    value: i64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let counter = unsafe { arc::<UpDownCounterI64Inner>(const_slot(counter)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        counter.add(value, &attributes);
        Ok(())
    })
}

/// Records a value in a histogram with optional attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_record(
    histogram: *const otel_histogram_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let histogram = unsafe { arc::<HistogramF64Inner>(const_slot(histogram)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        histogram.record(value, &attributes);
        Ok(())
    })
}

/// Array-based variant of [`otel_histogram_f64_record`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_histogram_f64_record_arr(
    histogram: *const otel_histogram_f64_t,
    value: f64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let histogram = unsafe { arc::<HistogramF64Inner>(const_slot(histogram)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        histogram.record(value, &attributes);
        Ok(())
    })
}

/// Records a value in a gauge with optional attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_record(
    gauge: *const otel_gauge_f64_t,
    value: f64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let gauge = unsafe { arc::<GaugeF64Inner>(const_slot(gauge)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        gauge.record(value, &attributes);
        Ok(())
    })
}

/// Array-based variant of [`otel_gauge_f64_record`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_gauge_f64_record_arr(
    gauge: *const otel_gauge_f64_t,
    value: f64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let gauge = unsafe { arc::<GaugeF64Inner>(const_slot(gauge)) }?;
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        gauge.record(value, &attributes);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::otel_attribute_t;
    use crate::config::tests::EnvGuard;
    use crate::handles::{
        otel_counter_f64_release, otel_counter_u64_release, otel_gauge_f64_release,
        otel_histogram_f64_release, otel_meter_provider_release, otel_meter_release,
        otel_observable_counter_u64_release, otel_observable_gauge_f64_release,
        otel_updown_counter_i64_release,
    };
    use opentelemetry::KeyValue;
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    unsafe extern "C" fn noop_observe_f64(_context: otel_observe_context_t, _data: *mut c_void) {}
    unsafe extern "C" fn noop_observe_u64(_context: otel_observe_context_t, _data: *mut c_void) {}

    #[test]
    fn instrument_options_handle_nulls_and_bad_utf8() {
        // SAFETY: NULL pointers are valid optional metadata values.
        let (unit, description) =
            unsafe { instrument_options(std::ptr::null(), std::ptr::null()) }.unwrap();
        assert_eq!((unit.as_deref(), description.as_deref()), (None, None));
        // SAFETY: the C strings are static literals that outlive the call.
        let (unit, description) =
            unsafe { instrument_options(c"ms".as_ptr(), c"latency".as_ptr()) }.unwrap();
        assert_eq!(unit.as_deref(), Some("ms"));
        assert_eq!(description.as_deref(), Some("latency"));
        let invalid = [0xffu8, 0];
        assert_eq!(
            // SAFETY: `invalid` is a NUL-terminated buffer that outlives the call.
            unsafe { instrument_options(invalid.as_ptr().cast(), std::ptr::null()) }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
        let empty = otel_str_t {
            ptr: std::ptr::null(),
            len: 0,
        };
        assert_eq!(instrument_options_n(empty, empty).unwrap(), (None, None));
        let view = otel_str_t {
            ptr: c"1".as_ptr(),
            len: 1,
        };
        assert_eq!(
            instrument_options_n(view, empty).unwrap(),
            (Some("1".to_owned()), None)
        );
    }

    struct RecordingInstrument {
        calls: AtomicUsize,
    }

    impl AsyncInstrument<f64> for RecordingInstrument {
        fn observe(&self, _value: f64, attributes: &[KeyValue]) {
            assert!(attributes.is_empty());
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct RecordingU64Instrument {
        calls: AtomicUsize,
    }

    impl AsyncInstrument<u64> for RecordingU64Instrument {
        fn observe(&self, _value: u64, attributes: &[KeyValue]) {
            assert!(attributes.is_empty());
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    unsafe extern "C" fn observe_f64_once(context: otel_observe_context_t, user_data: *mut c_void) {
        // SAFETY: `context` is live for this callback and `user_data` points to
        // a writable `Cell<bool>` that outlives the call.
        unsafe {
            let result = otel_observe_f64(context, 4.5, std::ptr::null::<otel_attribute_t>());
            let status = &*user_data.cast::<Cell<bool>>();
            status.set(result == otel_result_t::OTEL_OK);
        }
    }

    unsafe extern "C" fn observe_u64_once(context: otel_observe_context_t, user_data: *mut c_void) {
        // SAFETY: see `observe_f64_once`.
        unsafe {
            let result = otel_observe_u64(context, 7, std::ptr::null::<otel_attribute_t>());
            let status = &*user_data.cast::<Cell<bool>>();
            status.set(result == otel_result_t::OTEL_OK);
        }
    }

    #[test]
    fn observe_from_callback_uses_the_context() {
        let instrument = RecordingInstrument {
            calls: AtomicUsize::new(0),
        };
        let dynamic: &dyn AsyncInstrument<f64> = &instrument;
        let ok = Cell::new(false);
        let callback = CCallback::<f64> {
            callback: observe_f64_once,
            user_data: (&ok as *const Cell<bool>).cast_mut().cast::<c_void>(),
            _marker: PhantomData,
        };
        callback.invoke(dynamic);
        assert!(ok.get());
        assert_eq!(instrument.calls.load(Ordering::Relaxed), 1);

        let invalid = otel_observe_context_t {
            data: std::ptr::null(),
            vtable: std::ptr::null(),
        };
        assert_eq!(
            // SAFETY: the all-NULL context is intentionally invalid and is checked
            // before any pointer is dereferenced.
            unsafe { otel_observe_f64_arr(invalid, 1.0, std::ptr::null(), 0) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: see above.
            unsafe { otel_observe_u64_arr(invalid, 1, std::ptr::null(), 0) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );

        let instrument_u64 = RecordingU64Instrument {
            calls: AtomicUsize::new(0),
        };
        let dynamic_u64: &dyn AsyncInstrument<u64> = &instrument_u64;
        let callback_u64 = CCallback::<u64> {
            callback: observe_u64_once,
            user_data: (&ok as *const Cell<bool>).cast_mut().cast::<c_void>(),
            _marker: PhantomData,
        };
        ok.set(false);
        callback_u64.invoke(dynamic_u64);
        assert!(ok.get());
        assert_eq!(instrument_u64.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn no_exporter_metric_instruments() {
        let mut env = EnvGuard::new();
        env.set("OTEL_METRICS_EXPORTER", "none");
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
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
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
        }
    }

    #[test]
    fn every_instrument_constructor_and_recording_path() {
        let mut env = EnvGuard::new();
        env.set("OTEL_METRICS_EXPORTER", "none");
        let mut provider = otel_meter_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: std::ptr::null_mut(),
        };
        let mut counter_u64 = otel_counter_u64_t {
            inner: std::ptr::null_mut(),
        };
        let mut counter_f64 = otel_counter_f64_t {
            inner: std::ptr::null_mut(),
        };
        let mut updown = otel_updown_counter_i64_t {
            inner: std::ptr::null_mut(),
        };
        let mut histogram = otel_histogram_f64_t {
            inner: std::ptr::null_mut(),
        };
        let mut gauge = otel_gauge_f64_t {
            inner: std::ptr::null_mut(),
        };
        let mut observable_gauge = otel_observable_gauge_f64_t {
            inner: std::ptr::null_mut(),
        };
        let mut observable_counter = otel_observable_counter_u64_t {
            inner: std::ptr::null_mut(),
        };
        let empty = otel_str_t {
            ptr: std::ptr::null(),
            len: 0,
        };
        let name = otel_str_t {
            ptr: c"instrument".as_ptr(),
            len: 10,
        };
        let key = c"k";
        let value = c"v";
        let attribute = crate::attributes::otel_attribute_t {
            kind: crate::attributes::otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
            key: key.as_ptr(),
            key_len: 1,
            value: crate::attributes::otel_attribute_value_t {
                string: value.as_ptr(),
            },
            value_len: 1,
        };
        let attributes = [&attribute as *const otel_attribute_t];
        // SAFETY: all handles are local writable storage; all borrowed strings
        // and the attribute array outlive the calls.
        unsafe {
            assert_eq!(
                otel_meter_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_meter_n(
                    &provider,
                    &mut meter,
                    otel_str_t {
                        ptr: c"scope".as_ptr(),
                        len: 5
                    }
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_force_flush(&provider),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_shutdown(&provider),
                otel_result_t::OTEL_OK
            );

            assert_eq!(
                otel_meter_counter_u64_n(&meter, &mut counter_u64, name, empty, empty),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_counter_f64(
                    &meter,
                    &mut counter_f64,
                    c"f64.counter".as_ptr(),
                    std::ptr::null(),
                    c"description".as_ptr()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_updown_counter_i64_n(&meter, &mut updown, name, empty, empty),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_histogram_f64(
                    &meter,
                    &mut histogram,
                    c"histogram".as_ptr(),
                    c"s".as_ptr(),
                    std::ptr::null()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_gauge_f64_n(&meter, &mut gauge, name, empty, empty),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_observable_gauge_f64(
                    &meter,
                    &mut observable_gauge,
                    c"observable.gauge".as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    Some(noop_observe_f64),
                    std::ptr::null_mut()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_observable_counter_u64_n(
                    &meter,
                    &mut observable_counter,
                    name,
                    empty,
                    empty,
                    Some(noop_observe_u64),
                    std::ptr::null_mut()
                ),
                otel_result_t::OTEL_OK
            );

            assert_eq!(
                otel_counter_u64_add_arr(&counter_u64, 2, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_counter_f64_add(&counter_f64, 1.5, std::ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_counter_f64_add_arr(&counter_f64, 2.5, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_updown_counter_i64_add(&updown, -1, std::ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_updown_counter_i64_add_arr(&updown, 1, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_histogram_f64_record(&histogram, 1.0, std::ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_histogram_f64_record_arr(&histogram, 2.0, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_gauge_f64_record_arr(&gauge, 3.0, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );

            otel_observable_counter_u64_release(&mut observable_counter);
            otel_observable_gauge_f64_release(&mut observable_gauge);
            otel_gauge_f64_release(&mut gauge);
            otel_histogram_f64_release(&mut histogram);
            otel_updown_counter_i64_release(&mut updown);
            otel_counter_f64_release(&mut counter_f64);
            otel_counter_u64_release(&mut counter_u64);
            otel_meter_release(&mut meter);
            otel_meter_provider_release(&mut provider);
        }
    }

    #[test]
    fn instrument_creation_rejects_invalid_inputs() {
        let mut env = EnvGuard::new();
        env.set("OTEL_METRICS_EXPORTER", "none");
        let mut provider = otel_meter_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut meter = otel_meter_t {
            inner: std::ptr::null_mut(),
        };
        let mut counter = otel_counter_u64_t {
            inner: std::ptr::null_mut(),
        };
        let mut observable = otel_observable_gauge_f64_t {
            inner: std::ptr::null_mut(),
        };
        let empty = otel_str_t {
            ptr: std::ptr::null(),
            len: 0,
        };
        // SAFETY: all handles are local writable storage; NULL names and
        // callbacks are intentionally invalid inputs.
        unsafe {
            assert_eq!(
                otel_meter_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_provider_meter(&provider, &mut meter, c"scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_meter_counter_u64(
                    &meter,
                    &mut counter,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_meter_counter_u64_n(&meter, &mut counter, empty, empty, empty),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_meter_observable_gauge_f64(
                    &meter,
                    &mut observable,
                    c"observable".as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    None,
                    std::ptr::null_mut()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_counter_u64_add(std::ptr::null(), 1, std::ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert!(counter.inner.is_null());
            assert!(observable.inner.is_null());
            otel_meter_release(&mut meter);
            otel_meter_provider_release(&mut provider);
        }
    }
}
