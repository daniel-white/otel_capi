//! Lightweight SDK exporters that encode OTLP protobuf and call a registered
//! callback exporter.
//!
//! The core does not embed an HTTP client or a protocol stack. Each exporter
//! encodes one batch with `opentelemetry-proto`, builds an HTTP request, and
//! delegates delivery to the `HttpClient` adapter over the selected callback.

use std::fmt;
use std::future::{Ready, ready};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use opentelemetry_http::{Bytes, Request};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use prost::Message;

use crate::config::{self, OtlpProtocol};
use crate::error::{Error, Result, otel_result_t};
use crate::exporter::{CallbackExporterClient, callback_exporter_client};
use crate::handles::RuntimeInner;

fn callback_client(
    runtime: &Arc<RuntimeInner>,
    signal: &str,
    protocol: OtlpProtocol,
) -> Result<CallbackExporterClient> {
    let name = match protocol {
        OtlpProtocol::Grpc => config::OTLP_GRPC_EXPORTER,
        OtlpProtocol::HttpBinary | OtlpProtocol::HttpJson => config::OTLP_HTTP_EXPORTER,
    };
    callback_exporter_client(runtime, name, signal)
}

fn stdout_client(runtime: &Arc<RuntimeInner>, signal: &str) -> Result<CallbackExporterClient> {
    callback_exporter_client(runtime, config::STDOUT_EXPORTER, signal)
}

fn client_error(context: &str, err: impl fmt::Display) -> OTelSdkError {
    OTelSdkError::InternalFailure(format!("{context}: {err}"))
}

/// Validates OTLP configuration while building an exporter. This keeps
/// configuration errors visible before telemetry is emitted.
fn validate_config(signal: &str) -> Result<()> {
    let endpoint = config::otlp_endpoint(signal)?;
    Request::builder()
        .method("POST")
        .uri(endpoint)
        .body(())
        .map_err(|err| {
            Error::new(
                otel_result_t::OTEL_ERR_CONFIG,
                format!("invalid OTLP endpoint for {signal}: {err}"),
            )
        })?;
    config::validate_otlp_compression(signal)
}

fn post(client: &CallbackExporterClient, signal: &str, body: Vec<u8>) -> OTelSdkResult {
    let endpoint = config::otlp_endpoint(signal)
        .map_err(|err| client_error("invalid OTLP endpoint", err.message))?;
    let mut request = Request::builder()
        .method("POST")
        .uri(endpoint.as_str())
        .header("content-type", "application/x-protobuf")
        .body(Bytes::from(body))
        .map_err(|err| client_error("invalid OTLP request", err))?;
    for (name, value) in &config::otlp_headers(signal) {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    client
        .invoke(request)
        .map(|_| ())
        .map_err(|err| client_error("OTLP export failed", err))
}

fn locked_resource(resource: &RwLock<Resource>) -> std::result::Result<Resource, OTelSdkError> {
    resource
        .read()
        .map(|guard| guard.clone())
        .map_err(|_| client_error("resource lock poisoned", "resource lock poisoned"))
}

/// Creates the span exporter selected by `OTEL_TRACES_EXPORTER=otlp`.
pub(crate) fn span_exporter(runtime: &Arc<RuntimeInner>) -> Result<CallbackSpanExporter> {
    let signal = "TRACES";
    validate_config(signal)?;
    let client = callback_client(runtime, signal, config::otlp_protocol(signal)?)?;
    Ok(CallbackSpanExporter::new(client))
}

/// Creates the span exporter selected by `OTEL_TRACES_EXPORTER=stdout`.
pub(crate) fn stdout_span_exporter(runtime: &Arc<RuntimeInner>) -> Result<CallbackSpanExporter> {
    validate_config("TRACES")?;
    Ok(CallbackSpanExporter::new(stdout_client(runtime, "TRACES")?))
}

/// Exports protobuf-encoded trace batches through a callback exporter.
pub(crate) struct CallbackSpanExporter {
    client: CallbackExporterClient,
    resource: RwLock<Resource>,
}

impl CallbackSpanExporter {
    fn new(client: CallbackExporterClient) -> Self {
        Self {
            client,
            resource: RwLock::new(Resource::builder_empty().build()),
        }
    }
}

impl fmt::Debug for CallbackSpanExporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackSpanExporter")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl SpanExporter for CallbackSpanExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let result = (|| {
            let resource = locked_resource(&self.resource)?;
            let proto: ResourceAttributesWithSchema = (&resource).into();
            let request = ExportTraceServiceRequest {
                resource_spans: group_spans_by_resource_and_scope(batch, &proto),
            };
            post(&self.client, "TRACES", request.encode_to_vec())
        })();
        ready(result) as Ready<OTelSdkResult>
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Ok(mut slot) = self.resource.write() {
            *slot = resource.clone();
        }
    }
}

