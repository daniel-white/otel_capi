//! Trace provider, tracer, and span APIs.

use std::env;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use opentelemetry::trace::{Span as _, Status, Tracer as _, TracerProvider as _};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SimpleSpanProcessor, SdkTracerProvider,
    Span as SdkSpan,
};

use crate::attributes::from_varargs;
use crate::config::{
    build_error, duration_ms, exporter_kind, resource_from_env, sdk_disabled, sdk_error,
};
use crate::error::{Error, Result, c_str, guard, optional_utf8, otel_result_t, utf8};
use crate::handles::{
    RuntimeInner, SpanInner, TracerInner, TracerProviderInner, arc, const_slot, copy_string, init,
    optional_resource, optional_runtime, slot,
};
use crate::handles::{otel_runtime_t, otel_span_t, otel_tracer_provider_t, otel_tracer_t};

/// Explicit span status exported to C.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_span_status_t {
    /// No status has been set.
    OTEL_SPAN_STATUS_UNSET,
    /// The operation succeeded.
    OTEL_SPAN_STATUS_OK,
    /// The operation failed.
    OTEL_SPAN_STATUS_ERROR,
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
                builder = builder.with_span_processor(SimpleSpanProcessor::new(
                    opentelemetry_stdout::SpanExporter::default(),
                ));
            }
            crate::config::ExporterKind::Otlp => {
                let runtime = runtime.as_ref().ok_or_else(|| {
                    Error::new(
                        otel_result_t::OTEL_ERR_CONFIG,
                        "OTLP requires an otel_runtime_t; pass one to otel_tracer_provider_new",
                    )
                })?;
                let exporter = {
                    let _guard = runtime.enter();
                    opentelemetry_otlp::SpanExporter::builder().build()
                }
                .map_err(|err| build_error("failed to build OTLP span exporter", err))?;
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
        provider.provider.force_flush().map_err(|err| sdk_error("failed to flush spans", err))?;
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

/// Starts a span with an optional `NULL`-terminated list of attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracer_span_start(
    tracer: *const otel_tracer_t,
    handle: *mut otel_span_t,
    name: *const c_char,
    attributes: ...,
) -> otel_result_t {
    guard(|| {
        let tracer = unsafe { arc::<TracerInner>(const_slot(tracer)) }?;
        let name = utf8(unsafe { c_str(name, "span name")? }, "span name")?;
        let attributes = unsafe { from_varargs(attributes) }?;
        let mut builder = tracer.span_builder(name);
        builder.attributes = Some(attributes);
        let span: SdkSpan = tracer.build(builder);
        unsafe { init(slot(handle), Mutex::new(Some(span))) }?;
        Ok(())
    })
}

/// Adds attributes to a started span.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_attributes(
    span: *const otel_span_t,
    attributes: ...,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_mut()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        let attributes = unsafe { from_varargs(attributes) }?;
        span.set_attributes(attributes);
        Ok(())
    })
}

/// Adds a named event with optional attributes to a started span.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_add_event(
    span: *const otel_span_t,
    name: *const c_char,
    attributes: ...,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_mut()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        let name = utf8(unsafe { c_str(name, "event name")? }, "event name")?;
        let attributes = unsafe { from_varargs(attributes) }?;
        span.add_event(name, attributes);
        Ok(())
    })
}

/// Sets the span status. `description` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_set_status(
    span: *const otel_span_t,
    status: otel_span_status_t,
    description: *const c_char,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_mut()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        let description = unsafe { optional_utf8(description, "status description")? }.unwrap_or_default();
        let status = match status {
            otel_span_status_t::OTEL_SPAN_STATUS_UNSET => Status::Unset,
            otel_span_status_t::OTEL_SPAN_STATUS_OK => Status::Ok,
            otel_span_status_t::OTEL_SPAN_STATUS_ERROR => Status::Error {
                description: description.into(),
            },
        };
        span.set_status(status);
        Ok(())
    })
}

/// Ends a span. Calling this more than once is harmless.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_end(span: *const otel_span_t) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let mut guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        if let Some(span) = guard.as_mut() {
            span.end();
        }
        Ok(())
    })
}

/// Copies the 32-hex-digit trace ID (plus NUL) into `buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_trace_id(
    span: *const otel_span_t,
    buf: *mut c_char,
    capacity: usize,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        let id = span.span_context().trace_id().to_string();
        copy_string(&id, buf, capacity)?;
        Ok(())
    })
}

/// Copies the 16-hex-digit span ID (plus NUL) into `buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_span_span_id(
    span: *const otel_span_t,
    buf: *mut c_char,
    capacity: usize,
) -> otel_result_t {
    guard(|| {
        let inner = unsafe { arc::<SpanInner>(const_slot(span)) }?;
        let guard = inner.lock().map_err(|_| Error::internal("span lock is poisoned"))?;
        let span = guard
            .as_ref()
            .ok_or_else(|| Error::invalid("span has already ended"))?;
        let id = span.span_context().span_id().to_string();
        copy_string(&id, buf, capacity)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::{otel_tracer_provider_release, otel_tracer_release};

    #[test]
    fn no_exporter_provider_creates_tracer_and_span() {
        // Isolate this test from ambient configuration.
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
}
