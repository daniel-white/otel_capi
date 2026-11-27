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
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider, Span as SdkSpan,
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

type ParsedSpanOptions = (Option<SpanKind>, Option<SystemTime>, Option<Context>, bool);

unsafe fn span_options(options: *const otel_span_options_t) -> Result<ParsedSpanOptions> {
    if options.is_null() {
        return Ok((None, None, None, false));
    }
    // SAFETY: The caller guarantees the pointer is non-null, aligned, and references an initialized value.
    let options = unsafe { &*options };
    if options.size < std::mem::size_of::<otel_span_options_t>() {
        return Err(Error::invalid("otel_span_options_t.size is too small"));
    }
    let kind = span_kind(options.kind)?;
    let start = system_time(options.start_time_unix_nano)?;
    let parent = if options.parent.is_null() {
        None
    } else {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
    let tracer = unsafe { arc::<TracerInner>(const_slot(tracer)) }?;
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
    // SAFETY: The caller provides writable, aligned storage for one handle pointer.
    unsafe { init(slot(handle), SpanInner::new(span, end_on_release)) }
}

unsafe fn start_span(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: String,
    args: VaList,
) -> Result<()> {
    // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
    let attributes = unsafe { from_varargs(args) }?;
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
    // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
    let attributes = unsafe { from_array(attributes, count) }?;
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { start_span_with_attributes(tracer, options, handle, name, attributes) }
}

unsafe fn with_span<R>(
    span: *const otel_span_t,
    action: impl FnOnce(&mut SdkSpan) -> Result<R>,
) -> Result<R> {
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
    if let Ok(mut guard) = inner.lock()
        && let Some(span) = guard.as_mut()
    {
        span.end();
    }
}

/// Returns the `SpanContext` behind a handle, including ended spans.
pub(crate) unsafe fn span_context(span: *const otel_span_t) -> Result<SpanContext> {
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_new(
    handle: *mut otel_tracer_provider_t,
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

/// Flushes all pending spans.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_force_flush(
    provider: *const otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush spans", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its processors.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_shutdown(
    provider: *const otel_tracer_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down tracer provider", err))?;
        Ok(())
    })
}

/// Creates a tracer for an instrumentation scope.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_tracer(
    provider: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "tracer name")? }, "tracer name")?;
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.tracer(name)) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_tracer_provider_tracer`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_provider_tracer_n(
    provider: *const otel_tracer_provider_t,
    handle: *mut otel_tracer_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<TracerProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "tracer name")? }.to_owned();
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.tracer(name)) }?;
        Ok(())
    })
}

/// Starts a span with an optional `OTEL_ATTR_END`-terminated attribute list.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        // SAFETY: The caller guarantees the pointer targets valid, aligned storage for this operation.
        unsafe { start_span(tracer, ptr::null(), handle, name, attributes) }
    })
}

/// Array-based variant of [`otel_tracer_span_start`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_arr(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        // SAFETY: The caller guarantees the pointer targets valid, aligned storage for this operation.
        unsafe { start_span_arr(tracer, ptr::null(), handle, name, attributes, count) }
    })
}

/// Starts a span with an explicit parent, kind, and start timestamp.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { start_span(tracer, options, handle, name, attributes) }
    })
}

/// Array-based variant of [`otel_tracer_span_start_with_options`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { start_span_arr(tracer, options, handle, name, attributes, count) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_n(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        // SAFETY: The caller guarantees the pointer targets valid, aligned storage for this operation.
        unsafe { start_span(tracer, ptr::null(), handle, name, attributes) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_arr_n(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        // SAFETY: The caller guarantees the pointer targets valid, aligned storage for this operation.
        unsafe { start_span_arr(tracer, ptr::null(), handle, name, attributes, count) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_with_options`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start_with_options_n(
    tracer: *const otel_tracer_t,
    options: *const otel_span_options_t,
    handle: *mut otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { start_span(tracer, options, handle, name, attributes) }
    })
}

/// Length-aware variant of [`otel_tracer_span_start_with_options_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { start_span_arr(tracer, options, handle, name, attributes, count) }
    })
}

unsafe fn set_span_attributes(span: *const otel_span_t, attributes: Vec<KeyValue>) -> Result<()> {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        with_span(span, |span| {
            span.set_attributes(attributes);
            Ok(())
        })
    }
}

/// Adds attributes to a started span.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_attributes(
    span: *const otel_span_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { set_span_attributes(span, attributes) }
    })
}

/// Array-based variant of [`otel_span_set_attributes`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_attributes_arr(
    span: *const otel_span_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { set_span_attributes(span, attributes) }
    })
}

