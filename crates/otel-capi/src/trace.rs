//! Trace provider, tracer, and span APIs.

use std::env;
use std::ffi::{VaList, c_char};
use std::ptr;
use std::sync::Arc;
use std::time::SystemTime;

use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry::trace::{
    Span as _, SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState, Tracer as _,
    TracerProvider as _,
};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
    Span as SdkSpan,
};

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config;
use crate::config::{
    OtlpProtocol, build_error, duration_ms, exporter_kind, otlp_protocol, resource_from_env,
    sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, optional_utf8, otel_result_t, utf8};
use crate::handles::ContextInner;
use crate::handles::{
    RuntimeInner, SpanInner, TracerInner, TracerProviderInner, arc, const_slot, init,
    optional_resource, optional_runtime, slot,
};
use crate::handles::{
    otel_context_t, otel_runtime_t, otel_span_t, otel_tracer_provider_t, otel_tracer_t,
};
use crate::item_export::ItemSpanProcessor;
use crate::propagation::{buffer_utf8, otel_traceparent_t};
use crate::strings::{optional_str_slice, otel_str_t, str_slice};
use crate::transport::callback_client;
use crate::util::{copy_to_buffer, system_time};

/// A 16-byte OpenTelemetry trace ID, stored as two big-endian 64-bit words.
///
/// Two `uint64_t` fields are used instead of `unsigned __int128` so the ABI is
/// portable across C compilers. `hi` holds the first eight bytes of the ID.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
pub struct otel_trace_id_t {
    pub hi: u64,
    pub lo: u64,
}

/// An 8-byte OpenTelemetry span ID, stored as a big-endian `uint64_t`.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
pub struct otel_span_id_t {
    pub value: u64,
}

impl otel_trace_id_t {
    fn from_trace_id(id: TraceId) -> Self {
        let bytes = id.to_bytes();
        Self {
            hi: u64::from_be_bytes(
                bytes[..8]
                    .try_into()
                    .expect("trace ID has eight high bytes"),
            ),
            lo: u64::from_be_bytes(bytes[8..].try_into().expect("trace ID has eight low bytes")),
        }
    }

    fn to_trace_id(self) -> TraceId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.hi.to_be_bytes());
        bytes[8..].copy_from_slice(&self.lo.to_be_bytes());
        TraceId::from_bytes(bytes)
    }

    fn from_hex(text: &str) -> Result<Self> {
        if text.len() != 32 {
            return Err(Error::invalid("trace ID must be 32 hex digits"));
        }
        let id = TraceId::from_hex(text)
            .map_err(|_| Error::invalid("trace ID must be 32 hex digits"))?;
        Ok(Self::from_trace_id(id))
    }

    fn to_hex(self) -> String {
        format!("{:016x}{:016x}", self.hi, self.lo)
    }
}

impl otel_span_id_t {
    fn from_span_id(id: SpanId) -> Self {
        Self {
            value: u64::from_be_bytes(id.to_bytes()),
        }
    }

    fn to_span_id(self) -> SpanId {
        SpanId::from_bytes(self.value.to_be_bytes())
    }

    fn from_hex(text: &str) -> Result<Self> {
        if text.len() != 16 {
            return Err(Error::invalid("span ID must be 16 hex digits"));
        }
        let id =
            SpanId::from_hex(text).map_err(|_| Error::invalid("span ID must be 16 hex digits"))?;
        Ok(Self::from_span_id(id))
    }

    fn to_hex(self) -> String {
        format!("{:016x}", self.value)
    }
}

/// Explicit span status exported to C.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_span_status_t {
    /// No status has been set.
    OTEL_SPAN_STATUS_UNSET = 0,
    /// The operation succeeded.
    OTEL_SPAN_STATUS_OK = 1,
    /// The operation failed.
    OTEL_SPAN_STATUS_ERROR = 2,
}

/// Span kind. Pass `-1` in `otel_span_options_t` to use the SDK default.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_span_kind_t {
    OTEL_SPAN_KIND_INTERNAL = 0,
    OTEL_SPAN_KIND_SERVER = 1,
    OTEL_SPAN_KIND_CLIENT = 2,
    OTEL_SPAN_KIND_PRODUCER = 3,
    OTEL_SPAN_KIND_CONSUMER = 4,
}

