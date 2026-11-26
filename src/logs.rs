//! Logger provider, logger, and log record APIs.

#[cfg(test)]
use std::env;

use std::ffi::c_char;
use std::sync::Arc;

use opentelemetry::global::ObjectSafeSpan as _;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::Value;
use opentelemetry_sdk::logs::{
    BatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider, SimpleLogProcessor,
};

use crate::attributes::from_varargs;
use crate::config::{
    build_error, duration_ms, exporter_kind, resource_from_env, sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, otel_result_t, utf8};
use crate::handles::{
    LoggerInner, LoggerProviderInner, RuntimeInner, SpanInner, arc, const_slot, init,
    optional_resource, optional_runtime, slot,
};
use crate::handles::{otel_logger_t, otel_logger_provider_t, otel_runtime_t, otel_span_t};

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
    attributes: ...,
) -> otel_result_t {
    guard(|| {
        let logger = unsafe { arc::<LoggerInner>(const_slot(logger)) }?;
        let body = utf8(unsafe { c_str(body, "log body")? }, "log body")?;
        let attributes = unsafe { from_varargs(attributes) }?;
        let (number, text) = severity(level)?;

        let mut record = logger.create_log_record();
        record.set_severity_number(number);
        record.set_severity_text(text);
        record.set_body(AnyValue::String(body.into()));
        for attribute in attributes {
            record.add_attribute(attribute.key.to_string(), any_value(attribute.value));
        }

        if !span.is_null() {
            let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
            let guard = inner
                .lock()
                .map_err(|_| Error::internal("span lock is poisoned"))?;
            if let Some(span) = guard.as_ref() {
                let context = span.span_context();
                record.set_trace_context(
                    context.trace_id(),
                    context.span_id(),
                    Some(context.trace_flags()),
                );
            }
        }

        logger.emit(record);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::otel_attribute_t;
    use crate::handles::{
        otel_logger_provider_release, otel_logger_release,
    };

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
            otel_logger_release(&mut logger);
            otel_logger_provider_release(&mut provider);
            env::remove_var("OTEL_LOGS_EXPORTER");
        }
    }
}
