//! SDK adapters that deliver telemetry to the application's in-memory item
//! transport. Each adapter emits both typed C items and encoded OTLP bytes.

use std::ffi::CString;
use std::fmt;
use std::future::{Ready, ready};
use std::sync::RwLock;
use std::time::Duration;

use opentelemetry::Context;
use opentelemetry::InstrumentationScope;
use opentelemetry::KeyValue;
use opentelemetry::logs::AnyValue;
use opentelemetry::trace::{SpanId, Status};
use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
use opentelemetry_proto::tonic::trace::v1::ResourceSpans;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, GaugeDataPoint, HistogramDataPoint, Metric, MetricData, ResourceMetrics,
    SumDataPoint,
};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use prost::Message;

use crate::items::{
    AttributeViews, ItemTransport, c_str_view, nanos, otel_histogram_point_t,
    otel_item_signal_t, otel_log_item_t, otel_metric_item_type_t, otel_metric_item_t,
    otel_metric_point_t, otel_metric_value_kind_t, otel_metric_value_t, otel_span_event_item_t,
    otel_span_item_t, otel_span_link_item_t, severity, span_id, span_kind, trace_id,
};
use crate::logs::otel_severity_t;
use crate::trace::otel_span_status_t;

fn resource_kvs(resource: &Resource) -> Vec<KeyValue> {
    resource
        .iter()
        .map(|(key, value)| KeyValue::new(key.clone(), value.clone()))
        .collect()
}

fn any_value_string(value: &AnyValue) -> CString {
    let text = match value {
        AnyValue::String(value) => value.as_str().to_owned(),
        AnyValue::Int(value) => value.to_string(),
        AnyValue::Double(value) => value.to_string(),
        AnyValue::Boolean(value) => value.to_string(),
        AnyValue::Bytes(value) => String::from_utf8_lossy(value).into_owned(),
        other => format!("{other:?}"),
    };
    CString::new(text).unwrap_or_default()
}

fn status_view(status: &Status) -> (otel_span_status_t, CString) {
    match status {
        Status::Unset => (
            otel_span_status_t::OTEL_SPAN_STATUS_UNSET,
            CString::new("").unwrap(),
        ),
        Status::Ok => (
            otel_span_status_t::OTEL_SPAN_STATUS_OK,
            CString::new("").unwrap(),
        ),
        Status::Error { description } => (
            otel_span_status_t::OTEL_SPAN_STATUS_ERROR,
            CString::new(description.as_ref()).unwrap_or_default(),
        ),
    }
}

fn emit_typed_span(transport: &ItemTransport, span: &SpanData, resource: &Resource) {
    let name = CString::new(span.name.as_ref()).unwrap_or_default();
    let (status, status_message) = status_view(&span.status);
    let attributes = AttributeViews::new(span.attributes.iter());
    let (attr_ptr, attr_len) = attributes.as_slice();
    let resource_values = resource_kvs(resource);
    let resource_attributes = AttributeViews::new(resource_values.iter());
    let (resource_ptr, resource_len) = resource_attributes.as_slice();

    let mut event_names = Vec::with_capacity(span.events.len());
    let mut event_attributes = Vec::with_capacity(span.events.len());
    let mut events = Vec::with_capacity(span.events.len());
    for event in span.events.iter() {
        event_names.push(CString::new(event.name.as_ref()).unwrap_or_default());
        let views = AttributeViews::new(event.attributes.iter());
        let (ptr, len) = views.as_slice();
        events.push(otel_span_event_item_t {
            name: c_str_view(event_names.last().expect("event name was pushed")),
            timestamp_unix_nano: nanos(event.timestamp),
            attributes: ptr,
            attribute_count: len,
        });
        event_attributes.push(views);
    }

    let mut link_attributes = Vec::with_capacity(span.links.len());
    let mut links = Vec::with_capacity(span.links.len());
    for link in span.links.iter() {
        let views = AttributeViews::new(link.attributes.iter());
        let (ptr, len) = views.as_slice();
        links.push(otel_span_link_item_t {
            trace_id: trace_id(link.span_context.trace_id().to_bytes()),
            span_id: span_id(link.span_context.span_id().to_bytes()),
            attributes: ptr,
            attribute_count: len,
        });
        link_attributes.push(views);
    }

    let parent = if span.parent_span_id == SpanId::INVALID {
        [0u8; 8]
    } else {
        span.parent_span_id.to_bytes()
    };
    let item = otel_span_item_t {
        trace_id: trace_id(span.span_context.trace_id().to_bytes()),
        span_id: span_id(span.span_context.span_id().to_bytes()),
        parent_span_id: crate::items::span_id(parent),
        name: c_str_view(&name),
        kind: span_kind(span.span_kind.clone()),
        status,
        status_message: c_str_view(&status_message),
        start_time_unix_nano: nanos(span.start_time),
        end_time_unix_nano: nanos(span.end_time),
        attributes: attr_ptr,
        attribute_count: attr_len,
        events: events.as_ptr(),
        event_count: events.len(),
        links: links.as_ptr(),
        link_count: links.len(),
        resource_attributes: resource_ptr,
        resource_attribute_count: resource_len,
    };
    transport.emit_span(&item);
}

