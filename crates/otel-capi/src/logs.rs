//! Logger provider, logger, and log record APIs.

use std::ffi::{VaList, c_char};
use std::sync::Arc;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::{KeyValue, Value};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::{BatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider};

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config;
use crate::config::{
    OtlpProtocol, build_error, duration_ms, exporter_kind, otlp_protocol, resource_from_env,
    sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, otel_result_t, utf8};
use crate::handles::{
    LoggerInner, LoggerProviderInner, RuntimeInner, arc, const_slot, init, optional_resource,
    optional_runtime, slot,
};
use crate::handles::{otel_logger_provider_t, otel_logger_t, otel_runtime_t, otel_span_t};
use crate::item_export::ItemLogProcessor;
use crate::strings::{otel_str_t, str_slice};
use crate::transport::callback_client;
use crate::util::{interned, system_time};

/// Log severity exported to C. Values match the OpenTelemetry severity scale.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_severity_t {
    /// Trace-level detail.
    OTEL_SEVERITY_TRACE = 1,
    /// Debug-level detail.
    OTEL_SEVERITY_DEBUG = 5,
    /// Informational message.
    OTEL_SEVERITY_INFO = 9,
    /// Warning.
    OTEL_SEVERITY_WARN = 13,
    /// Error.
    OTEL_SEVERITY_ERROR = 17,
    /// Fatal error.
    OTEL_SEVERITY_FATAL = 21,
}

/// Optional fields for a structured log record. Initialize with
/// `OTEL_LOG_RECORD_OPTIONS_INIT` and set `size` to
/// `sizeof(otel_log_record_options_t)`.
#[repr(C)]
pub struct otel_log_record_options_t {
    pub size: usize,
    /// One of `otel_severity_t`.
    pub level: i32,
    /// Event name; interned for the process lifetime.
    pub event_name: *const c_char,
    /// Instrumentation target. May be NULL.
    pub target: *const c_char,
    /// Severity text override; interned for the process lifetime. May be NULL.
    pub severity_text: *const c_char,
    /// Event Unix timestamp in nanoseconds; zero means unset.
    pub timestamp_unix_nano: i64,
    /// Observed Unix timestamp in nanoseconds; zero means unset.
    pub observed_timestamp_unix_nano: i64,
    /// Optional span used for trace correlation.
    pub span: *const otel_span_t,
}

unsafe fn emit_log_with_attributes(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: String,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    if options.is_null() {
        return Err(Error::invalid("log options must not be NULL"));
    }
    // SAFETY: The caller guarantees the pointer is non-null, aligned, and references an initialized value.
    let options = unsafe { &*options };
    if options.size < std::mem::size_of::<otel_log_record_options_t>() {
        return Err(Error::invalid(
            "otel_log_record_options_t.size is too small",
        ));
    }
    // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
    let logger = unsafe { arc::<LoggerInner>(const_slot(logger)) }?;
    let (number, default_text) = severity(options.level)?;
    let timestamp = system_time(options.timestamp_unix_nano)?;
    let observed = system_time(options.observed_timestamp_unix_nano)?;

    let mut record = logger.create_log_record();
    record.set_severity_number(number);
    record.set_severity_text(
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        match unsafe { crate::util::optional_str(options.severity_text, "severity text")? } {
            Some(text) => interned(text.to_owned()),
            None => default_text,
        },
    );
    record.set_body(AnyValue::String(body.into()));
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    if let Some(name) = unsafe { crate::util::optional_str(options.event_name, "event name")? } {
        record.set_event_name(interned(name.to_owned()));
    }
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    if let Some(target) = unsafe { crate::util::optional_str(options.target, "target")? } {
        record.set_target(target.to_owned());
    }
    if let Some(timestamp) = timestamp {
        record.set_timestamp(timestamp);
    }
    if let Some(observed) = observed {
        record.set_observed_timestamp(observed);
    }
    for attribute in attributes {
        record.add_attribute(attribute.key.to_string(), any_value(attribute.value));
    }
    if !options.span.is_null() {
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let context = unsafe { crate::trace::span_context(options.span)? };
        record.set_trace_context(
            context.trace_id(),
            context.span_id(),
            Some(context.trace_flags()),
        );
    }
    logger.emit(record);
    Ok(())
}

unsafe fn emit_log(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: String,
    args: VaList,
) -> Result<()> {
    // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
    let attributes = unsafe { from_varargs(args) }?;
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { emit_log_with_attributes(logger, options, body, attributes) }
}