/// Optional span-start configuration. Initialize with
/// `OTEL_SPAN_OPTIONS_INIT` and set `size` to `sizeof(otel_span_options_t)`.
#[repr(C)]
pub struct otel_span_options_t {
    pub size: usize,
    /// One of `otel_span_kind_t`, or `-1` for the SDK default.
    pub kind: i32,
    /// Unix timestamp in nanoseconds; zero means "now".
    pub start_time_unix_nano: i64,
    /// Optional parent context; NULL uses the context attached to this thread.
    pub parent: *const otel_context_t,
    /// Non-zero to end the span when its last handle is released.
    pub end_on_release: i32,
}

fn span_kind(value: i32) -> Result<Option<SpanKind>> {
    match value {
        -1 => Ok(None),
        0 => Ok(Some(SpanKind::Internal)),
        1 => Ok(Some(SpanKind::Server)),
        2 => Ok(Some(SpanKind::Client)),
        3 => Ok(Some(SpanKind::Producer)),
        4 => Ok(Some(SpanKind::Consumer)),
        other => Err(Error::invalid(format!("invalid span kind {other}"))),
    }
}

unsafe fn span_options(
    options: *const otel_span_options_t,
) -> Result<(Option<SpanKind>, Option<SystemTime>, Option<Context>, bool)> {
    if options.is_null() {
        return Ok((None, None, None, false));
    }
    let options = unsafe { &*options };
    if options.size < std::mem::size_of::<otel_span_options_t>() {
        return Err(Error::invalid("otel_span_options_t.size is too small"));
    }
    let kind = span_kind(options.kind)?;
    let start = system_time(options.start_time_unix_nano)?;
    let parent = if options.parent.is_null() {
        None
    } else {
        let context = unsafe { arc::<ContextInner>(const_slot(options.parent)) }?;
        Some(context.as_ref().clone())
    };
    Ok((kind, start, parent, options.end_on_release != 0))
}

unsafe fn start_span_with_attributes(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: String,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    let tracer = unsafe { arc::<TracerInner>(const_slot(tracer)) }?;
    let (kind, start, parent, end_on_release) = unsafe { span_options(options)? };
    let mut builder = tracer.span_builder(name).with_attributes(attributes);
    if let Some(kind) = kind {
        builder = builder.with_kind(kind);
    }
    if let Some(start) = start {
        builder = builder.with_start_time(start);
    }
    let span: SdkSpan = match parent {
        Some(parent) => tracer.build_with_context(builder, &parent),
        None => tracer.build(builder),
    };
    unsafe { init(slot(handle), SpanInner::new(span, end_on_release)) }
}

unsafe fn start_span(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: String,
    args: VaList,
) -> Result<()> {
    let attributes = unsafe { from_varargs(args) }?;
    unsafe { start_span_with_attributes(tracer, options, handle, name, attributes) }
}

unsafe fn start_span_arr(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: String,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> Result<()> {
    let attributes = unsafe { from_array(attributes, count) }?;
    unsafe { start_span_with_attributes(tracer, options, handle, name, attributes) }
}

unsafe fn with_span<R>(
    span: *const otel_span_t,
    action: impl FnOnce(&mut SdkSpan) -> Result<R>,
) -> Result<R> {
    let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
    let mut guard = inner
        .lock()
        .map_err(|_| Error::internal("span lock is poisoned"))?;
    let span = guard
        .as_mut()
        .ok_or_else(|| Error::invalid("span has already ended"))?;
    action(span)
}

/// Ends a span once, ignoring a poisoned lock because the handle is already
/// being released.
pub(crate) fn end_span_once(inner: &SpanInner) {
    if let Ok(mut guard) = inner.lock() {
        if let Some(span) = guard.as_mut() {
            span.end();
        }
    }
}

/// Returns the `SpanContext` behind a handle, including ended spans.
pub(crate) unsafe fn span_context(span: *const otel_span_t) -> Result<SpanContext> {
    let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
    let guard = inner
        .lock()
        .map_err(|_| Error::internal("span lock is poisoned"))?;
    let span = guard
        .as_ref()
        .ok_or_else(|| Error::invalid("span has already ended"))?;
    Ok(span.span_context().clone())
}

fn sampler_from_env() -> Result<Sampler> {
    let name = env::var("OTEL_TRACES_SAMPLER").unwrap_or_default();
    let arg = env::var("OTEL_TRACES_SAMPLER_ARG").unwrap_or_default();
    let ratio = || {
        let text = arg.trim();
        if text.is_empty() {
            return Ok(1.0);
        }
        text.parse::<f64>().map_err(|_| {
            Error::new(
                otel_result_t::OTEL_ERR_CONFIG,
                "OTEL_TRACES_SAMPLER_ARG must be a number between 0 and 1",
            )
        })
    };

    match name.trim().to_ascii_lowercase().as_str() {
        "" | "always_on" => Ok(Sampler::AlwaysOn),
        "always_off" => Ok(Sampler::AlwaysOff),
        "parentbased_always_on" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOn))),
        "parentbased_always_off" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOff))),
        "traceidratio" => Ok(Sampler::TraceIdRatioBased(ratio()?)),
        "parentbased_traceidratio" => Ok(Sampler::ParentBased(Box::new(
            Sampler::TraceIdRatioBased(ratio()?),
        ))),
        other => Err(Error::new(
            otel_result_t::OTEL_ERR_CONFIG,
            format!("OTEL_TRACES_SAMPLER={other} is not supported"),
        )),
    }
}