/// Passes each finished span to the application as a typed item and an OTLP
/// protobuf request.
pub(crate) struct ItemSpanProcessor {
    transport: ItemTransport,
    resource: RwLock<Resource>,
}

impl ItemSpanProcessor {
    pub(crate) fn new(transport: ItemTransport) -> Self {
        Self {
            transport,
            resource: RwLock::new(Resource::builder_empty().build()),
        }
    }
}

impl fmt::Debug for ItemSpanProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemSpanProcessor").finish_non_exhaustive()
    }
}

impl SpanProcessor for ItemSpanProcessor {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }
        let resource = self.resource.read().expect("resource lock poisoned");
        emit_typed_span(&self.transport, &span, &resource);
        let proto_resource: ResourceAttributesWithSchema = (&*resource).into();
        let request = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans::new(span, &proto_resource)],
        };
        self.transport.emit_encoded(
            otel_item_signal_t::OTEL_ITEM_SIGNAL_TRACE,
            &request.encode_to_vec(),
        );
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        *self.resource.write().expect("resource lock poisoned") = resource.clone();
    }
}

fn emit_typed_log(
    transport: &ItemTransport,
    record: &SdkLogRecord,
    resource: &Resource,
) {
    let severity_text = CString::new(record.severity_text().unwrap_or("")).unwrap_or_default();
    let empty_body = CString::new("").unwrap();
    let body = record.body().map(any_value_string).unwrap_or(empty_body);
    let attributes = AttributeViews::from_any(record.attributes_iter());
    let (attr_ptr, attr_len) = attributes.as_slice();
    let resource_values = resource_kvs(resource);
    let resource_attributes = AttributeViews::new(resource_values.iter());
    let (resource_ptr, resource_len) = resource_attributes.as_slice();
    let (trace, span) = match record.trace_context() {
        Some(context) => (
            trace_id(context.trace_id.to_bytes()),
            span_id(context.span_id.to_bytes()),
        ),
        None => (trace_id([0u8; 16]), span_id([0u8; 8])),
    };
    let item = otel_log_item_t {
        timestamp_unix_nano: record.timestamp().map(nanos).unwrap_or(0),
        observed_timestamp_unix_nano: record.observed_timestamp().map(nanos).unwrap_or(0),
        severity: record
            .severity_number()
            .map(severity)
            .unwrap_or(otel_severity_t::OTEL_SEVERITY_INFO),
        severity_text: c_str_view(&severity_text),
        body: c_str_view(&body),
        trace_id: trace,
        span_id: span,
        attributes: attr_ptr,
        attribute_count: attr_len,
        resource_attributes: resource_ptr,
        resource_attribute_count: resource_len,
    };
    transport.emit_log(&item);
}

/// Passes each log record to the application as a typed item and an OTLP
/// protobuf request.
pub(crate) struct ItemLogProcessor {
    transport: ItemTransport,
    resource: RwLock<Resource>,
}

impl ItemLogProcessor {
    pub(crate) fn new(transport: ItemTransport) -> Self {
        Self {
            transport,
            resource: RwLock::new(Resource::builder_empty().build()),
        }
    }
}

impl fmt::Debug for ItemLogProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemLogProcessor").finish_non_exhaustive()
    }
}