unsafe fn emit_log_arr(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: String,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> Result<()> {
    // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
    let attributes = unsafe { from_array(attributes, count) }?;
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { emit_log_with_attributes(logger, options, body, attributes) }
}

fn severity(value: i32) -> Result<(Severity, &'static str)> {
    match value {
        1 => Ok((Severity::Trace, "TRACE")),
        5 => Ok((Severity::Debug, "DEBUG")),
        9 => Ok((Severity::Info, "INFO")),
        13 => Ok((Severity::Warn, "WARN")),
        17 => Ok((Severity::Error, "ERROR")),
        21 => Ok((Severity::Fatal, "FATAL")),
        other => Err(Error::invalid(format!("invalid severity {other}"))),
    }
}

fn any_value(value: Value) -> AnyValue {
    match value {
        Value::Bool(value) => AnyValue::Boolean(value),
        Value::I64(value) => AnyValue::Int(value),
        Value::F64(value) => AnyValue::Double(value),
        Value::String(value) => AnyValue::String(value),
        other => AnyValue::String(other.to_string().into()),
    }
}

fn build_otlp_exporter(runtime: &Arc<RuntimeInner>) -> Result<opentelemetry_otlp::LogExporter> {
    let protocol = otlp_protocol("LOGS")?;
    let builder = opentelemetry_otlp::LogExporter::builder();
    let exporter = match protocol {
        OtlpProtocol::Grpc => builder
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_http_client(callback_client(
                runtime,
                config::OTLP_GRPC_TRANSPORT,
                "LOGS",
            )?)
            .build(),
        OtlpProtocol::HttpBinary | OtlpProtocol::HttpJson => builder
            .with_http()
            .with_protocol(protocol.into())
            .with_http_client(callback_client(
                runtime,
                config::OTLP_HTTP_TRANSPORT,
                "LOGS",
            )?)
            .build(),
    };
    exporter.map_err(|err| build_error("failed to build OTLP log exporter", err))
}

fn build_stdout_exporter(runtime: &Arc<RuntimeInner>) -> Result<opentelemetry_otlp::LogExporter> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_http_client(callback_client(runtime, config::STDOUT_TRANSPORT, "LOGS")?)
        .build();
    exporter.map_err(|err| build_error("failed to build stdout log exporter", err))
}

fn build_provider(
    resource: Option<crate::handles::ResourceInner>,
    runtime: Option<Arc<RuntimeInner>>,
) -> Result<LoggerProviderInner> {
    let resource = resource.unwrap_or_else(resource_from_env);
    let mut builder = SdkLoggerProvider::builder().with_resource(resource);

    if !sdk_disabled() {
        match exporter_kind("LOGS")? {
            crate::config::ExporterKind::None => {}
            crate::config::ExporterKind::Stdout => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "stdout requires an otel_runtime_t; pass one to otel_logger_provider_new",
                    )
                })?;
                let exporter = build_stdout_exporter(runtime)?;
                let delay = duration_ms("OTEL_BLRP_SCHEDULE_DELAY", 1_000)?;
                let config = BatchConfigBuilder::default()
                    .with_scheduled_delay(delay)
                    .build();
                builder = builder.with_log_processor(
                    BatchLogProcessor::builder(exporter)
                        .with_batch_config(config)
                        .build(),
                );
            }
            crate::config::ExporterKind::Otlp => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "OTLP requires an otel_runtime_t; pass one to otel_logger_provider_new",
                    )
                })?;
                let exporter = build_otlp_exporter(runtime)?;
                let delay = duration_ms("OTEL_BLRP_SCHEDULE_DELAY", 1_000)?;
                let config = BatchConfigBuilder::default()
                    .with_scheduled_delay(delay)
                    .build();
                builder = builder.with_log_processor(
                    BatchLogProcessor::builder(exporter)
                        .with_batch_config(config)
                        .build(),
                );
            }
            crate::config::ExporterKind::Item => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires an otel_runtime_t; pass one to otel_logger_provider_new",
                    )
                })?;
                let transport = runtime.item_transport()?.ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "item exporter requires otel_runtime_set_item_transport",
                    )
                })?;
                builder = builder.with_log_processor(ItemLogProcessor::new(transport));
            }
        }
    }
    Ok(LoggerProviderInner {
        provider: builder.build(),
        runtime,
    })
}

/// Creates a logger provider. `resource` and `runtime` may be NULL when the
/// selected exporter does not need them.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_new(
    handle: *mut otel_logger_provider_t,
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

