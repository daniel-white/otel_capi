//! Logger provider, logger, and log record APIs.

#[cfg(test)]
use std::env;

use std::collections::HashSet;
use std::ffi::{VaList, c_char};
use std::sync::{Arc, Mutex, OnceLock};

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::logs::{
    BatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider, SimpleLogProcessor,
};

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config::{
    build_error, duration_ms, exporter_kind, resource_from_env, sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, otel_result_t, utf8};
use crate::handles::{
    LoggerInner, LoggerProviderInner, RuntimeInner, arc, const_slot, init, optional_resource,
    optional_runtime, slot,
};
use crate::handles::{otel_logger_provider_t, otel_logger_t, otel_runtime_t, otel_span_t};
use crate::util::system_time;

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

static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

fn interned(text: String) -> &'static str {
    let mut set = INTERNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("string interner is poisoned");
    if let Some(existing) = set.get(text.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.into_boxed_str());
    set.insert(leaked);
    leaked
}

unsafe fn emit_log_with_attributes(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: Vec<KeyValue>,
) -> Result<()> {
    if options.is_null() {
        return Err(Error::invalid("log options must not be NULL"));
    }
    let options = unsafe { &*options };
    if options.size < std::mem::size_of::<otel_log_record_options_t>() {
        return Err(Error::invalid(
            "otel_log_record_options_t.size is too small",
        ));
    }
    let logger = unsafe { arc::<LoggerInner>(const_slot(logger)) }?;
    let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
    let (number, default_text) = severity(options.level)?;
    let timestamp = system_time(options.timestamp_unix_nano)?;
    let observed = system_time(options.observed_timestamp_unix_nano)?;

    let mut record = logger.create_log_record();
    record.set_severity_number(number);
    record.set_severity_text(
        match unsafe { crate::util::optional_str(options.severity_text, "severity text")? } {
            Some(text) => interned(text.to_owned()),
            None => default_text,
        },
    );
    record.set_body(AnyValue::String(body.into()));
    if let Some(name) = unsafe { crate::util::optional_str(options.event_name, "event name")? } {
        record.set_event_name(interned(name.to_owned()));
    }
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
    body: *const c_char,
    args: VaList,
) -> Result<()> {
    let attributes = unsafe { from_varargs(args) }?;
    unsafe { emit_log_with_attributes(logger, options, body, attributes) }
}

unsafe fn emit_log_arr(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> Result<()> {
    let attributes = unsafe { from_array(attributes, count) }?;
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
                builder = builder.with_log_processor(SimpleLogProcessor::new(
                    opentelemetry_stdout::LogExporter::default(),
                ));
            }
            crate::config::ExporterKind::Otlp => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "OTLP requires an otel_runtime_t; pass one to otel_logger_provider_new",
                    )
                })?;
                let exporter = {
                    let _guard = runtime.enter();
                    opentelemetry_otlp::LogExporter::builder().build()
                }
                .map_err(|err| build_error("failed to build OTLP log exporter", err))?;
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_new(
    handle: *mut otel_logger_provider_t,
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

/// Flushes all pending log records.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_force_flush(
    provider: *const otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .force_flush()
            .map_err(|err| sdk_error("failed to flush logs", err))?;
        Ok(())
    })
}

/// Shuts down the provider and its processors.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_shutdown(
    provider: *const otel_logger_provider_t,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        provider
            .provider
            .shutdown()
            .map_err(|err| sdk_error("failed to shut down logger provider", err))?;
        Ok(())
    })
}

/// Creates a logger for an instrumentation scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_provider_logger(
    provider: *const otel_logger_provider_t,
    handle: *mut otel_logger_t,
    name: *const c_char,
) -> otel_result_t {
    guard(|| {
        let provider = unsafe { arc::<LoggerProviderInner>(const_slot(provider)) }?;
        let name = utf8(unsafe { c_str(name, "logger name")? }, "logger name")?;
        // The scope name is retained by the provider for its lifetime.
        let name: &'static str = Box::leak(name.into_boxed_str());
        unsafe { init(slot(handle), provider.provider.logger(name)) }?;
        Ok(())
    })
}

/// Emits a log record with optional attributes. `span` may be NULL; when set,
/// the record carries that span's trace and span IDs.
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
        unsafe { emit_log(logger, &options, body, attributes) }
    })
}

/// Array-based variant of [`otel_logger_log`].
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
        unsafe { emit_log_arr(logger, &options, body, attributes, count) }
    })
}

/// Emits a structured log record with event name, target, timestamps, and
/// optional span correlation configured through `options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: ...
) -> otel_result_t {
    guard(|| unsafe { emit_log(logger, options, body, attributes) })
}

/// Array-based variant of [`otel_logger_log_record`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_logger_log_record_arr(
    logger: *const otel_logger_t,
    options: *const otel_log_record_options_t,
    body: *const c_char,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| unsafe { emit_log_arr(logger, options, body, attributes, count) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::{otel_attribute_t, otel_attribute_type_t, otel_attribute_value_t};
    use crate::handles::{otel_logger_provider_release, otel_logger_release};

    #[test]
    fn rejects_invalid_severity() {
        let error = severity(999).unwrap_err();
        assert_eq!(
            error.status,
            crate::error::otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn no_exporter_logger_emits() {
        unsafe { env::set_var("OTEL_LOGS_EXPORTER", "none") };
        let mut provider = otel_logger_provider_t {
            inner: std::ptr::null_mut(),
        };
        let mut logger = otel_logger_t {
            inner: std::ptr::null_mut(),
        };
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
                value: otel_attribute_value_t {
                    string: value.as_ptr(),
                },
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
            env::remove_var("OTEL_LOGS_EXPORTER");
        }
    }
}