fn build_otlp_exporter(runtime: &Arc<RuntimeInner>) -> Result<opentelemetry_otlp::SpanExporter> {
    let protocol = otlp_protocol("TRACES")?;
    let builder = opentelemetry_otlp::SpanExporter::builder();
    let exporter = match protocol {
        OtlpProtocol::Grpc => builder
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_http_client(callback_client(
                runtime,
                config::OTLP_GRPC_TRANSPORT,
                "TRACES",
            )?)
            .build(),
        OtlpProtocol::HttpBinary | OtlpProtocol::HttpJson => builder
            .with_http()
            .with_protocol(protocol.into())
            .with_http_client(callback_client(
                runtime,
                config::OTLP_HTTP_TRANSPORT,
                "TRACES",
            )?)
            .build(),
    };
    exporter.map_err(|err| build_error("failed to build OTLP span exporter", err))
}

fn build_stdout_exporter(runtime: &Arc<RuntimeInner>) -> Result<opentelemetry_otlp::SpanExporter> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_http_client(callback_client(
            runtime,
            config::STDOUT_TRANSPORT,
            "TRACES",
        )?)
        .build();
    exporter.map_err(|err| build_error("failed to build stdout span exporter", err))
}

fn build_provider(
    resource: Option<crate::handles::ResourceInner>,
    runtime: Option<Arc<RuntimeInner>>,
) -> Result<TracerProviderInner> {
    let resource = resource.unwrap_or_else(resource_from_env);
    let mut builder = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler_from_env()?);

    if !sdk_disabled() {
        match exporter_kind("TRACES")? {
            crate::config::ExporterKind::None => {}
            crate::config::ExporterKind::Stdout => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "stdout requires an otel_runtime_t; pass one to otel_tracer_provider_new",
                    )
                })?;
                let exporter = build_stdout_exporter(runtime)?;
                let delay = duration_ms("OTEL_BSP_SCHEDULE_DELAY", 5_000)?;
                let config = BatchConfigBuilder::default()
                    .with_scheduled_delay(delay)
                    .build();
                builder = builder.with_span_processor(
                    BatchSpanProcessor::builder(exporter)
                        .with_batch_config(config)
                        .build(),
                );
            }
            crate::config::ExporterKind::Otlp => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "OTLP requires an otel_runtime_t; pass one to otel_tracer_provider_new",
                    )
                })?;
                let exporter = build_otlp_exporter(runtime)?;
                let delay = duration_ms("OTEL_BSP_SCHEDULE_DELAY", 5_000)?;
                let config = BatchConfigBuilder::default()
                    .with_scheduled_delay(delay)
                    .build();
                builder = builder.with_span_processor(
                    BatchSpanProcessor::builder(exporter)
                        .with_batch_config(config)
                        .build(),
                );
            }
            crate::config::ExporterKind::Item => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires an otel_runtime_t; pass one to otel_tracer_provider_new",
                    )
                })?;
                let transport = runtime.item_transport()?.ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires otel_runtime_set_item_transport",
                    )
                })?;
                builder = builder.with_span_processor(ItemSpanProcessor::new(transport));
            }
        }
    }
    Ok(TracerProviderInner {
        provider: builder.build(),
        runtime,
    })
}