/// Flushes all pending log records.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_force_flush(
    provider: *const otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush logs", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its processors.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_shutdown(
    provider: *const otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down logger provider", err))?;
        Ok(())
    })
}

/// Creates a logger for an instrumentation scope.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_logger(
    provider: *const otel_logger_provider_t,
    handle: *mut otel_logger_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = utf8(unsafe { c_str(name, "logger name")? }, "logger name")?;
        // The scope name is retained by the provider for its lifetime.
        let name = interned(name);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.logger(name)) }?;
        Ok(())
    })
}

/// Length-aware variant of [`otel_logger_provider_logger`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_logger_n(
    provider: *const otel_logger_provider_t,
    handle: *mut otel_logger_t,
    name: otel_str_t,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the handle is initialized; `arc` increments the strong count before taking ownership.
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let name = unsafe { str_slice(name, "logger name")? }.to_owned();
        // The scope name is retained by the provider for its lifetime.
        let name = interned(name);
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), provider.provider.logger(name)) }?;
        Ok(())
    })
}

/// Emits a log record with optional attributes. `span` may be NULL; when set,
/// the record carries that span's trace and span IDs.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log(
    logger: *const otel_logger_t,
    span: *const otel_span_t,
    level: i32,
    body: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let options = otel_log_record_options_t {
            size: std::mem::size_of::<otel_log_record_options_t>(),
            level,
            event_name: std::ptr::null(),
            target: std::ptr::null(),
            severity_text: std::ptr::null(),
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            span,
        };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log(logger, &options, body, attributes) }
    })
}

/// Array-based variant of [`otel_logger_log`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_arr(
    logger: *const otel_logger_t,
    span: *const otel_span_t,
    level: i32,
    body: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let options = otel_log_record_options_t {
            size: std::mem::size_of::<otel_log_record_options_t>(),
            level,
            event_name: std::ptr::null(),
            target: std::ptr::null(),
            severity_text: std::ptr::null(),
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            span,
        };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log_arr(logger, &options, body, attributes, count) }
    })
}

/// Emits a structured log record with event name, target, timestamps, and
/// optional span correlation configured through `options`.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log(logger, options, body, attributes) }
    })
}

/// Array-based variant of [`otel_logger_log_record`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record_arr(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log_arr(logger, options, body, attributes, count) }
    })
}

/// Length-aware variant of [`otel_logger_log`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_n(
    logger: *const otel_logger_t,
    span: *const otel_span_t,
    level: i32,
    body: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let options = otel_log_record_options_t {
            size: std::mem::size_of::<otel_log_record_options_t>(),
            level,
            event_name: std::ptr::null(),
            target: std::ptr::null(),
            severity_text: std::ptr::null(),
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            span,
        };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = unsafe { str_slice(body, "log body")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log(logger, &options, body, attributes) }
    })
}

/// Length-aware variant of [`otel_logger_log_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_arr_n(
    logger: *const otel_logger_t,
    span: *const otel_span_t,
    level: i32,
    body: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let options = otel_log_record_options_t {
            size: std::mem::size_of::<otel_log_record_options_t>(),
            level,
            event_name: std::ptr::null(),
            target: std::ptr::null(),
            severity_text: std::ptr::null(),
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            span,
        };
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = unsafe { str_slice(body, "log body")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log_arr(logger, &options, body, attributes, count) }
    })
}

/// Length-aware variant of [`otel_logger_log_record`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record_n(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: otel_str_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = unsafe { str_slice(body, "log body")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log(logger, options, body, attributes) }
    })
}