unsafe fn add_span_event(
    span: *const otel_span_t,
    name: String,
    timestamp: Option<i64>,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event(
    span: *const otel_span_t,
    name: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_event`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_arr(
    span: *const otel_span_t,
    name: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Sets the span status. `description` may be NULL.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_status(
    span: *const otel_span_t,
    status: i32,
    description: *const c_char,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_update_name(
    span: *const otel_span_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe {
            with_span(span, |span| {
                span.update_name(name);
                Ok(())
            })
        }
    })
}

/// Length-aware variant of [`otel_span_set_status`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_status_n(
    span: *const otel_span_t,
    status: i32,
    description: otel_str_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_update_name_n(
    span: *const otel_span_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "span name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe {
            with_span(span, |span| {
                span.update_name(name);
                Ok(())
            })
        }
    })
}

/// Adds an event with an explicit Unix timestamp in nanoseconds.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp(
    span: *const otel_span_t,
    name: *const c_char,
    timestamp_unix_nano: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Array-based variant of [`otel_span_add_event_with_timestamp`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_arr(
    span: *const otel_span_t,
    name: *const c_char,
    timestamp_unix_nano: i64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_n(
    span: *const otel_span_t,
    name: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_arr_n(
    span: *const otel_span_t,
    name: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, None, attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_with_timestamp`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_n(
    span: *const otel_span_t,
    name: otel_str_t,
    timestamp_unix_nano: i64,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_event(span, name, Some(timestamp_unix_nano), attributes) }
    })
}

/// Length-aware variant of [`otel_span_add_event_with_timestamp_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event_with_timestamp_arr_n(
    span: *const otel_span_t,
    name: otel_str_t,
    timestamp_unix_nano: i64,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "event name")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        with_span(span, |span| {
            span.add_link(context, attributes);
            Ok(())
        })
    }
}

/// Adds a link to a span using numeric trace and span IDs.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link(
    span: *const otel_span_t,
    trace_id: otel_trace_id_t,
    span_id: otel_span_id_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_link`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_arr(
    span: *const otel_span_t,
    trace_id: otel_trace_id_t,
    span_id: otel_span_id_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

unsafe fn traceparent_ids(
    parent: *const otel_traceparent_t,
) -> Result<(otel_trace_id_t, otel_span_id_t)> {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    let text = unsafe { buffer_utf8(parent)? };
    Ok((
        otel_trace_id_t::from_hex(&text[3..35])?,
        otel_span_id_t::from_hex(&text[36..52])?,
    ))
}

/// Adds a link to a span using the IDs from a validated W3C `traceparent`.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_from_traceparent(
    span: *const otel_span_t,
    parent: *const otel_traceparent_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (trace_id, span_id) = unsafe { traceparent_ids(parent)? };
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { add_span_link(span, trace_id, span_id, attributes) }
    })
}

/// Array-based variant of [`otel_span_add_link_from_traceparent`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_link_from_traceparent_arr(
    span: *const otel_span_t,
    parent: *const otel_traceparent_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (trace_id, span_id) = unsafe { traceparent_ids(parent)? };
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
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
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe {
        with_span(span, |span| {
            span.add_event("exception", values);
            Ok(())
        })
    }
}

/// Records an exception as an event with standard semantic attributes.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception(
    span: *const otel_span_t,
    exception_type: *const c_char,
    message: *const c_char,
    stacktrace: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let exception_type = unsafe { optional_utf8(exception_type, "exception type")? };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let message = unsafe { optional_utf8(message, "exception message")? };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let stacktrace = unsafe { optional_utf8(stacktrace, "exception stacktrace")? };
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Array-based variant of [`otel_span_record_exception`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let exception_type = unsafe { optional_utf8(exception_type, "exception type")? };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let message = unsafe { optional_utf8(message, "exception message")? };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let stacktrace = unsafe { optional_utf8(stacktrace, "exception stacktrace")? };
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Length-aware variant of [`otel_span_record_exception`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_record_exception_n(
    span: *const otel_span_t,
    exception_type: otel_str_t,
    message: otel_str_t,
    stacktrace: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        let exception_type =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(exception_type, "exception type")? }.map(str::to_owned);
        let message =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(message, "exception message")? }.map(str::to_owned);
        let stacktrace =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(stacktrace, "exception stacktrace")? }.map(str::to_owned);
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Length-aware variant of [`otel_span_record_exception_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        let exception_type =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(exception_type, "exception type")? }.map(str::to_owned);
        let message =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(message, "exception message")? }.map(str::to_owned);
        let stacktrace =
            // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
            unsafe { optional_str_slice(stacktrace, "exception stacktrace")? }.map(str::to_owned);
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { record_span_exception(span, exception_type, message, stacktrace, attributes) }
    })
}

/// Ends a span at an explicit Unix timestamp in nanoseconds.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_end_with_timestamp(
    span: *const otel_span_t,
    timestamp_unix_nano: i64,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_end(span: *const otel_span_t) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_trace_id(
    span: *const otel_span_t,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = otel_trace_id_t::from_trace_id(span.span_context().trace_id()) };
        Ok(())
    })
}

/// Copies a span's 64-bit span ID into numeric storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_span_id(
    span: *const otel_span_t,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner
            .lock()
            .map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = otel_span_id_t::from_span_id(span.span_context().span_id()) };
        Ok(())
    })
}

/// Parses 32 hex digits into a numeric trace ID.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_trace_id_from_str(
    src: *const c_char,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let text = utf8(unsafe { c_str(src, "trace ID")? }, "trace ID")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = otel_trace_id_t::from_hex(&text)? };
        Ok(())
    })
}

/// Parses 16 hex digits into a numeric span ID.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_from_str(
    src: *const c_char,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let text = utf8(unsafe { c_str(src, "span ID")? }, "span ID")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = otel_span_id_t::from_hex(&text)? };
        Ok(())
    })
}

/// Formats a trace ID as 32 lowercase hex digits plus a NUL terminator.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
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
///
/// # Safety
/// The pointer may be null or must point to an initialized handle; no reference is taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_id_is_valid(value: *const otel_span_id_t) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `value` points to a live span ID.
    unsafe { (*value).value != 0 }
}