/// Creates a tracer provider using the current environment configuration.
/// `resource` may be NULL to build one from the environment. `runtime` may be
/// NULL unless `OTEL_TRACES_EXPORTER=otlp`.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_new(
    handle: *mut otel_tracer_provider_t,
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

/// Flushes all pending spans.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_force_flush(
    provider: *const otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush spans", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its processors.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_shutdown(
    provider: *const otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down tracer provider", err))?;
        Ok(())
    })
}

/// Creates a tracer for an instrumentation scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_tracer(
    provider: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        let name = utf8(unsafe { c_str(name, "tracer name")? }, "tracer name")?;
        unsafe { init(slot(handle), provider.provider.tracer(name)) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_tracer_provider_tracer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_tracer_n(
    provider: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        let name = unsafe { str_slice(name, "tracer name")? }.to_owned();
        unsafe { init(slot(handle), provider.provider.tracer(name)) }?;
        Ok(())
    })
}

/// Starts a span with an optional `OTEL_ATTR_END`-terminated attribute list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        unsafe { start_span(tracer, ptr::null(), handle, name, attributes) }
    })
}

/// Array-based variant of [`otel_tracer_span_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_arr(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        unsafe { start_span_arr(tracer, ptr::null(), handle, name, attributes, count) }
    })
}

/// Starts a span with an explicit parent, kind, and start timestamp.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        unsafe { start_span(tracer, options, handle, name, attributes) }
    })
}

/// Array-based variant of [`otel_tracer_span_start_with_options`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options_arr(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        unsafe { start_span_arr(tracer, options, handle, name, attributes, count) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_n(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        unsafe { start_span(tracer, ptr::null(), handle, name, attributes) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_arr_n(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        unsafe { start_span_arr(tracer, ptr::null(), handle, name, attributes, count) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_with_options`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options_n(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        unsafe { start_span(tracer, options, handle, name, attributes) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_with_options_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options_arr_n(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        unsafe { start_span_arr(tracer, options, handle, name, attributes, count) }
    })
}

unsafe fn set_span_attributes(span: *const otel_span_t, attributes: Vec<KeyValue>) -> Result<()> {
    unsafe {
        with_span(span, |span| {
            span.set_attributes(attributes);
            Ok(())
        })
    }
}

/// Adds attributes to a started span.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_attributes(
    span: *const otel_span_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        unsafe { set_span_attributes(span, attributes) }
    })
}

/// Array-based variant of [`otel_span_set_attributes`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_attributes_arr(
    span: *const otel_span_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        unsafe { set_span_attributes(span, attributes) }
    })
}

unsafe fn add_span_event(
    span: *const otel_span_t,
    name: String,
    timestamp: Option<i64>,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    unsafe {
        with_span(span, |span| {
            match timestamp {
                Some(value) => {
                    let timestamp = system_time(value)?
                        .ok_or_else(|| Error::invalid("event timestamp must be non-zero"))?;
                    span.add_event_with_timestamp(name, timestamp, attributes);
                }
                None => span.add_event(name, attributes),
            }
            Ok(())
        })
    }
}