/// Length-aware variant of [`otel_logger_log_record_arr`].
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record_arr_n(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: otel_str_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let body = unsafe { str_slice(body, "log body")? }.to_owned();
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { emit_log_arr(logger, options, body, attributes, count) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::{otel_attribute_t, otel_attribute_type_t, otel_attribute_value_t};
    use crate::config::tests::EnvGuard;
    use crate::handles::{otel_logger_provider_release, otel_logger_release, otel_logger_retain};

    fn options(level: i32) -> otel_log_record_options_t {
        otel_log_record_options_t {
            size: std::mem::size_of::<otel_log_record_options_t>(),
            level,
            event_name: std::ptr::null(),
            target: std::ptr::null(),
            severity_text: std::ptr::null(),
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            span: std::ptr::null(),
        }
    }

    #[test]
    fn rejects_invalid_severity() {
        let error = severity(999).unwrap_err();
        assert_eq!(
            error.status,
            crate::error::otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn severity_maps_every_level() {
        for (value, expected) in [
            (1, Severity::Trace),
            (5, Severity::Debug),
            (9, Severity::Info),
            (13, Severity::Warn),
            (17, Severity::Error),
            (21, Severity::Fatal),
        ] {
            let (number, text) = severity(value).unwrap();
            assert_eq!(number, expected);
            assert!(!text.is_empty());
        }
        assert_eq!(severity(9).unwrap().1, "INFO");
        assert!(severity(0).is_err());
        assert!(severity(22).is_err());
    }

    #[test]
    fn any_value_converts_all_attribute_types() {
        assert_eq!(any_value(Value::Bool(true)), AnyValue::Boolean(true));
        assert_eq!(any_value(Value::I64(-2)), AnyValue::Int(-2));
        assert_eq!(any_value(Value::F64(2.5)), AnyValue::Double(2.5));
        assert_eq!(
            any_value(Value::String("text".into())),
            AnyValue::String("text".into())
        );
        assert_eq!(
            any_value(Value::Array(opentelemetry::Array::Bool(vec![true]))),
            AnyValue::String("[true]".into())
        );
    }

    #[test]
    fn emit_with_attributes_validates_inputs() {
        let mut short = options(9);
        short.size = 1;
        assert_eq!(
            // SAFETY: NULL options are an intentionally invalid input.
            unsafe {
                emit_log_with_attributes(std::ptr::null(), std::ptr::null(), "x".into(), vec![])
            }
            .unwrap_err()
            .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: `short` is a valid local struct with an intentionally small size.
            unsafe { emit_log_with_attributes(std::ptr::null(), &short, "x".into(), vec![]) }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        let valid = options(9);
        assert_eq!(
            // SAFETY: NULL logger handle is an intentionally invalid handle input.
            unsafe { emit_log_with_attributes(std::ptr::null(), &valid, "x".into(), vec![]) }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn provider_requires_runtime_for_otlp_exporter() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "otlp");
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `provider` is writable, aligned storage; NULL arguments are validated.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());
    }

    #[test]
    fn no_exporter_logger_emits() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "none");
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe {
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"test".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log(
                    &logger,
                    std::ptr::null(),
                    otel_severity_t::OTEL_SEVERITY_INFO as i32,
                    c"hello".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            let key = c"key";
            let value = c"value";
            let attribute = otel_attribute_t {
                kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
                key: key.as_ptr(),
                key_len: key.to_bytes().len(),
                value: otel_attribute_value_t {
                    string: value.as_ptr(),
                },
                value_len: value.to_bytes().len(),
            };
            let options = otel_log_record_options_t {
                size: std::mem::size_of::<otel_log_record_options_t>(),
                level: otel_severity_t::OTEL_SEVERITY_WARN as i32,
                event_name: c"test.event".as_ptr(),
                target: c"test.target".as_ptr(),
                severity_text: c"CRITICAL".as_ptr(),
                timestamp_unix_nano: 1,
                observed_timestamp_unix_nano: 2,
                span: std::ptr::null(),
            };
            assert_eq!(
                otel_logger_log_record(
                    &logger,
                    &options,
                    c"structured".as_ptr(),
                    &attribute,
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log_record(
                    &logger,
                    std::ptr::null(),
                    c"structured".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
        }
    }

    #[test]
    fn log_variants_and_lifecycle() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "none");
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };
        let mut retained = otel_logger_t {
            inner: std::ptr::null_mut(),
        };
        // SAFETY: all handles below are local, writable storage; borrowed C
        // strings are static literals that outlive the calls.
        unsafe {
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_force_flush(&provider),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_shutdown(&provider),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger_n(
                    &provider,
                    &mut logger,
                    otel_str_t {
                        ptr: c"scoped".as_ptr(),
                        len: 6,
                    },
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_retain(&logger, &mut retained),
                otel_result_t::OTEL_OK
            );

            let key = c"k";
            let value = c"v";
            let attribute = otel_attribute_t {
                kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
                key: key.as_ptr(),
                key_len: 1,
                value: otel_attribute_value_t {
                    string: value.as_ptr(),
                },
                value_len: 1,
            };
            let attributes = [&attribute as *const otel_attribute_t];
            let level = otel_severity_t::OTEL_SEVERITY_DEBUG as i32;
            assert_eq!(
                otel_logger_log_arr(
                    &logger,
                    std::ptr::null(),
                    level,
                    c"arr".as_ptr(),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log_n(
                    &logger,
                    std::ptr::null(),
                    level,
                    otel_str_t {
                        ptr: c"len".as_ptr(),
                        len: 3
                    },
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log_arr_n(
                    &logger,
                    std::ptr::null(),
                    level,
                    otel_str_t {
                        ptr: c"len-arr".as_ptr(),
                        len: 7
                    },
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            let opts = options(otel_severity_t::OTEL_SEVERITY_ERROR as i32);
            assert_eq!(
                otel_logger_log_record_arr(
                    &logger,
                    &opts,
                    c"record-arr".as_ptr(),
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log_record_n(
                    &logger,
                    &opts,
                    otel_str_t {
                        ptr: c"record-n".as_ptr(),
                        len: 8
                    },
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log_record_arr_n(
                    &logger,
                    &opts,
                    otel_str_t {
                        ptr: c"record-arr-n".as_ptr(),
                        len: 12
                    },
                    attributes.as_ptr(),
                    1
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log(
                    &logger,
                    std::ptr::null(),
                    42,
                    c"bad".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_n(
                    &logger,
                    std::ptr::null(),
                    level,
                    otel_str_t {
                        ptr: std::ptr::null(),
                        len: 3
                    },
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            otel_logger_release(&mut logger);
            assert_eq!(
                otel_logger_log(
                    &retained,
                    std::ptr::null(),
                    level,
                    c"still".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_logger_release(&mut retained);
            otel_logger_provider_release(&mut provider);
        }
    }

    use crate::abi::otel_transport_t;
    use crate::handles::{
        otel_resource_release, otel_resource_t, otel_runtime_release, otel_span_release,
        otel_tracer_provider_release, otel_tracer_provider_t, otel_tracer_release, otel_tracer_t,
    };
    use crate::items::{otel_item_transport_t, otel_log_item_t};
    use crate::resource::otel_resource_new;
    use crate::runtime::{
        otel_runtime_new, otel_runtime_set_item_transport, otel_runtime_set_transport,
    };
    use crate::trace::{
        otel_tracer_provider_new, otel_tracer_provider_tracer, otel_tracer_span_start,
    };
    use crate::transport::Transport;
    use std::ffi::{CStr, c_void};

    unsafe extern "C" fn noop_transport_send(
        _request: *const crate::abi::otel_transport_request_t,
        _response: *mut crate::abi::otel_transport_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn noop_log_item(_item: *const otel_log_item_t, _user_data: *mut c_void) {}

    fn new_runtime_handle() -> otel_runtime_t {
        let mut runtime = otel_runtime_t {
            inner: std::ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `runtime` is writable, aligned storage for one handle pointer.
            unsafe { otel_runtime_new(&mut runtime) },
            otel_result_t::OTEL_OK
        );
        runtime
    }

    fn register_transport(runtime: &mut otel_runtime_t, name: &CStr) {
        let descriptor = otel_transport_t {
            size: std::mem::size_of::<otel_transport_t>(),
            name: otel_str_t {
                ptr: name.as_ptr(),
                len: name.to_bytes().len(),
            },
            send: noop_transport_send,
            user_data: std::ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `runtime` is initialized and `descriptor` is valid for this call.
            unsafe { otel_runtime_set_transport(runtime, &descriptor) },
            otel_result_t::OTEL_OK
        );
    }

    fn register_log_item_transport(runtime: &mut otel_runtime_t) {
        let transport = otel_item_transport_t {
            size: std::mem::size_of::<otel_item_transport_t>(),
            on_span: None,
            on_log: Some(noop_log_item),
            on_metric: None,
            on_encoded: None,
            user_data: std::ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: `runtime` is initialized and `transport` is valid for this call.
            unsafe { otel_runtime_set_item_transport(runtime, &transport) },
            otel_result_t::OTEL_OK
        );
    }

    fn runtime_inner_with_transport(name: &str) -> Arc<RuntimeInner> {
        let runtime = Arc::new(RuntimeInner::new());
        runtime
            .set_transport(
                name.to_owned(),
                Transport::new(noop_transport_send, std::ptr::null_mut()),
            )
            .unwrap();
        runtime
    }

    #[test]
    fn otlp_log_exporter_builder_handles_each_protocol() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT");
        env.remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        let grpc = runtime_inner_with_transport("grpc");
        let http = runtime_inner_with_transport("http");

        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "grpc");
        assert!(build_otlp_exporter(&grpc).is_ok());
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/protobuf");
        assert!(build_otlp_exporter(&http).is_ok());
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json");
        assert!(build_otlp_exporter(&http).is_ok());

        let missing = Arc::new(RuntimeInner::new());
        assert_eq!(
            build_otlp_exporter(&missing).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "bogus");
        assert_eq!(
            build_otlp_exporter(&http).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json");
        env.set("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "not a valid URI");
        assert_eq!(
            build_otlp_exporter(&http).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
    }

    #[test]
    fn stdout_log_exporter_builder_handles_success_and_errors() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT");
        env.remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        let stdout = runtime_inner_with_transport("stdout");

        assert!(build_stdout_exporter(&stdout).is_ok());
        let missing = Arc::new(RuntimeInner::new());
        assert_eq!(
            build_stdout_exporter(&missing).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.set("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "not a valid URI");
        assert_eq!(
            build_stdout_exporter(&stdout).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
    }

    #[test]
    fn provider_exporter_branches_report_config_errors() {
        let mut env = EnvGuard::new();
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };

        env.set("OTEL_LOGS_EXPORTER", "stdout");
        assert_eq!(
            // SAFETY: `provider` is writable storage; NULL arguments are validated.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());

        env.set("OTEL_LOGS_EXPORTER", "item");
        assert_eq!(
            // SAFETY: `provider` is writable storage; NULL arguments are validated.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());

        env.set("OTEL_LOGS_EXPORTER", "stdout");
        env.set("OTEL_BLRP_SCHEDULE_DELAY", "bad");
        let mut stdout_runtime = new_runtime_handle();
        register_transport(&mut stdout_runtime, c"stdout");
        assert_eq!(
            // SAFETY: both handles are initialized and alive for this call.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), &stdout_runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());

        env.remove("OTEL_BLRP_SCHEDULE_DELAY");
        env.set("OTEL_LOGS_EXPORTER", "item");
        let mut bare_runtime = new_runtime_handle();
        assert_eq!(
            // SAFETY: both handles are initialized and alive for this call.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), &bare_runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());

        env.set("OTEL_LOGS_EXPORTER", "otlp");
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/protobuf");
        env.set("OTEL_BLRP_SCHEDULE_DELAY", "bad");
        let mut http_runtime = new_runtime_handle();
        register_transport(&mut http_runtime, c"http");
        assert_eq!(
            // SAFETY: both handles are initialized and alive for this call.
            unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), &http_runtime) },
            otel_result_t::OTEL_ERR_CONFIG
        );
        assert!(provider.inner.is_null());

        // SAFETY: every runtime handle was initialized by `otel_runtime_new`.
        unsafe {
            otel_runtime_release(&mut stdout_runtime);
            otel_runtime_release(&mut bare_runtime);
            otel_runtime_release(&mut http_runtime);
            otel_logger_provider_release(&mut provider);
        }
    }

    #[test]
    fn provider_builds_batch_processors_with_valid_delay() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_SDK_DISABLED");
        env.remove("OTEL_BLRP_SCHEDULE_DELAY");
        env.remove("OTEL_EXPORTER_OTLP_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL");
        env.remove("OTEL_EXPORTER_OTLP_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_COMPRESSION");
        env.remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        env.remove("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT");
        // A long delay keeps the worker idle for the duration of the test.
        env.set("OTEL_BLRP_SCHEDULE_DELAY", "60000");
        env.set("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/protobuf");
        let mut runtime = new_runtime_handle();
        register_transport(&mut runtime, c"stdout");
        register_transport(&mut runtime, c"http");

        for exporter in ["stdout", "otlp"] {
            env.set("OTEL_LOGS_EXPORTER", exporter);
            let mut provider = otel_logger_provider_t {
                inner: std::ptr::null_mut(),
            };
            assert_eq!(
                // SAFETY: both handles are initialized and alive for this call.
                unsafe { otel_logger_provider_new(&mut provider, std::ptr::null(), &runtime) },
                otel_result_t::OTEL_OK
            );
            // SAFETY: `provider` was initialized by the successful call above.
            unsafe { otel_logger_provider_release(&mut provider) };
        }

        // SAFETY: `runtime` was initialized by `new_runtime_handle`.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn item_exporter_provider_builds_without_explicit_resource() {
        let mut env = EnvGuard::new();
        env.remove("OTEL_SDK_DISABLED");
        env.remove("OTEL_RESOURCE_ATTRIBUTES");
        env.remove("OTEL_SERVICE_NAME");
        env.set("OTEL_LOGS_EXPORTER", "item");
        let mut runtime = new_runtime_handle();
        register_log_item_transport(&mut runtime);
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };

        // SAFETY: all handles are local, writable storage and the strings are static.
        unsafe {
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), &runtime),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"item-implicit".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log(
                    &logger,
                    std::ptr::null(),
                    otel_severity_t::OTEL_SEVERITY_INFO as i32,
                    c"item body".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_shutdown(&provider),
                otel_result_t::OTEL_OK
            );
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
        }
        // SAFETY: `runtime` was initialized by `new_runtime_handle`.
        unsafe { otel_runtime_release(&mut runtime) };
    }

    #[test]
    fn item_exporter_provider_emits_without_background_threads() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "item");
        let mut runtime = new_runtime_handle();
        register_log_item_transport(&mut runtime);
        let mut resource = otel_resource_t {
            inner: std::ptr::null_mut(),
        };
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };

        // SAFETY: handles are local, writable storage and all borrowed strings are static.
        unsafe {
            assert_eq!(otel_resource_new(&mut resource), otel_result_t::OTEL_OK);
            assert_eq!(
                otel_logger_provider_new(&mut provider, &resource, &runtime),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"item-scope".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_log(
                    &logger,
                    std::ptr::null(),
                    otel_severity_t::OTEL_SEVERITY_INFO as i32,
                    c"item body".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_force_flush(&provider),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_shutdown(&provider),
                otel_result_t::OTEL_OK
            );
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
            otel_resource_release(&mut resource);
            otel_runtime_release(&mut runtime);
        }
    }

    #[test]
    fn log_record_with_span_carries_trace_context() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "none");
        env.set("OTEL_TRACES_EXPORTER", "none");
        let mut tracer_provider = otel_tracer_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut tracer = otel_tracer_t {
            inner: std::ptr::null_mut(),
        };
        let mut span = otel_span_t {
            inner: std::ptr::null_mut(),
        };
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };

        // SAFETY: handles are local, writable storage and borrowed strings are static.
        unsafe {
            assert_eq!(
                otel_tracer_provider_new(&mut tracer_provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_provider_tracer(&tracer_provider, &mut tracer, c"correlation".as_ptr()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracer_span_start(
                    &tracer,
                    &mut span,
                    c"work".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"correlation".as_ptr()),
                otel_result_t::OTEL_OK
            );
            let mut correlated = options(otel_severity_t::OTEL_SEVERITY_INFO as i32);
            correlated.span = &span;
            assert_eq!(
                otel_logger_log_record(
                    &logger,
                    &correlated,
                    c"correlated".as_ptr(),
                    std::ptr::null::<otel_attribute_t>()
                ),
                otel_result_t::OTEL_OK
            );
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
            otel_span_release(&mut span);
            otel_tracer_release(&mut tracer);
            otel_tracer_provider_release(&mut tracer_provider);
        }
    }

    #[test]
    fn log_variants_reject_invalid_handles_and_bodies() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "none");
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };

        // SAFETY: handles are local, writable storage and borrowed strings are static.
        unsafe {
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"invalid-input".as_ptr()),
                otel_result_t::OTEL_OK
            );

            let invalid_utf8 = b"\xff\0";
            let bad_view = otel_str_t {
                ptr: invalid_utf8.as_ptr().cast(),
                len: 1,
            };
            let no_attrs = std::ptr::null::<otel_attribute_t>();
            let no_attr_array = std::ptr::null::<*const otel_attribute_t>();
            let opts = options(otel_severity_t::OTEL_SEVERITY_INFO as i32);
            let null_logger = std::ptr::null::<otel_logger_t>();

            assert_eq!(
                otel_logger_log(null_logger, std::ptr::null(), 9, c"x".as_ptr(), no_attrs),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_arr(
                    null_logger,
                    std::ptr::null(),
                    9,
                    c"x".as_ptr(),
                    no_attr_array,
                    0
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_n(
                    null_logger,
                    std::ptr::null(),
                    9,
                    otel_str_t {
                        ptr: c"x".as_ptr(),
                        len: 1
                    },
                    no_attrs
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_arr_n(
                    null_logger,
                    std::ptr::null(),
                    9,
                    otel_str_t {
                        ptr: c"x".as_ptr(),
                        len: 1
                    },
                    no_attr_array,
                    0
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record(null_logger, &opts, c"x".as_ptr(), no_attrs),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_arr(null_logger, &opts, c"x".as_ptr(), no_attr_array, 0),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_n(
                    null_logger,
                    &opts,
                    otel_str_t {
                        ptr: c"x".as_ptr(),
                        len: 1
                    },
                    no_attrs
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_arr_n(
                    null_logger,
                    &opts,
                    otel_str_t {
                        ptr: c"x".as_ptr(),
                        len: 1
                    },
                    no_attr_array,
                    0
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );

            assert_eq!(
                otel_logger_log(&logger, std::ptr::null(), 9, std::ptr::null(), no_attrs),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_arr(
                    &logger,
                    std::ptr::null(),
                    9,
                    std::ptr::null(),
                    no_attr_array,
                    0
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_n(&logger, std::ptr::null(), 9, bad_view, no_attrs),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_log_arr_n(&logger, std::ptr::null(), 9, bad_view, no_attr_array, 0),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_log_record(&logger, &opts, std::ptr::null(), no_attrs),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_arr(&logger, &opts, std::ptr::null(), no_attr_array, 0),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_n(&logger, &opts, bad_view, no_attrs),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_log_record_arr_n(&logger, &opts, bad_view, no_attr_array, 0),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );

            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
        }
    }

    #[test]
    fn log_record_options_reject_invalid_fields() {
        let mut env = EnvGuard::new();
        env.set("OTEL_LOGS_EXPORTER", "none");
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };
        let mut other = otel_logger_t {
            inner: std::ptr::null_mut(),
        };

        // SAFETY: handles are local, writable storage and borrowed strings are static.
        unsafe {
            assert_eq!(
                otel_logger_provider_new(&mut provider, std::ptr::null(), std::ptr::null()),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut logger, c"options".as_ptr()),
                otel_result_t::OTEL_OK
            );

            let invalid_utf8 = b"\xff\0";
            let bad_view = otel_str_t {
                ptr: invalid_utf8.as_ptr().cast(),
                len: 1,
            };
            let no_attrs = std::ptr::null::<otel_attribute_t>();
            let no_attr_array = std::ptr::null::<*const otel_attribute_t>();
            let mut short = options(9);
            short.size = 1;
            let bad_level = options(0);
            let mut negative_time = options(9);
            negative_time.timestamp_unix_nano = -1;
            let mut bad_severity_text = options(9);
            bad_severity_text.severity_text = invalid_utf8.as_ptr().cast();
            let mut bad_event = options(9);
            bad_event.event_name = invalid_utf8.as_ptr().cast();
            let mut bad_target = options(9);
            bad_target.target = invalid_utf8.as_ptr().cast();

            assert_eq!(
                otel_logger_log_record(&logger, std::ptr::null(), c"x".as_ptr(), no_attrs),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_arr(
                    &logger,
                    std::ptr::null(),
                    c"x".as_ptr(),
                    no_attr_array,
                    0
                ),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_record_n(&logger, std::ptr::null(), bad_view, no_attrs),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_log_record_arr_n(&logger, std::ptr::null(), bad_view, no_attr_array, 0),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            for invalid in [&short, &bad_level, &negative_time] {
                assert_eq!(
                    otel_logger_log_record(&logger, invalid, c"x".as_ptr(), no_attrs),
                    otel_result_t::OTEL_ERR_INVALID_ARGUMENT
                );
                assert_eq!(
                    otel_logger_log_record_arr(&logger, invalid, c"x".as_ptr(), no_attr_array, 0),
                    otel_result_t::OTEL_ERR_INVALID_ARGUMENT
                );
            }
            for invalid in [&bad_severity_text, &bad_event, &bad_target] {
                assert_eq!(
                    otel_logger_log_record(&logger, invalid, c"x".as_ptr(), no_attrs),
                    otel_result_t::OTEL_ERR_INVALID_UTF8
                );
            }

            let bad_attr = otel_attribute_t {
                kind: 99,
                key: c"k".as_ptr(),
                key_len: 1,
                value: otel_attribute_value_t { integer: 0 },
                value_len: 0,
            };
            let attr_ref = &bad_attr as *const otel_attribute_t;
            assert_eq!(
                otel_logger_log(&logger, std::ptr::null(), 9, c"x".as_ptr(), attr_ref),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_log_arr(&logger, std::ptr::null(), 9, c"x".as_ptr(), &attr_ref, 1),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );

            assert_eq!(
                otel_logger_provider_logger(std::ptr::null(), &mut other, c"x".as_ptr()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut other, std::ptr::null()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_provider_logger(&provider, &mut other, invalid_utf8.as_ptr().cast()),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_provider_logger_n(&provider, &mut other, bad_view),
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert_eq!(
                otel_logger_provider_force_flush(std::ptr::null()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                otel_logger_provider_shutdown(std::ptr::null()),
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );

            otel_logger_release(&mut other);
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
        }
    }
}