impl LogProcessor for ItemLogProcessor {
    fn emit(&self, data: &mut SdkLogRecord, instrumentation: &InstrumentationScope) {
        let resource = self.resource.read().expect("resource lock poisoned");
        emit_typed_log(&self.transport, data, &resource);
        let proto_resource: ResourceAttributesWithSchema = (&*resource).into();
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs::from((
                (&*data, instrumentation),
                &proto_resource,
            ))],
        };
        self.transport.emit_encoded(
            otel_item_signal_t::OTEL_ITEM_SIGNAL_LOG,
            &request.encode_to_vec(),
        );
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        *self.resource.write().expect("resource lock poisoned") = resource.clone();
    }
}

trait MetricValue {
    fn view(self) -> (otel_metric_value_kind_t, otel_metric_value_t);
    fn as_f64(self) -> f64;
}

impl MetricValue for f64 {
    fn view(self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        (
            otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64,
            otel_metric_value_t { as_f64: self },
        )
    }

    fn as_f64(self) -> f64 {
        self
    }
}

impl MetricValue for u64 {
    fn view(self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        (
            otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64,
            otel_metric_value_t { as_u64: self },
        )
    }

    fn as_f64(self) -> f64 {
        self as f64
    }
}

impl MetricValue for i64 {
    fn view(self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        (
            otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64,
            otel_metric_value_t { as_i64: self },
        )
    }

    fn as_f64(self) -> f64 {
        self as f64
    }
}

trait MetricPoint {
    fn attribute_kvs(&self) -> Vec<KeyValue>;
    fn value_view(&self) -> (otel_metric_value_kind_t, otel_metric_value_t);
}

impl<T: Copy + MetricValue> MetricPoint for GaugeDataPoint<T> {
    fn attribute_kvs(&self) -> Vec<KeyValue> {
        self.attributes().cloned().collect()
    }

    fn value_view(&self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        GaugeDataPoint::value(self).view()
    }
}

impl<T: Copy + MetricValue> MetricPoint for SumDataPoint<T> {
    fn attribute_kvs(&self) -> Vec<KeyValue> {
        self.attributes().cloned().collect()
    }

    fn value_view(&self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        SumDataPoint::value(self).view()
    }
}

// `attributes` owns the C strings referenced by `views`.
#[allow(dead_code)]
struct NumericPoints {
    attributes: Vec<AttributeViews>,
    views: Vec<otel_metric_point_t>,
}

fn numeric_points<'a, P: MetricPoint + 'a>(points: impl Iterator<Item = &'a P>) -> NumericPoints {
    let mut attributes = Vec::new();
    let mut views = Vec::new();
    for point in points {
        let values = point.attribute_kvs();
        let view = AttributeViews::new(values.iter());
        let (ptr, len) = view.as_slice();
        let (kind, value) = point.value_view();
        views.push(otel_metric_point_t {
            attributes: ptr,
            attribute_count: len,
            value_kind: kind,
            value,
        });
        attributes.push(view);
    }
    NumericPoints { attributes, views }
}

// The owned vectors keep the pointers in `views` alive for the callback.
#[allow(dead_code)]
struct HistogramPoints {
    attributes: Vec<AttributeViews>,
    bounds: Vec<Vec<f64>>,
    counts: Vec<Vec<u64>>,
    views: Vec<otel_histogram_point_t>,
}

fn histogram_points<'a, T: Copy + MetricValue + 'a>(
    points: impl Iterator<Item = &'a HistogramDataPoint<T>>,
) -> HistogramPoints {
    let mut attributes = Vec::new();
    let mut bounds = Vec::new();
    let mut counts = Vec::new();
    let mut views = Vec::new();
    for point in points {
        let values: Vec<KeyValue> = point.attributes().cloned().collect();
        let view = AttributeViews::new(values.iter());
        let (ptr, len) = view.as_slice();
        let bounds_for_point: Vec<f64> = point.bounds().collect();
        let counts_for_point: Vec<u64> = point.bucket_counts().collect();
        let (boundaries, bucket_counts, bucket_count) = (
            bounds_for_point.as_ptr(),
            counts_for_point.as_ptr(),
            bounds_for_point.len(),
        );
        views.push(otel_histogram_point_t {
            attributes: ptr,
            attribute_count: len,
            count: point.count(),
            sum: HistogramDataPoint::sum(point).as_f64(),
            has_min: point.min().is_some(),
            min: point.min().map(MetricValue::as_f64).unwrap_or(0.0),
            has_max: point.max().is_some(),
            max: point.max().map(MetricValue::as_f64).unwrap_or(0.0),
            bucket_boundaries: boundaries,
            bucket_counts,
            bucket_count,
        });
        attributes.push(view);
        bounds.push(bounds_for_point);
        counts.push(counts_for_point);
    }
    HistogramPoints {
        attributes,
        bounds,
        counts,
        views,
    }
}