/// Adds a named event with optional attributes to a started span.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event(
    span: *const otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_event`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_arr(
    span: *const otel_span_t,
    name: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Sets the span status. `description` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_status(
    span: *const otel_span_t,
    status: i32,
    description: *const c_char,
) -> otel_result_t {
    guard(|| unsafe {
        with_span(span, |span| {
            let description = optional_utf8(description, "status description")?.unwrap_or_default();
            let status = match status {
                0 => Status::Unset,
                1 => Status::Ok,
                2 => Status::Error {
                    description: description.into(),
                },
                other => {
                    return Err(Error::invalid(format!("invalid span status {other}")));
                }
            };
            span.set_status(status);
            Ok(())
        })
    })
}

/// Updates the name of a started span.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_update_name(
    span: *const otel_span_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        unsafe {
            with_span(span, |span| {
                span.update_name(name);
                Ok(())
            })
        }
    })
}

/// Length-aware variant of [`otel_span_set_status`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_status_n(
    span: *const otel_span_t,
    status: i32,
    description: otel_str_t,
) -> otel_result_t {
    guard(|| unsafe {
        with_span(span, |span| {
            let description = optional_str_slice(description, "status description")?
                .unwrap_or_default()
                .to_owned();
            let status = match status {
                0 => Status::Unset,
                1 => Status::Ok,
                2 => Status::Error {
                    description: description.into(),
                },
                other => return Err(Error::invalid(format!("invalid span status {other}"))),
            };
            span.set_status(status);
            Ok(())
        })
    })
}

/// Length-aware variant of [`otel_span_update_name`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_update_name_n(
    span: *const otel_span_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        unsafe {
            with_span(span, |span| {
                span.update_name(name);
                Ok(())
            })
        }
    })
}

/// Adds an event with an explicit Unix timestamp in nanoseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp(
    span: *const otel_span_t,
    name: *const c_char,
    timestamp_unix_nano: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Array-based variant of [`otel_span_add_event_with_timestamp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_arr(
    span: *const otel_span_t,
    name: *const c_char,
    timestamp_unix_nano: i64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_n(
    span: *const otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_arr_n(
    span: *const otel_span_t,
    name: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_with_timestamp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_n(
    span: *const otel_span_t,
    name: otel_str_t,
    timestamp_unix_nano: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_with_timestamp_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_arr_n(
    span: *const otel_span_t,
    name: otel_str_t,
    timestamp_unix_nano: i64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

unsafe fn add_span_link(
    span: *const otel_span_t,
    trace_id: otel_trace_id_t,
    span_id: otel_span_id_t,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    let context = SpanContext::new(
        trace_id.to_trace_id(),
        span_id.to_span_id(),
        TraceFlags::SAMPLED,
        false,
        TraceState::NONE,
    );
    if !context.is_valid() {
        return Err(Error::invalid("link span context is not valid"));
    }
    unsafe {
        with_span(span, |span| {
            span.add_link(context, attributes);
            Ok(())
        })
    }
}

/// Adds a link to a span using numeric trace and span IDs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link(
    span: *const otel_span_t,
    trace_id: otel_trace_id_t,
    span_id: otel_span_id_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_link`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_arr(
    span: *const otel_span_t,
    trace_id: otel_trace_id_t,
    span_id: otel_span_id_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

unsafe fn traceparent_ids(
    parent: *const otel_traceparent_t,
) -> Result<(otel_trace_id_t, otel_span_id_t)> {
    let text = unsafe { buffer_utf8(parent)? };
    Ok((
        otel_trace_id_t::from_hex(&text[3..35])?,
        otel_span_id_t::from_hex(&text[36..52])?,
    ))
}

/// Adds a link to a span using the IDs from a validated W3C `traceparent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_from_traceparent(
    span: *const otel_span_t,
    parent: *const otel_traceparent_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let (trace_id, span_id) = unsafe { traceparent_ids(parent)? };
        let attributes = unsafe { from_varargs(attributes) }?;
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_link_from_traceparent`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_from_traceparent_arr(
    span: *const otel_span_t,
    parent: *const otel_traceparent_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let (trace_id, span_id) = unsafe { traceparent_ids(parent)? };
        let attributes = unsafe { from_array(attributes, count) }?;
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

unsafe fn record_span_exception(
    span: *const otel_span_t,
    exception_type: Option<String>,
    message: Option<String>,
    stacktrace: Option<String>,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    let mut values = attributes;
    for (key, value) in [
        ("exception.type", exception_type),
        ("exception.message", message),
        ("exception.stacktrace", stacktrace),
    ] {
        if let Some(value) = value {
            values.push(KeyValue::new(key, value));
        }
    }
    unsafe {
        with_span(span, |span| {
            span.add_event("exception", values);
            Ok(())
        })
    }
}

/// Records an exception as an event with standard semantic attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception(
    span: *const otel_span_t,
    exception_type: *const c_char,
    message: *const c_char,
    stacktrace: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let exception_type = unsafe { optional_utf8(exception_type, "exception type")? };
        let message = unsafe { optional_utf8(message, "exception message")? };
        let stacktrace = unsafe { optional_utf8(stacktrace, "exception stacktrace")? };
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Array-based variant of [`otel_span_record_exception`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception_arr(
    span: *const otel_span_t,
    exception_type: *const c_char,
    message: *const c_char,
    stacktrace: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let exception_type = unsafe { optional_utf8(exception_type, "exception type")? };
        let message = unsafe { optional_utf8(message, "exception message")? };
        let stacktrace = unsafe { optional_utf8(stacktrace, "exception stacktrace")? };
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Length-aware variant of [`otel_span_record_exception`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception_n(
    span: *const otel_span_t,
    exception_type: otel_str_t,
    message: otel_str_t,
    stacktrace: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        let exception_type =
            unsafe { optional_str_slice(exception_type, "exception type")? }.map(str::to_owned);
        let message =
            unsafe { optional_str_slice(message, "exception message")? }.map(str::to_owned);
        let stacktrace =
            unsafe { optional_str_slice(stacktrace, "exception stacktrace")? }.map(str::to_owned);
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Length-aware variant of [`otel_span_record_exception_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception_arr_n(
    span: *const otel_span_t,
    exception_type: otel_str_t,
    message: otel_str_t,
    stacktrace: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        let exception_type =
            unsafe { optional_str_slice(exception_type, "exception type")? }.map(str::to_owned);
        let message =
            unsafe { optional_str_slice(message, "exception message")? }.map(str::to_owned);
        let stacktrace =
            unsafe { optional_str_slice(stacktrace, "exception stacktrace")? }.map(str::to_owned);
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Ends a span at an explicit Unix timestamp in nanoseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_end_with_timestamp(
    span: *const otel_span_t,
    timestamp_unix_nano: i64,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        if let Some(span) = guard.as_mut() {
            match system_time(timestamp_unix_nano)? {
                Some(timestamp) => span.end_with_timestamp(timestamp),
                None => span.end(),
            }
        }
        Ok(())
    })
}

/// Ends a span. Calling this more than once is harmless.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_end(span: *const otel_span_t) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        if let Some(span) = guard.as_mut() {
            span.end();
        }
        Ok(())
    })
}

/// Copies a span's 128-bit trace ID into numeric storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_trace_id(
    span: *const otel_span_t,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        unsafe { *out = otel_trace_id_t::from_trace_id(span.span_context().trace_id()) };
        Ok(())
    })
}