/// Creates the log exporter selected by `OTEL_LOGS_EXPORTER=otlp`.
pub(crate) fn log_exporter(runtime: &Arc<RuntimeInner>) -> Result<CallbackLogExporter> {
    let signal = "LOGS";
    validate_config(signal)?;
    let client = callback_client(runtime, signal, config::otlp_protocol(signal)?)?;
    Ok(CallbackLogExporter::new(client))
}

/// Creates the log exporter selected by `OTEL_LOGS_EXPORTER=stdout`.
pub(crate) fn stdout_log_exporter(runtime: &Arc<RuntimeInner>) -> Result<CallbackLogExporter> {
    validate_config("LOGS")?;
    Ok(CallbackLogExporter::new(stdout_client(runtime, "LOGS")?))
}

/// Exports protobuf-encoded log batches through a callback exporter.
pub(crate) struct CallbackLogExporter {
    client: CallbackExporterClient,
    resource: RwLock<Resource>,
}

impl CallbackLogExporter {
    fn new(client: CallbackExporterClient) -> Self {
        Self {
            client,
            resource: RwLock::new(Resource::builder_empty().build()),
        }
    }
}

impl fmt::Debug for CallbackLogExporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackLogExporter")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl LogExporter for CallbackLogExporter {
    fn export(
        &self,
        batch: LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let result = (|| {
            let resource = locked_resource(&self.resource)?;
            let proto: ResourceAttributesWithSchema = (&resource).into();
            let request = ExportLogsServiceRequest {
                resource_logs: group_logs_by_resource_and_scope(&batch, &proto),
            };
            post(&self.client, "LOGS", request.encode_to_vec())
        })();
        ready(result) as Ready<OTelSdkResult>
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Ok(mut slot) = self.resource.write() {
            *slot = resource.clone();
        }
    }
}

/// Creates the metric exporter selected by `OTEL_METRICS_EXPORTER=otlp`.
pub(crate) fn metric_exporter(runtime: &Arc<RuntimeInner>) -> Result<CallbackMetricExporter> {
    let signal = "METRICS";
    validate_config(signal)?;
    let client = callback_client(runtime, signal, config::otlp_protocol(signal)?)?;
    Ok(CallbackMetricExporter::new(
        client,
        config::otlp_metrics_temporality(),
    ))
}

/// Creates the metric exporter selected by `OTEL_METRICS_EXPORTER=stdout`.
pub(crate) fn stdout_metric_exporter(
    runtime: &Arc<RuntimeInner>,
) -> Result<CallbackMetricExporter> {
    validate_config("METRICS")?;
    Ok(CallbackMetricExporter::new(
        stdout_client(runtime, "METRICS")?,
        config::otlp_metrics_temporality(),
    ))
}

/// Exports protobuf-encoded metric batches through a callback exporter.
pub(crate) struct CallbackMetricExporter {
    client: CallbackExporterClient,
    temporality: Temporality,
}

impl CallbackMetricExporter {
    fn new(client: CallbackExporterClient, temporality: Temporality) -> Self {
        Self {
            client,
            temporality,
        }
    }
}