/// Returns whether two trace IDs hold the same 128-bit value.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_trace_id(
    parent: *const otel_traceparent_t,
    out: *mut otel_trace_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("trace ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (trace_id, _) = unsafe { traceparent_ids(parent)? };
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = trace_id };
        Ok(())
    })
}

/// Extracts the span ID from a validated W3C `traceparent` header.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_span_id(
    parent: *const otel_traceparent_t,
    out: *mut otel_span_id_t,
) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid("span ID output must not be NULL"));
        }
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let (_, span_id) = unsafe { traceparent_ids(parent)? };
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { *out = span_id };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::otel_transport_t;
    use crate::attributes::{otel_attribute_type_t, otel_attribute_value_t};
    use crate::config::tests::EnvGuard;
    use crate::context::{otel_context_from_span, otel_context_release};
    use crate::handles::{
        otel_runtime_release, otel_span_release, otel_span_retain, otel_tracer_provider_release,
        otel_tracer_release,
    };
    use crate::items::otel_item_transport_t;
    use crate::propagation::otel_traceparent_from_str;
    use crate::runtime::{
        otel_runtime_new, otel_runtime_set_item_transport, otel_runtime_set_transport,
    };
    use std::ffi::{CStr, c_void};

    /// Owns a no-exporter provider and tracer, releasing both on drop.
    struct Fixture {
        _env: EnvGuard,
        provider: otel_tracer_provider_t,
        tracer: otel_tracer_t,
    }

    impl Fixture {
        fn new() -> Self {
            let mut env = EnvGuard::new();
            env.set("OTEL_TRACES_EXPORTER", "none");
            let mut provider = otel_tracer_provider_t {
                inner: std::ptr::null_mut(),
            };
            let mut tracer = otel_tracer_t {
                inner: std::ptr::null_mut(),
            };
            // SAFETY: `provider` and `tracer` are writable, aligned handle
            // slots; the C string is static and NUL-terminated.
            unsafe {
                assert_eq!(
                    otel_tracer_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                    otel_result_t::OTEL_OK
                );
                assert_eq!(
                    otel_tracer_provider_tracer(&provider, &mut tracer, c"test".as_ptr()),
                    otel_result_t::OTEL_OK
                );
            }
            Self {
                _env: env,
                provider,
                tracer,
            }
        }

        fn start(&self, name: &CStr) -> otel_span_t {
            let mut span = otel_span_t {
                inner: std::ptr::null_mut(),
            };
            // SAFETY: `span` is writable, aligned storage; `name` and the NULL
            // attribute sentinel are valid for the duration of the call.
            unsafe {
                assert_eq!(
                    otel_tracer_span_start(
                        &self.tracer,
                        &mut span,
                        name.as_ptr(),
                        std::ptr::null::<otel_attribute_t>()
                    ),
                    otel_result_t::OTEL_OK
                );
            }
            span
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: both handles were initialized by `new` and have not been
            // released; release nulls the slots before dropping the Arc.
            unsafe {
                otel_tracer_release(&mut self.tracer);
                otel_tracer_provider_release(&mut self.provider);
            }
        }
    }

    fn str_view(text: &str) -> otel_str_t {
        otel_str_t {
            ptr: text.as_ptr().cast(),
            len: text.len(),
        }
    }

    fn string_attribute(key: &CStr, value: &CStr) -> otel_attribute_t {
        otel_attribute_t {
            kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
            key: key.as_ptr(),
            key_len: key.to_bytes().len(),
            value: otel_attribute_value_t {
                string: value.as_ptr(),
            },
            value_len: value.to_bytes().len(),
        }
    }

    fn new_runtime() -> otel_runtime_t {
        let mut runtime = otel_runtime_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `runtime` is writable, aligned storage for one handle pointer.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );
        runtime
    }

    unsafe extern "C" fn noop_transport_send(
        _request: *const crate::abi::otel_transport_request_t,
        _response: *mut crate::abi::otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn noop_encoded_item(
        _item: *const crate::items::otel_encoded_item_t,
        _user_data: *mut c_void,
    ) {
    }

    #[test]
    fn span_kind_maps_every_option() {
        assert_eq!(span_kind(-1).unwrap(), None);
        assert_eq!(span_kind(0).unwrap(), Some(SpanKind::Internal));
        assert_eq!(span_kind(1).unwrap(), Some(SpanKind::Server));
        assert_eq!(span_kind(2).unwrap(), Some(SpanKind::Client));
        assert_eq!(span_kind(3).unwrap(), Some(SpanKind::Producer));
        assert_eq!(span_kind(4).unwrap(), Some(SpanKind::Consumer));
        assert_eq!(
            span_kind(5).unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn span_options_validates_size_kind_and_time() {
        // SAFETY: NULL is a documented optional pointer for span options.
        let (kind, start, parent, end_on_release) = unsafe { span_options(ptr::null()) }.unwrap();
        assert!(kind.is_none() && start.is_none() && parent.is_none());
        assert!(!end_on_release);

        let mut options = otel_span_options_t {
            size: 1,
            kind: -1,
            start_time_unix_nano: 0,
            parent: ptr::null(),
            end_on_release: 0,
        };
        assert_eq!(
            // SAFETY: `options` is an initialized local with an intentionally small size.
            unsafe { span_options(&options) }.unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        options.size = std::mem::size_of::<otel_span_options_t>();
        options.kind = 42;
        assert_eq!(
            // SAFETY: `options` is initialized; only its kind discriminator is invalid.
            unsafe { span_options(&options) }.unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        options.kind = -1;
        options.start_time_unix_nano = -1;
        assert_eq!(
            // SAFETY: `options` is initialized; only its timestamp is invalid.
            unsafe { span_options(&options) }.unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        options.start_time_unix_nano = 0;
        options.end_on_release = 1;
        // SAFETY: `options` now contains valid values in a local struct.
        let (kind, start, parent, end_on_release) = unsafe { span_options(&options) }.unwrap();
        assert!(kind.is_none() && start.is_none() && parent.is_none());
        assert!(end_on_release);
    }

    #[test]
    fn sampler_from_env_selects_supported_samplers() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_TRACES_SAMPLER");
        env.remove("OTEL_TRACES_SAMPLER_ARG");
        assert!(matches!(sampler_from_env().unwrap(), Sampler::AlwaysOn));

        env.set("OTEL_TRACES_SAMPLER", "always_off");
        assert!(matches!(sampler_from_env().unwrap(), Sampler::AlwaysOff));
        env.set("OTEL_TRACES_SAMPLER", "parentbased_always_on");
        assert!(matches!(
            sampler_from_env().unwrap(),
            Sampler::ParentBased(_)
        ));
        env.set("OTEL_TRACES_SAMPLER", "traceidratio");
        env.set("OTEL_TRACES_SAMPLER_ARG", "0.25");
        match sampler_from_env().unwrap() {
            Sampler::TraceIdRatioBased(value) => assert_eq!(value, 0.25),
            other => panic!("unexpected sampler: {other:?}"),
        }
        env.set("OTEL_TRACES_SAMPLER", "parentbased_traceidratio");
        assert!(matches!(
            sampler_from_env().unwrap(),
            Sampler::ParentBased(_)
        ));
        env.set("OTEL_TRACES_SAMPLER_ARG", "nope");
        assert_eq!(
            sampler_from_env().unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.set("OTEL_TRACES_SAMPLER_ARG", "0.5");
        env.set("OTEL_TRACES_SAMPLER", "bogus");
        assert_eq!(
            sampler_from_env().unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
    }

    #[test]
    fn provider_requires_runtime_for_configured_exporters() {
        let mut env = EnvGuard::new();
        for exporter in ["otlp", "stdout", "item"] {
            env.set("OTEL_TRACES_EXPORTER", exporter);
            let mut provider = otel_tracer_provider_t {
                inner: ptr::null_mut(),
            };
            // SAFETY: `provider` is writable, aligned storage; NULL resource and
            // runtime pointers are intentionally invalid configured arguments.
            unsafe {
                assert_eq!(
                    otel_tracer_provider_new(&mut provider, ptr::null(), ptr::null()),
                    otel_result_t::OTEL_ERR_CONFIG,
                    "{exporter} should require a runtime"
                );
            }
            assert!(provider.inner.is_null());
        }
    }

    #[test]
    fn span_start_variants_cover_names_attributes_and_parent() {
        let fixture = Fixture::new();
        let attribute = string_attribute(c"start.key", c"value");
        let attributes = [&attribute as *const otel_attribute_t];

        // SAFETY: every handle slot is writable and aligned; attribute and
        // string pointers reference initialized locals that outlive the calls.
        unsafe {
            let mut array = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_arr(
                    &fixture.tracer,
                    &mut array,
                    c"array".as_ptr(),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut array);

            let mut named = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_n(
                    &fixture.tracer,
                    &mut named,
                    str_view("named"),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut named);

            let mut array_named = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_arr_n(
                    &fixture.tracer,
                    &mut array_named,
                    str_view("array-n"),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut array_named);

            let mut parent = fixture.start(c"parent");
            let mut context = otel_context_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_context_from_span(&parent, &mut context),
                otel_result_t::OTEL_OK
            );
            let options = otel_span_options_t {
                size: std::mem::size_of::<otel_span_options_t>(),
                kind: otel_span_kind_t::OTEL_SPAN_KIND_CLIENT as i32,
                start_time_unix_nano: 1_700_000_000_000_000_000,
                parent: &context,
                end_on_release: 0,
            };
            let (kind, start, parent_context, end_on_release) = span_options(&options).unwrap();
            assert_eq!(kind, Some(SpanKind::Client));
            assert!(start.is_some());
            assert!(parent_context.is_some());
            assert!(!end_on_release);

            let mut child = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_with_options(
                    &fixture.tracer,
                    &options,
                    &mut child,
                    c"child".as_ptr(),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut child);

            let mut child_array = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_with_options_arr(
                    &fixture.tracer,
                    &options,
                    &mut child_array,
                    c"child-array".as_ptr(),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut child_array);

            let mut child_named = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_with_options_n(
                    &fixture.tracer,
                    &options,
                    &mut child_named,
                    str_view("child-n"),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut child_named);

            let mut child_array_named = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start_with_options_arr_n(
                    &fixture.tracer,
                    &options,
                    &mut child_array_named,
                    str_view("child-array-n"),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut child_array_named);

            otel_context_release(&mut context);
            otel_span_release(&mut parent);
        }
    }

    #[test]
    fn span_updates_events_and_status_are_accepted() {
        let fixture = Fixture::new();
        let mut span = fixture.start(c"updates");
        let attribute = string_attribute(c"event.key", c"value");
        let attributes = [&attribute as *const otel_attribute_t];
        let invalid_utf8 = [0xffu8, 0];

        // SAFETY: `span` is initialized; all strings and attributes are valid
        // or intentionally invalid inputs that are checked before use.
        unsafe {
            assert_eq!(
                otel_span_update_name(&span, c"renamed".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_update_name_n(&span, str_view("length-aware")),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_update_name(&span, invalid_utf8.as_ptr().cast()),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_span_set_status(&span, 0, ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status(&span, 1, ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status(&span, 2, c"failed".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status_n(&span, 2, str_view("length-aware error")),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status(&span, 3, ptr::null()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_set_attributes(&span, &attribute, ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_attributes_arr(&span, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event(
                    &span,
                    c"event".as_ptr(),
                    &attribute,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_arr(&span, c"event-array".as_ptr(), attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_n(
                    &span,
                    str_view("event-n"),
                    &attribute,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_arr_n(&span, str_view("event-array-n"), attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_with_timestamp(
                    &span,
                    c"timed".as_ptr(),
                    1_700_000_000_000_000_000,
                    &attribute,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_with_timestamp_arr(
                    &span,
                    c"timed-array".as_ptr(),
                    1_700_000_000_000_000_000,
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_with_timestamp_n(
                    &span,
                    str_view("timed-n"),
                    1_700_000_000_000_000_000,
                    &attribute,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_with_timestamp_arr_n(
                    &span,
                    str_view("timed-array-n"),
                    1_700_000_000_000_000_000,
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_event_with_timestamp(
                    &span,
                    c"bad".as_ptr(),
                    0,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_add_event_with_timestamp(
                    &span,
                    c"negative".as_ptr(),
                    -1,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
        }
        // SAFETY: `span` was initialized by `Fixture::start` with a valid tracer.
        unsafe { otel_span_release(&mut span) };
    }

    #[test]
    fn span_links_and_exceptions_cover_optional_fields() {
        let fixture = Fixture::new();
        let mut span = fixture.start(c"linked");
        let trace_id = otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        let span_id = otel_span_id_t::from_hex("00f067aa0ba902b7").unwrap();
        let attribute = string_attribute(c"link.key", c"value");
        let attributes = [&attribute as *const otel_attribute_t];

        // SAFETY: `span` is initialized, IDs are non-zero, and all strings and
        // attribute pointers reference valid initialized storage.
        unsafe {
            assert_eq!(
                otel_span_add_link(&span, trace_id, span_id, ptr::null::<otel_attribute_t>()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_link_arr(&span, trace_id, span_id, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_link(
                    &span,
                    otel_trace_id_t::default(),
                    span_id,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_record_exception(
                    &span,
                    c"TypeError".as_ptr(),
                    c"boom".as_ptr(),
                    c"stack".as_ptr(),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_record_exception_arr(
                    &span,
                    c"ValueError".as_ptr(),
                    ptr::null(),
                    c"frame".as_ptr(),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_record_exception_n(
                    &span,
                    str_view("IoError"),
                    str_view("disk full"),
                    str_view("trace"),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_record_exception_arr_n(
                    &span,
                    str_view("RuntimeError"),
                    str_view("oops"),
                    str_view("frame"),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
        }
        // SAFETY: `span` was initialized by `Fixture::start` with a valid tracer.
        unsafe { otel_span_release(&mut span) };
    }

    #[test]
    fn traceparent_ids_flow_into_outputs_and_links() {
        let mut parent = otel_traceparent_t { value: [0; 56] };
        // SAFETY: `parent` is writable, aligned storage and the C string is a
        // static, NUL-terminated, valid W3C traceparent header.
        unsafe {
            assert_eq!(
                otel_traceparent_from_str(
                    c"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".as_ptr(),
                    &mut parent
                ),
                otel_result_t::OTEL_OK
            );
        }
        let expected_trace = otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        let expected_span = otel_span_id_t::from_hex("00f067aa0ba902b7").unwrap();
        let mut trace_out = otel_trace_id_t::default();
        let mut span_out = otel_span_id_t::default();
        let attribute = string_attribute(c"traceparent.key", c"value");
        let attributes = [&attribute as *const otel_attribute_t];
        let fixture = Fixture::new();
        let mut span = fixture.start(c"traceparent");

        // SAFETY: the traceparent buffer is validated, the output slots are
        // writable, and the span and attribute pointers are initialized.
        unsafe {
            assert_eq!(
                otel_traceparent_trace_id(&parent, &mut trace_out),
                otel_result_t::OTEL_OK
            );
            assert_eq!(trace_out.to_hex(), expected_trace.to_hex());
            assert_eq!(
                otel_traceparent_span_id(&parent, &mut span_out),
                otel_result_t::OTEL_OK
            );
            assert_eq!(span_out.to_hex(), expected_span.to_hex());
            assert_eq!(
                otel_traceparent_trace_id(&parent, ptr::null_mut()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_traceparent_span_id(&parent, ptr::null_mut()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_add_link_from_traceparent(
                    &span,
                    &parent,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_add_link_from_traceparent_arr(&span, &parent, attributes.as_ptr(), 1),
                otel_result_t::OTEL_OK
            );
            let invalid = otel_traceparent_t { value: [0; 56] };
            assert_eq!(
                otel_span_add_link_from_traceparent(
                    &span,
                    &invalid,
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
        }
        // SAFETY: `span` was initialized by `Fixture::start` with a valid tracer.
        unsafe { otel_span_release(&mut span) };
    }

    #[test]
    fn id_ffi_round_trips_and_rejects_bad_buffers() {
        let trace = otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        let span = otel_span_id_t::from_hex("00f067aa0ba902b7").unwrap();
        let mut parsed_trace = otel_trace_id_t::default();
        let mut parsed_span = otel_span_id_t::default();
        let mut trace_buffer = [0 as c_char; 33];
        let mut span_buffer = [0 as c_char; 17];
        let mut short_buffer = [0 as c_char; 32];

        // SAFETY: output slots are writable and large enough for their IDs;
        // static C strings and the intentionally undersized buffer are checked
        // by the functions before any out-of-bounds write.
        unsafe {
            assert_eq!(
                otel_trace_id_from_str(
                    c"4bf92f3577b34da6a3ce929d0e0e4736".as_ptr(),
                    &mut parsed_trace
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(parsed_trace.to_hex(), trace.to_hex());
            assert_eq!(
                otel_span_id_from_str(c"00f067aa0ba902b7".as_ptr(), &mut parsed_span),
                otel_result_t::OTEL_OK
            );
            assert_eq!(parsed_span.to_hex(), span.to_hex());
            assert_eq!(
                otel_trace_id_from_str(
                    c"4bf92f3577b34da6a3ce929d0e0e4736".as_ptr(),
                    ptr::null_mut()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_id_from_str(c"00f067aa0ba902b7".as_ptr(), ptr::null_mut()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_trace_id_to_str(&trace, trace_buffer.as_mut_ptr(), trace_buffer.len()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                CStr::from_ptr(trace_buffer.as_ptr()).to_str().unwrap(),
                trace.to_hex()
            );
            assert_eq!(
                otel_span_id_to_str(&span, span_buffer.as_mut_ptr(), span_buffer.len()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                CStr::from_ptr(span_buffer.as_ptr()).to_str().unwrap(),
                span.to_hex()
            );
            assert_eq!(
                otel_trace_id_to_str(&trace, short_buffer.as_mut_ptr(), short_buffer.len()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_trace_id_to_str(ptr::null(), trace_buffer.as_mut_ptr(), trace_buffer.len()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_id_to_str(ptr::null(), span_buffer.as_mut_ptr(), span_buffer.len()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert!(!otel_trace_id_is_valid(ptr::null()));
            assert!(!otel_span_id_is_valid(ptr::null()));
            assert!(!otel_trace_id_equal(ptr::null(), &trace));
            assert!(!otel_span_id_equal(&span, ptr::null()));
        }
    }

    #[test]
    fn provider_flush_and_shutdown_are_safe_without_processors() {
        let fixture = Fixture::new();
        // SAFETY: the provider handle was initialized by `Fixture::new`.
        unsafe {
            assert_eq!(
                otel_tracer_provider_force_flush(&fixture.provider),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_provider_shutdown(&fixture.provider),
                otel_result_t::OTEL_OK
            );
        }
    }

    #[test]
    fn end_on_release_ends_retained_span() {
        let fixture = Fixture::new();
        let mut span = otel_span_t {
            inner: ptr::null_mut(),
        };
        let mut retained = otel_span_t {
            inner: ptr::null_mut(),
        };
        // SAFETY: both handle slots are writable and aligned; the static C
        // strings and NULL attribute sentinel are valid for the calls.
        unsafe {
            let options = otel_span_options_t {
                size: std::mem::size_of::<otel_span_options_t>(),
                kind: -1,
                start_time_unix_nano: 0,
                parent: ptr::null(),
                end_on_release: 1,
            };
            assert_eq!(
                otel_tracer_span_start_with_options(
                    &fixture.tracer,
                    &options,
                    &mut span,
                    c"deferred".as_ptr(),
                    ptr::null::<otel_attribute_t>()
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
        }
    }

    #[test]
    fn no_exporter_provider_creates_tracer_and_span() {
        let fixture = Fixture::new();
        let mut span = fixture.start(c"created");
        // SAFETY: `span` was initialized by `Fixture::start` with a valid tracer.
        unsafe { otel_span_release(&mut span) };
        assert!(span.inner.is_null());
    }

    #[test]
    fn numeric_ids_round_trip_hex() {
        let trace = otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(trace.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        let span = otel_span_id_t::from_hex("00f067aa0ba902b7").unwrap();
        assert_eq!(span.to_hex(), "00f067aa0ba902b7");
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(unsafe { otel_trace_id_is_valid(&trace) });
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(unsafe { otel_span_id_is_valid(&span) });
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(unsafe { otel_trace_id_equal(&trace, &trace) });
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(unsafe { otel_span_id_equal(&span, &span) });
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(!(unsafe { otel_trace_id_is_valid(&otel_trace_id_t::default()) }));
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        assert!(!(unsafe { otel_span_id_is_valid(&otel_span_id_t::default()) }));
        assert!(otel_trace_id_t::from_hex("42").is_err());
        assert!(otel_span_id_t::from_hex("42").is_err());
    }

    #[test]
    fn sampler_defaults_ratio_and_supports_parentbased_always_off() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_TRACES_SAMPLER");
        env.remove("OTEL_TRACES_SAMPLER_ARG");
        env.set("OTEL_TRACES_SAMPLER", "traceidratio");
        env.set("OTEL_TRACES_SAMPLER_ARG", "   ");
        match sampler_from_env().unwrap() {
            Sampler::TraceIdRatioBased(value) => assert_eq!(value, 1.0),
            other => panic!("unexpected sampler: {other:?}"),
        }
        env.set("OTEL_TRACES_SAMPLER", "parentbased_always_off");
        assert!(matches!(
            sampler_from_env().unwrap(),
            Sampler::ParentBased(_)
        ));
    }

    #[test]
    fn id_parsing_rejects_non_hex_digits() {
        let mut trace = otel_trace_id_t::default();
        let mut span = otel_span_id_t::default();
        // SAFETY: both output slots are writable; the static strings have the
        // required lengths but contain non-hex digits, so they are rejected.
        unsafe {
            assert_eq!(
                otel_trace_id_from_str(c"4bf92f3577b34da6a3ce929d0e0e473z".as_ptr(), &mut trace),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_id_from_str(c"00f067aa0ba902bz".as_ptr(), &mut span),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
        }
        assert!(otel_trace_id_t::from_hex("4bf92f3577b34da6a3ce929d0e0e473z").is_err());
        assert!(otel_span_id_t::from_hex("00f067aa0ba902bz").is_err());
    }

    #[test]
    fn tracer_n_creates_a_tracer_that_starts_spans() {
        let fixture = Fixture::new();
        let mut tracer = otel_tracer_t {
            inner: ptr::null_mut(),
        };
        // SAFETY: all handle slots are writable and aligned; the name views and
        // NULL attribute sentinel stay valid for the duration of each call.
        unsafe {
            assert_eq!(
                otel_tracer_provider_tracer_n(
                    &fixture.provider,
                    &mut tracer,
                    str_view("length-aware-scope")
                ),
                otel_result_t::OTEL_OK
            );
            let mut span = otel_span_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                otel_tracer_span_start(
                    &tracer,
                    &mut span,
                    c"from-tracer-n".as_ptr(),
                    ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut span);
            otel_tracer_release(&mut tracer);
        }
    }

    #[test]
    fn span_status_n_sets_unset_ok_and_rejects_invalid() {
        let fixture = Fixture::new();
        let mut span = fixture.start(c"status-n");
        // SAFETY: `span` is initialized; both descriptions are static views
        // with accurate lengths and the invalid status is rejected before use.
        unsafe {
            assert_eq!(
                otel_span_set_status_n(&span, 0, str_view("")),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status_n(&span, 1, str_view("")),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_set_status_n(&span, 3, str_view("bad")),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
        }
        // SAFETY: `span` was initialized by `Fixture::start`.
        unsafe { otel_span_release(&mut span) };
    }

    #[test]
    fn span_end_timestamp_and_id_outputs_cover_all_paths() {
        let fixture = Fixture::new();
        let mut timestamped = fixture.start(c"timestamped");
        let mut plain = fixture.start(c"plain");
        let mut trace_id = otel_trace_id_t::default();
        let mut span_id = otel_span_id_t::default();
        // SAFETY: both spans are initialized; ID outputs are writable; the
        // negative timestamp is rejected and zero means "end now" to the SDK.
        unsafe {
            assert_eq!(
                otel_span_trace_id(&timestamped, ptr::null_mut()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_span_id(&timestamped, ptr::null_mut()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_trace_id(&timestamped, &mut trace_id),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_span_id(&timestamped, &mut span_id),
                otel_result_t::OTEL_OK
            );
            assert!(otel_trace_id_is_valid(&trace_id));
            assert!(otel_span_id_is_valid(&span_id));
            assert_eq!(
                otel_span_end_with_timestamp(&timestamped, -1),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_span_end_with_timestamp(&timestamped, 1_700_000_000_000_000_000),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_end_with_timestamp(&timestamped, 0),
                otel_result_t::OTEL_OK
            );
            assert_eq!(otel_span_end(&plain), otel_result_t::OTEL_OK);
            assert_eq!(otel_span_end(&plain), otel_result_t::OTEL_OK);
            // A span context remains readable after the span has ended.
            assert_eq!(
                otel_span_trace_id(&timestamped, &mut trace_id),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_span_span_id(&timestamped, &mut span_id),
                otel_result_t::OTEL_OK
            );
            otel_span_release(&mut timestamped);
            otel_span_release(&mut plain);
        }
    }

    #[test]
    fn provider_builds_batch_processors_with_valid_delay() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_SDK_DISABLED");
        env.remove("OTEL_TRACES_SAMPLER");
        env.remove("OTEL_TRACES_SAMPLER_ARG");
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL");
        env.set("OTEL_BSP_SCHEDULE_DELAY", "60000");
        let mut runtime = new_runtime();
        for name in [config::STDOUT_TRANSPORT, config::OTLP_HTTP_TRANSPORT] {
            let descriptor = otel_transport_t {
                size: std::mem::size_of::<otel_transport_t>(),
                name: str_view(name),
                send: noop_transport_send,
                user_data: ptr::null_mut(),
            };
            assert_eq!(
                // SAFETY: `runtime` is initialized and `descriptor` has a valid
                // size, a static name view, and a no-op callback.
                unsafe { otel_runtime_set_transport(&mut runtime, &descriptor) },
                otel_result_t::OTEL_OK
            );
        }
        for exporter in ["stdout", "otlp"] {
            env.set("OTEL_TRACES_EXPORTER", exporter);
            let mut provider = otel_tracer_provider_t {
                inner: ptr::null_mut(),
            };
            assert_eq!(
                // SAFETY: both handles are writable and aligned; the runtime is
                // initialized and carries the callback transports registered above.
                unsafe { otel_tracer_provider_new(&mut provider, ptr::null(), &runtime) },
                otel_result_t::OTEL_OK
            );
            // SAFETY: `provider` was initialized by the successful call above.
            unsafe { otel_tracer_provider_release(&mut provider) };
        }

        env.set("OTEL_TRACES_EXPORTER", "item");
        let mut provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: both handles are writable and aligned; no item transport
            // was registered, so the configuration is rejected.
            unsafe { otel_tracer_provider_new(&mut provider, ptr::null(), &runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());
        // SAFETY: `runtime` was initialized by `new_runtime` and is released once.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn exporter_builders_use_registered_callback_transports() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL");
        let mut runtime = new_runtime();
        for name in [
            config::STDOUT_TRANSPORT,
            config::OTLP_HTTP_TRANSPORT,
            config::OTLP_GRPC_TRANSPORT,
        ] {
            let descriptor = otel_transport_t {
                size: std::mem::size_of::<otel_transport_t>(),
                name: str_view(name),
                send: noop_transport_send,
                user_data: ptr::null_mut(),
            };
            assert_eq!(
                // SAFETY: `runtime` is initialized and `descriptor` has a valid
                // size, a static name view, and a no-op callback.
                unsafe { otel_runtime_set_transport(&mut runtime, &descriptor) },
                otel_result_t::OTEL_OK
            );
        }

        // SAFETY: `runtime` was initialized by `new_runtime`; `arc` increments
        // its strong count and the returned Arc keeps it alive for these calls.
        let registry = unsafe { arc::<RuntimeInner>(const_slot(&runtime)) }.unwrap();
        assert!(build_stdout_exporter(&registry).is_ok());
        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc");
        assert!(build_otlp_exporter(&registry).is_ok());
        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/protobuf");
        assert!(build_otlp_exporter(&registry).is_ok());
        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json");
        assert!(build_otlp_exporter(&registry).is_ok());
        drop(registry);
        // SAFETY: `runtime` was initialized by `new_runtime` and is released once.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn provider_exporter_paths_cover_item_and_reject_invalid_delay() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_SDK_DISABLED");
        env.remove("OTEL_BSP_SCHEDULE_DELAY");
        env.remove("OTEL_TRACES_SAMPLER");
        env.remove("OTEL_TRACES_SAMPLER_ARG");
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL");
        let mut runtime = new_runtime();

        let item_transport = otel_item_transport_t {
            size: std::mem::size_of::<otel_item_transport_t>(),
            on_span: None,
            on_log: None,
            on_metric: None,
            on_encoded: Some(noop_encoded_item),
            user_data: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `runtime` is initialized and `item_transport` is a fully
            // initialized descriptor with a no-op encoded callback.
            unsafe { otel_runtime_set_item_transport(&mut runtime, &item_transport) },
            otel_result_t::OTEL_OK
        );
        for name in [config::STDOUT_TRANSPORT, config::OTLP_HTTP_TRANSPORT] {
            let descriptor = otel_transport_t {
                size: std::mem::size_of::<otel_transport_t>(),
                name: str_view(name),
                send: noop_transport_send,
                user_data: ptr::null_mut(),
            };
            assert_eq!(
                // SAFETY: `runtime` is initialized and `descriptor` has a valid
                // size, a static name view, and a no-op callback.
                unsafe { otel_runtime_set_transport(&mut runtime, &descriptor) },
                otel_result_t::OTEL_OK
            );
        }

        env.set("OTEL_TRACES_EXPORTER", "item");
        let mut item_provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: both handles are writable and aligned; the runtime is
            // initialized and carries the item transport registered above.
            unsafe { otel_tracer_provider_new(&mut item_provider, ptr::null(), &runtime) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: `item_provider` was initialized by the successful call above.
        unsafe { otel_tracer_provider_release(&mut item_provider) };

        env.set("OTEL_TRACES_EXPORTER", "stdout");
        env.set("OTEL_BSP_SCHEDULE_DELAY", "not-a-number");
        let mut stdout_provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: the handle is writable and aligned; the invalid delay is
            // rejected before a batch processor is built.
            unsafe { otel_tracer_provider_new(&mut stdout_provider, ptr::null(), &runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(stdout_provider.inner.is_null());

        env.set("OTEL_TRACES_EXPORTER", "otlp");
        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json");
        let mut otlp_provider = otel_tracer_provider_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: the handle is writable and aligned; the invalid delay is
            // rejected before a batch processor is built.
            unsafe { otel_tracer_provider_new(&mut otlp_provider, ptr::null(), &runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(otlp_provider.inner.is_null());

        // SAFETY: `runtime` was initialized by `new_runtime` and is released once.
        unsafe { otel_runtime_release(&mut runtime) };
    }
}