/// Copies a span's 64-bit span ID into numeric storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_span_id(
    span: *const otel_span_t,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        unsafe { *out = otel_span_id_t::from_span_id(span.span_context().span_id()) };
        Ok(())
    })
}

/// Parses 32 hex digits into a numeric trace ID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_trace_id_from_str(
    src: *const c_char,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        let text = utf8(unsafe { c_str(src, "trace ID")? }, "trace ID")?;
        unsafe { *out = otel_trace_id_t::from_hex(&text)? };
        Ok(())
    })
}

/// Parses 16 hex digits into a numeric span ID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_from_str(
    src: *const c_char,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        let text = utf8(unsafe { c_str(src, "span ID")? }, "span ID")?;
        unsafe { *out = otel_span_id_t::from_hex(&text)? };
        Ok(())
    })
}

/// Formats a trace ID as 32 lowercase hex digits plus a NUL terminator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_trace_id_to_str(
    value: *const otel_trace_id_t,
    out: *mut c_char,
    capacity: usize,
) -> otel_result_t {
    guard(|| {
        if value.is_null() {
            return Err(Error::invalid("trace ID must not be NULL"));
        }
        // SAFETY: the caller guarantees `value` points to a live trace ID.
        let text = unsafe { &*value }.to_hex();
        copy_to_buffer(&text, out, capacity)
    })
}

/// Formats a span ID as 16 lowercase hex digits plus a NUL terminator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_to_str(
    value: *const otel_span_id_t,
    out: *mut c_char,
    capacity: usize,
) -> otel_result_t {
    guard(|| {
        if value.is_null() {
            return Err(Error::invalid("span ID must not be NULL"));
        }
        // SAFETY: the caller guarantees `value` points to a live span ID.
        let text = unsafe { &*value }.to_hex();
        copy_to_buffer(&text, out, capacity)
    })
}

/// Returns whether a trace ID contains a non-zero word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_trace_id_is_valid(value: *const otel_trace_id_t) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `value` points to a live trace ID.
    let value = unsafe { &*value };
    value.hi != 0 || value.lo != 0
}

/// Returns whether a span ID is non-zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_is_valid(value: *const otel_span_id_t) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `value` points to a live span ID.
    unsafe { (*value).value != 0 }
}

/// Returns whether two trace IDs hold the same 128-bit value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_trace_id_equal(
    left: *const otel_trace_id_t,
    right: *const otel_trace_id_t,
) -> bool {
    if left.is_null() || right.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees both pointers point to live trace IDs.
    unsafe { *left == *right }
}