impl fmt::Debug for CallbackMetricExporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackMetricExporter")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl PushMetricExporter for CallbackMetricExporter {
    fn export(
        &self,
        metrics: &ResourceMetrics,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let request = ExportMetricsServiceRequest::from(metrics);
        let result = post(&self.client, "METRICS", request.encode_to_vec());
        ready(result) as Ready<OTelSdkResult>
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        self.temporality
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{
        otel_exporter_request_t, otel_exporter_response_t, otel_exporter_send_fn, otel_str_t,
    };
    use crate::config::tests::EnvGuard;
    use crate::error::otel_result_t;
    use crate::exporter::Exporter;
    use crate::strings::str_slice;
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::{InstrumentationScope, KeyValue};
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use std::ffi::{CStr, c_char, c_void};
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::pin;
    use std::slice;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::UNIX_EPOCH;

    #[derive(Default)]
    struct Captured {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u64,
    }

    #[derive(Default)]
    struct Observed {
        requests: Vec<Captured>,
    }

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut task = TaskContext::from_waker(Waker::noop());
        let mut future = pin!(future);
        match future.as_mut().poll(&mut task) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("callback exporter future should be ready immediately"),
        }
    }

    fn runtime_with(send: otel_exporter_send_fn, user_data: *mut c_void) -> Arc<RuntimeInner> {
        let runtime = Arc::new(RuntimeInner::new());
        for name in [
            config::OTLP_HTTP_EXPORTER,
            config::OTLP_GRPC_EXPORTER,
            config::STDOUT_EXPORTER,
        ] {
            runtime
                .set_exporter(name.to_owned(), Exporter::new(send, user_data))
                .unwrap();
        }
        runtime
    }

    unsafe extern "C" fn capture_and_succeed(
        request: *const otel_exporter_request_t,
        response: *mut otel_exporter_response_t,
        user_data: *mut c_void,
    ) -> otel_result_t {
        // SAFETY: the test passes valid borrowed views and `user_data` points to
        // a live `Observed` for the duration of the export call.
        unsafe {
            let view = &*request;
            let observed = &mut *user_data.cast::<Observed>();
            let mut captured = Captured {
                method: CStr::from_ptr(view.method).to_string_lossy().into_owned(),
                url: CStr::from_ptr(view.url).to_string_lossy().into_owned(),
                body: slice::from_raw_parts(view.body, view.body_len).to_vec(),
                timeout_ms: view.timeout_ms,
                ..Captured::default()
            };
            for header in slice::from_raw_parts(view.headers, view.header_count) {
                let name = str_slice(header.name, "header name").unwrap();
                let value = str_slice(header.value, "header value").unwrap();
                captured.headers.push((name.to_owned(), value.to_owned()));
            }
            observed.requests.push(captured);
            *response = otel_exporter_response_t {
                status_code: 200,
                ..otel_exporter_response_t::EMPTY
            };
        }
        otel_result_t::OTEL_OK
    }

    unsafe extern "C" fn fail_export(
        _request: *const otel_exporter_request_t,
        response: *mut otel_exporter_response_t,
        _user_data: *mut c_void,
    ) -> otel_result_t {
        let message = b"collector unavailable";
        // SAFETY: the test supplies a valid response pointer for this call.
        unsafe {
            (*response).error = otel_str_t {
                ptr: message.as_ptr().cast::<c_char>(),
                len: message.len(),
            };
        }
        otel_result_t::OTEL_ERR_EXPORT
    }

    fn set_signal_env(env: &mut EnvGuard, protocol: &'static str, endpoint: &'static str) {
        env.set(protocol, "http/protobuf");
        env.set(endpoint, "https://collector.example/base");
        let timeout = match endpoint {
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT" => "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT" => "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
            _ => panic!("unexpected endpoint variable"),
        };
        env.set(timeout, "250");
    }

    fn sample_span() -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_bytes([1; 16]),
                SpanId::from_bytes([2; 8]),
                TraceFlags::SAMPLED,
                false,
                TraceState::NONE,
            ),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Server,
            name: "operation".into(),
            start_time: UNIX_EPOCH + Duration::from_secs(1),
            end_time: UNIX_EPOCH + Duration::from_secs(2),
            attributes: vec![KeyValue::new("key", "value")],
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Ok,
            instrumentation_scope: InstrumentationScope::builder("scope").build(),
        }
    }

    #[test]
    fn callback_exporters_encode_each_signal() {
        let mut env = EnvGuard::new();
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        );
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        );
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        );
        let mut observed = Observed::default();
        let runtime = runtime_with(capture_and_succeed, &mut observed as *mut _ as *mut c_void);

        let mut span = span_exporter(&runtime).unwrap();
        let mut log = log_exporter(&runtime).unwrap();
        let metric = metric_exporter(&runtime).unwrap();
        let resource = Resource::builder()
            .with_attributes(vec![KeyValue::new("service.name", "callback-test")])
            .build();
        SpanExporter::set_resource(&mut span, &resource);
        LogExporter::set_resource(&mut log, &resource);

        assert!(block_on_ready(SpanExporter::export(&span, vec![sample_span()])).is_ok());

        let provider = SdkLoggerProvider::builder().build();
        let logger = provider.logger("test");
        let mut record = logger.create_log_record();
        record.set_timestamp(UNIX_EPOCH + Duration::from_secs(1));
        record.set_observed_timestamp(UNIX_EPOCH + Duration::from_secs(1));
        record.set_body("hello".into());
        let scope = InstrumentationScope::builder("scope").build();
        let records = [(&record, &scope)];
        assert!(block_on_ready(LogExporter::export(&log, LogBatch::new(&records))).is_ok());
        assert!(
            block_on_ready(PushMetricExporter::export(
                &metric,
                &ResourceMetrics::default()
            ))
            .is_ok()
        );

        assert_eq!(observed.requests.len(), 3);
        for request in &observed.requests {
            assert_eq!(request.method, "POST");
            assert_eq!(request.timeout_ms, 250);
            assert!(
                request.headers.iter().any(
                    |(name, value)| name == "content-type" && value == "application/x-protobuf"
                )
            );
        }
        assert_eq!(observed.requests[0].url, "https://collector.example/base");
        assert_eq!(observed.requests[1].url, "https://collector.example/base");
        assert_eq!(observed.requests[2].url, "https://collector.example/base");

        let trace =
            ExportTraceServiceRequest::decode(observed.requests[0].body.as_slice()).unwrap();
        assert!(!trace.resource_spans.is_empty());
        assert!(
            trace.resource_spans[0]
                .resource
                .as_ref()
                .unwrap()
                .attributes
                .iter()
                .any(|attribute| attribute.key == "service.name")
        );
        let logs = ExportLogsServiceRequest::decode(observed.requests[1].body.as_slice()).unwrap();
        assert!(!logs.resource_logs.is_empty());
    }

    #[test]
    fn callback_exporters_attach_configured_otlp_headers() {
        let mut env = EnvGuard::new();
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        );
        env.set(
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "x-one=first%20value, x-two=second",
        );
        env.set("OTEL_EXPORTER_OTLP_HEADERS", "x-global=ignored");
        let mut observed = Observed::default();
        let runtime = runtime_with(capture_and_succeed, &mut observed as *mut _ as *mut c_void);

        let exporter = span_exporter(&runtime).unwrap();
        assert!(block_on_ready(SpanExporter::export(&exporter, Vec::new())).is_ok());

        let headers = &observed.requests[0].headers;
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-one" && value == "first value")
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-two" && value == "second")
        );
        assert!(!headers.iter().any(|(name, _)| name == "x-global"));
    }

    #[test]
    fn callback_metric_exporter_honors_temporality_preference() {
        let mut env = EnvGuard::new();
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        );
        env.set("OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE", "delta");
        let mut observed = Observed::default();
        let runtime = runtime_with(capture_and_succeed, &mut observed as *mut _ as *mut c_void);

        let exporter = metric_exporter(&runtime).unwrap();
        assert_eq!(
            PushMetricExporter::temporality(&exporter),
            Temporality::Delta
        );
        let stdout = stdout_metric_exporter(&runtime).unwrap();
        assert_eq!(PushMetricExporter::temporality(&stdout), Temporality::Delta);

        env.set(
            "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
            "invalid",
        );
        let exporter = metric_exporter(&runtime).unwrap();
        assert_eq!(
            PushMetricExporter::temporality(&exporter),
            Temporality::Cumulative
        );
    }

    #[test]
    fn callback_exporters_report_configuration_and_callback_errors() {
        let mut env = EnvGuard::new();
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        );
        let mut observed = Observed::default();
        let runtime = runtime_with(fail_export, &mut observed as *mut _ as *mut c_void);

        let exporter = span_exporter(&runtime).unwrap();
        let error = block_on_ready(SpanExporter::export(&exporter, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("OTLP export failed"));

        env.set(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "not a valid URI with spaces",
        );
        let error = block_on_ready(SpanExporter::export(&exporter, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("invalid OTLP request"));
        let error = span_exporter(&runtime).unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_CONFIG);

        env.set(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "https://collector.example",
        );
        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc");
        let exporter = span_exporter(&runtime).unwrap();
        assert!(format!("{exporter:?}").contains("name: \"grpc\""));

        env.set("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "bogus");
        assert_eq!(
            span_exporter(&runtime).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );

        let empty = Arc::new(RuntimeInner::new());
        assert_eq!(
            span_exporter(&empty).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
    }

    #[test]
    fn callback_exporters_support_stdout_and_lifecycle_defaults() {
        let mut env = EnvGuard::new();
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        );
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        );
        set_signal_env(
            &mut env,
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        );
        let mut observed = Observed::default();
        let runtime = runtime_with(capture_and_succeed, &mut observed as *mut _ as *mut c_void);

        let span = stdout_span_exporter(&runtime).unwrap();
        let log = stdout_log_exporter(&runtime).unwrap();
        let metric = stdout_metric_exporter(&runtime).unwrap();
        assert!(format!("{span:?}").contains("CallbackSpanExporter"));
        assert!(format!("{span:?}").contains("name: \"stdout\""));
        assert!(format!("{log:?}").contains("name: \"stdout\""));
        assert!(format!("{metric:?}").contains("name: \"stdout\""));

        assert!(SpanExporter::shutdown_with_timeout(&span, Duration::ZERO).is_ok());
        assert!(LogExporter::shutdown_with_timeout(&log, Duration::ZERO).is_ok());
        assert!(PushMetricExporter::force_flush(&metric).is_ok());
        assert!(PushMetricExporter::shutdown_with_timeout(&metric, Duration::ZERO).is_ok());
        assert_eq!(
            PushMetricExporter::temporality(&metric),
            Temporality::Cumulative
        );

        let poisoned = span_exporter(&runtime).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = poisoned.resource.write().unwrap();
            panic!("poison resource lock");
        }));
        assert!(result.is_err());
        let error = block_on_ready(SpanExporter::export(&poisoned, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("resource lock poisoned"));
    }
}