fn emit_metric_data<T: Copy + MetricValue>(
    transport: &ItemTransport,
    name: &CString,
    description: &CString,
    unit: &CString,
    data: &MetricData<T>,
    resource: (*const crate::attributes::otel_attribute_t, usize),
) {
    let (resource_ptr, resource_len) = resource;
    let emit = |kind,
                points: (*const otel_metric_point_t, usize),
                histograms: (*const otel_histogram_point_t, usize)| {
        let item = otel_metric_item_t {
            name: c_str_view(name),
            description: c_str_view(description),
            unit: c_str_view(unit),
            kind,
            points: points.0,
            point_count: points.1,
            histograms: histograms.0,
            histogram_count: histograms.1,
            resource_attributes: resource_ptr,
            resource_attribute_count: resource_len,
        };
        transport.emit_metric(&item);
    };

    match data {
        MetricData::Gauge(gauge) => {
            let points = numeric_points(gauge.data_points());
            emit(
                otel_metric_item_type_t::OTEL_METRIC_ITEM_GAUGE,
                (points.views.as_ptr(), points.views.len()),
                (std::ptr::null(), 0),
            );
        }
        MetricData::Sum(sum) => {
            let points = numeric_points(sum.data_points());
            emit(
                if sum.is_monotonic() {
                    otel_metric_item_type_t::OTEL_METRIC_ITEM_COUNTER
                } else {
                    otel_metric_item_type_t::OTEL_METRIC_ITEM_UPDOWN_COUNTER
                },
                (points.views.as_ptr(), points.views.len()),
                (std::ptr::null(), 0),
            );
        }
        MetricData::Histogram(histogram) => {
            let points = histogram_points(histogram.data_points());
            emit(
                otel_metric_item_type_t::OTEL_METRIC_ITEM_HISTOGRAM,
                (std::ptr::null(), 0),
                (points.views.as_ptr(), points.views.len()),
            );
        }
        MetricData::ExponentialHistogram(_) => {
            emit(
                otel_metric_item_type_t::OTEL_METRIC_ITEM_HISTOGRAM,
                (std::ptr::null(), 0),
                (std::ptr::null(), 0),
            );
        }
    }
}

fn emit_typed_metric(
    transport: &ItemTransport,
    metric: &Metric,
    resource: (*const crate::attributes::otel_attribute_t, usize),
) {
    let name = CString::new(metric.name()).unwrap_or_default();
    let description = CString::new(metric.description()).unwrap_or_default();
    let unit = CString::new(metric.unit()).unwrap_or_default();
    match metric.data() {
        AggregatedMetrics::F64(data) => {
            emit_metric_data(transport, &name, &description, &unit, data, resource);
        }
        AggregatedMetrics::U64(data) => {
            emit_metric_data(transport, &name, &description, &unit, data, resource);
        }
        AggregatedMetrics::I64(data) => {
            emit_metric_data(transport, &name, &description, &unit, data, resource);
        }
    }
}

/// Passes collected metrics to the application as typed items and one encoded
/// OTLP protobuf request.
pub(crate) struct ItemMetricExporter {
    transport: ItemTransport,
}

impl fmt::Debug for ItemMetricExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemMetricExporter").finish_non_exhaustive()
    }
}

impl ItemMetricExporter {
    pub(crate) fn new(transport: ItemTransport) -> Self {
        Self { transport }
    }

    fn send(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let resource_values = resource_kvs(metrics.resource());
        let resource_attributes = AttributeViews::new(resource_values.iter());
        let resource = resource_attributes.as_slice();
        for scope in metrics.scope_metrics() {
            for metric in scope.metrics() {
                emit_typed_metric(&self.transport, metric, resource);
            }
        }
        let request = ExportMetricsServiceRequest::from(metrics);
        self.transport.emit_encoded(
            otel_item_signal_t::OTEL_ITEM_SIGNAL_METRIC,
            &request.encode_to_vec(),
        );
        Ok(())
    }
}

impl PushMetricExporter for ItemMetricExporter {
    fn export(
        &self,
        metrics: &ResourceMetrics,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let result = self.send(metrics);
        ready(result) as Ready<OTelSdkResult>
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}