/// Returns whether two span IDs hold the same 64-bit value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_equal(
    left: *const otel_span_id_t,
    right: *const otel_span_id_t,
) -> bool {
    if left.is_null() || right.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees both pointers point to live span IDs.
    unsafe { *left == *right }
}

/// Extracts the trace ID from a validated W3C `traceparent` header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_trace_id(
    parent: *const otel_traceparent_t,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        let (trace_id, _) = unsafe { traceparent_ids(parent)? };
        unsafe { *out = trace_id };
        Ok(())
    })
}

/// Extracts the span ID from a validated W3C `traceparent` header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_span_id(
    parent: *const otel_traceparent_t,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        let (_, span_id) = unsafe { traceparent_ids(parent)? };
        unsafe { *out = span_id };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::{
        otel_span_release, otel_span_retain, otel_tracer_provider_release, otel_tracer_release,
    };
    use std::sync::Mutex;

    static TRACE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn end_on_release_ends_retained_span() {
        let _guard = TRACE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { env::set_var("OTEL_TRACES_EXPORTER", "none") };
        let mut provider = crate::handles::otel_tracer_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut tracer = crate::handles::otel_tracer_t {
            inner: std::ptr::null_mut(),
        };
        let mut span = crate::handles::otel_span_t {
            inner: std::ptr::null_mut(),
        };
        let mut retained = crate::handles::otel_span_t {
            inner: std::ptr::null_mut(),
        };
        unsafe {
            assert_eq!(
                otel_tracer_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_provider_tracer(&provider, &mut tracer, c"test".as_ptr()),
                otel_result_t::OTEL_OK
            );
            let options = otel_span_options_t {
                size: std::mem::size_of::<otel_span_options_t>(),
                kind: -1,
                start_time_unix_nano: 0,
                parent: std::ptr::null(),
                end_on_release: 1,
            };
            assert_eq!(
                otel_tracer_span_start_with_options(
                    &tracer,
                    &options,
                    &mut span,
                    c"deferred".as_ptr(),
                    std::ptr::null::<crate::attributes::otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_retain(&span, &mut retained),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut span);
            assert_eq!(
                otel_span_update_name(&retained, c"still-open".as_ptr()),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut retained);
            otel_tracer_release(&mut tracer);
            otel_tracer_provider_release(&mut provider);
            env::remove_var("OTEL_TRACES_EXPORTER");
        }
    }

    #[test]
    fn no_exporter_provider_creates_tracer_and_span() {
        // Isolate this test from ambient configuration.
        let _guard = TRACE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            env::set_var("OTEL_TRACES_EXPORTER", "none");
        }
        let mut provider = crate::handles::otel_tracer_provider_t {
            inner: std::ptr::null_mut(),
        };
        assert_eq!(
            unsafe { otel_tracer_provider_new(&mut provider, std::ptr::null(), std::ptr::null()) },
            otel_result_t::OTEL_OK
        );
        let mut tracer = crate::handles::otel_tracer_t {
            inner: std::ptr::null_mut(),
        };
        assert_eq!(
            unsafe { otel_tracer_provider_tracer(&provider, &mut tracer, c"test".as_ptr()) },
            otel_result_t::OTEL_OK
        );
        unsafe {
            otel_tracer_release(&mut tracer);
            otel_tracer_provider_release(&mut provider);
            env::remove_var("OTEL_TRACES_EXPORTER");
        }
    }

    #[test]
    fn numeric_ids_round_trip_hex() {
        let trace = otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(trace.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        let span = otel_span_id_t::from_hex("00f067aa0ba902b7").unwrap();
        assert_eq!(span.to_hex(), "00f067aa0ba902b7");
        assert!(unsafe { otel_trace_id_is_valid(&trace) });
        assert!(unsafe { otel_span_id_is_valid(&span) });
        assert!(unsafe { otel_trace_id_equal(&trace, &trace) });
        assert!(unsafe { otel_span_id_equal(&span, &span) });
        assert!(!(unsafe { otel_trace_id_is_valid(&otel_trace_id_t::default()) }));
        assert!(!(unsafe { otel_span_id_is_valid(&otel_span_id_t::default()) }));
        assert!(otel_trace_id_t::from_hex("42").is_err());
        assert!(otel_span_id_t::from_hex("42").is_err());
    }
}
