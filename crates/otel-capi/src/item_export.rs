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
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
use opentelemetry_proto::tonic::trace::v1::ResourceSpans;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, GaugeDataPoint, HistogramDataPoint, Metric, MetricData, ResourceMetrics,
    SumDataPoint,
};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use prost::Message;

use crate::items::{
    AttributeViews, ItemTransport, c_str_view, nanos, otel_histogram_point_t, otel_item_signal_t,
    otel_log_item_t, otel_metric_item_t, otel_metric_item_type_t, otel_metric_point_t,
    otel_metric_value_kind_t, otel_metric_value_t, otel_span_event_item_t, otel_span_item_t,
    otel_span_link_item_t, severity, span_id, span_kind, trace_id,
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

fn emit_typed_log(transport: &ItemTransport, record: &SdkLogRecord, resource: &Resource) {
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
    fn to_f64(self) -> f64;
}

impl MetricValue for f64 {
    fn view(self) -> (otel_metric_value_kind_t, otel_metric_value_t) {
        (
            otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64,
            otel_metric_value_t { as_f64: self },
        )
    }

    fn to_f64(self) -> f64 {
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

    fn to_f64(self) -> f64 {
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

    fn to_f64(self) -> f64 {
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
            sum: HistogramDataPoint::sum(point).to_f64(),
            has_min: point.min().is_some(),
            min: point.min().map(MetricValue::to_f64).unwrap_or(0.0),
            has_max: point.max().is_some(),
            max: point.max().map(MetricValue::to_f64).unwrap_or(0.0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::otel_encoded_item_t;
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
    use opentelemetry::trace::{
        Event, Link, SpanContext, TraceFlags, TraceId, TraceState, Tracer as _, TracerProvider as _,
    };
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::data::{Exemplar, ExponentialHistogramDataPoint, ScopeMetrics};
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanEvents, SpanLinks};
    use std::borrow::Cow;
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::future::Future;
    use std::mem::{ManuallyDrop, align_of, size_of};
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Debug, PartialEq)]
    struct HistogramView {
        count: u64,
        sum: f64,
        has_min: bool,
        min: f64,
        has_max: bool,
        max: f64,
    }

    #[derive(Default)]
    struct Capture {
        spans: Cell<usize>,
        logs: Cell<usize>,
        metrics: Cell<usize>,
        encoded: RefCell<Vec<(i32, usize)>>,
        statuses: RefCell<Vec<(i32, String)>>,
        bodies: RefCell<Vec<String>>,
        resource_counts: RefCell<Vec<usize>>,
        span_names: RefCell<Vec<String>>,
        event_names: RefCell<Vec<String>>,
        link_counts: RefCell<Vec<usize>>,
        parent_span_ids: RefCell<Vec<u64>>,
        metric_names: RefCell<Vec<String>>,
        metric_kinds: RefCell<Vec<i32>>,
        point_kinds: RefCell<Vec<i32>>,
        point_values: RefCell<Vec<(i32, f64)>>,
        point_counts: RefCell<Vec<usize>>,
        bucket_counts: RefCell<Vec<usize>>,
        boundaries: RefCell<Vec<Vec<f64>>>,
        bucket_values: RefCell<Vec<Vec<u64>>>,
        histogram_values: RefCell<Vec<HistogramView>>,
    }

    impl Capture {
        fn transport(&self) -> ItemTransport {
            ItemTransport {
                on_span: Some(capture_span),
                on_log: Some(capture_log),
                on_metric: Some(capture_metric),
                on_encoded: Some(capture_encoded),
                user_data: (self as *const Capture).cast_mut().cast::<c_void>(),
            }
        }
    }

    unsafe fn captured(user_data: *mut c_void) -> &'static Capture {
        // SAFETY: the test callbacks only read the item for the duration of the
        // call and `user_data` points to a live `Capture` for that whole call.
        unsafe { &*user_data.cast::<Capture>() }
    }

    // SAFETY: `view` is borrowed from a live CString in the captured item.
    unsafe fn view_text(view: crate::strings::otel_str_t) -> String {
        // SAFETY: the view points to `view.len` readable bytes for this call.
        let bytes = unsafe { std::slice::from_raw_parts(view.ptr.cast::<u8>(), view.len) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    unsafe extern "C" fn capture_span(item: *const otel_span_item_t, user_data: *mut c_void) {
        // SAFETY: `item` is borrowed for this call and `user_data` is live.
        unsafe {
            let item = &*item;
            let capture = captured(user_data);
            capture.spans.set(capture.spans.get() + 1);
            capture
                .statuses
                .borrow_mut()
                .push((item.status as i32, view_text(item.status_message)));
            capture
                .resource_counts
                .borrow_mut()
                .push(item.resource_attribute_count);
            capture.span_names.borrow_mut().push(view_text(item.name));
            capture
                .parent_span_ids
                .borrow_mut()
                .push(item.parent_span_id.value);
            if item.event_count > 0 {
                // SAFETY: `events` points to `event_count` initialized views.
                let events = std::slice::from_raw_parts(item.events, item.event_count);
                for event in events {
                    capture.event_names.borrow_mut().push(view_text(event.name));
                }
            }
            if item.link_count > 0 {
                // SAFETY: `links` points to `link_count` initialized views.
                let links = std::slice::from_raw_parts(item.links, item.link_count);
                capture.link_counts.borrow_mut().push(links.len());
            }
        }
    }

    unsafe extern "C" fn capture_log(item: *const otel_log_item_t, user_data: *mut c_void) {
        // SAFETY: `item` is borrowed for this call and `user_data` is live.
        unsafe {
            let item = &*item;
            let capture = captured(user_data);
            capture.logs.set(capture.logs.get() + 1);
            capture.bodies.borrow_mut().push(view_text(item.body));
            capture
                .statuses
                .borrow_mut()
                .push((item.severity as i32, String::new()));
        }
    }

    unsafe extern "C" fn capture_metric(item: *const otel_metric_item_t, user_data: *mut c_void) {
        // SAFETY: `item` is borrowed for this call and `user_data` is live.
        unsafe {
            let item = &*item;
            let capture = captured(user_data);
            capture.metrics.set(capture.metrics.get() + 1);
            capture.metric_names.borrow_mut().push(view_text(item.name));
            capture.metric_kinds.borrow_mut().push(item.kind as i32);
            capture.point_counts.borrow_mut().push(item.point_count);
            capture
                .resource_counts
                .borrow_mut()
                .push(item.resource_attribute_count);
            if item.point_count > 0 {
                // SAFETY: `points` points to `point_count` initialized views.
                let point = &*item.points;
                capture
                    .point_kinds
                    .borrow_mut()
                    .push(point.value_kind as i32);
                // SAFETY: `value_kind` selects the active union member.
                let value = match point.value_kind {
                    otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64 => point.value.as_u64 as f64,
                    otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64 => point.value.as_f64,
                    otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64 => point.value.as_i64 as f64,
                };
                capture
                    .point_values
                    .borrow_mut()
                    .push((point.value_kind as i32, value));
            }
            if item.histogram_count > 0 {
                // SAFETY: `histograms` points to `histogram_count` initialized views.
                let histogram = &*item.histograms;
                capture
                    .bucket_counts
                    .borrow_mut()
                    .push(histogram.bucket_count);
                // SAFETY: `bucket_counts` has `bucket_count + 1` readable entries.
                let counts =
                    std::slice::from_raw_parts(histogram.bucket_counts, histogram.bucket_count + 1);
                capture.bucket_values.borrow_mut().push(counts.to_vec());
                // SAFETY: `bucket_boundaries` has `bucket_count` readable entries.
                let bounds =
                    std::slice::from_raw_parts(histogram.bucket_boundaries, histogram.bucket_count);
                capture.boundaries.borrow_mut().push(bounds.to_vec());
                capture.histogram_values.borrow_mut().push(HistogramView {
                    count: histogram.count,
                    sum: histogram.sum,
                    has_min: histogram.has_min,
                    min: histogram.min,
                    has_max: histogram.has_max,
                    max: histogram.max,
                });
            }
        }
    }

    unsafe extern "C" fn capture_encoded(item: *const otel_encoded_item_t, user_data: *mut c_void) {
        // SAFETY: `item` is borrowed for this call and `user_data` is live.
        unsafe {
            let item = &*item;
            captured(user_data)
                .encoded
                .borrow_mut()
                .push((item.signal as i32, item.len));
        }
    }

    // The SDK's aggregated metric types have private fields and no public
    // constructors. These mirrors duplicate their layouts so tests can fabricate
    // data without starting a periodic reader or an exporter thread.
    struct MirrorGaugeDataPoint<T> {
        attributes: Vec<KeyValue>,
        value: T,
        exemplars: Vec<Exemplar<T>>,
    }

    struct MirrorGauge<T> {
        data_points: Vec<GaugeDataPoint<T>>,
        start_time: Option<SystemTime>,
        time: SystemTime,
    }

    struct MirrorSumDataPoint<T> {
        attributes: Vec<KeyValue>,
        value: T,
        exemplars: Vec<Exemplar<T>>,
    }

    struct MirrorSum<T> {
        data_points: Vec<SumDataPoint<T>>,
        start_time: SystemTime,
        time: SystemTime,
        temporality: Temporality,
        is_monotonic: bool,
    }

    struct MirrorHistogramDataPoint<T> {
        attributes: Vec<KeyValue>,
        count: u64,
        bounds: Vec<f64>,
        bucket_counts: Vec<u64>,
        min: Option<T>,
        max: Option<T>,
        sum: T,
        exemplars: Vec<Exemplar<T>>,
    }

    struct MirrorHistogram<T> {
        data_points: Vec<HistogramDataPoint<T>>,
        start_time: SystemTime,
        time: SystemTime,
        temporality: Temporality,
    }

    struct MirrorExponentialHistogram<T> {
        data_points: Vec<ExponentialHistogramDataPoint<T>>,
        start_time: SystemTime,
        time: SystemTime,
        temporality: Temporality,
    }

    enum MirrorMetricData<T> {
        Gauge(MirrorGauge<T>),
        Sum(MirrorSum<T>),
        Histogram(MirrorHistogram<T>),
        ExponentialHistogram(MirrorExponentialHistogram<T>),
    }

    enum MirrorAggregatedMetrics {
        F64(MetricData<f64>),
        U64(MetricData<u64>),
        I64(MetricData<i64>),
    }

    struct MirrorMetric {
        name: Cow<'static, str>,
        description: Cow<'static, str>,
        unit: Cow<'static, str>,
        data: AggregatedMetrics,
    }

    struct MirrorScopeMetrics {
        scope: InstrumentationScope,
        metrics: Vec<Metric>,
    }

    struct MirrorResourceMetrics {
        resource: Resource,
        scope_metrics: Vec<ScopeMetrics>,
    }

    fn with_same_layout<A, B>(value: A) -> B {
        assert_eq!(size_of::<A>(), size_of::<B>());
        assert_eq!(align_of::<A>(), align_of::<B>());
        let value = ManuallyDrop::new(value);
        // SAFETY: each mirror type is declared with the same fields, field order,
        // variants, and payloads as the SDK type it stands in for; the assertions
        // verify size and alignment, and `ManuallyDrop` transfers ownership to `B`.
        unsafe { std::mem::transmute_copy(&value) }
    }

    fn gauge_data_point<T: Copy>(value: T) -> GaugeDataPoint<T> {
        let mirror = MirrorGaugeDataPoint {
            attributes: vec![KeyValue::new("gauge", 1i64)],
            value,
            exemplars: Vec::new(),
        };
        let _ = (&mirror.attributes, mirror.value, &mirror.exemplars);
        with_same_layout(mirror)
    }

    fn sum_data_point<T: Copy>(value: T) -> SumDataPoint<T> {
        let mirror = MirrorSumDataPoint {
            attributes: vec![KeyValue::new("sum", true)],
            value,
            exemplars: Vec::new(),
        };
        let _ = (&mirror.attributes, mirror.value, &mirror.exemplars);
        with_same_layout(mirror)
    }

    fn histogram_data_point<T: Copy>(
        sum: T,
        min: Option<T>,
        max: Option<T>,
    ) -> HistogramDataPoint<T> {
        let mirror = MirrorHistogramDataPoint {
            attributes: vec![KeyValue::new("histogram", "one")],
            count: 3,
            bounds: vec![1.0, 2.5],
            bucket_counts: vec![1, 1, 1],
            min,
            max,
            sum,
            exemplars: Vec::new(),
        };
        let _ = (
            &mirror.attributes,
            mirror.count,
            &mirror.bounds,
            &mirror.bucket_counts,
            mirror.min,
            mirror.max,
            mirror.sum,
            &mirror.exemplars,
        );
        with_same_layout(mirror)
    }

    fn gauge_metric_data<T: Copy>(value: T) -> MetricData<T> {
        let gauge = MirrorGauge {
            data_points: vec![gauge_data_point(value)],
            start_time: Some(UNIX_EPOCH),
            time: UNIX_EPOCH + Duration::from_secs(1),
        };
        let _ = (&gauge.data_points, gauge.start_time, gauge.time);
        let mirror = MirrorMetricData::Gauge(gauge);
        match &mirror {
            MirrorMetricData::Gauge(data) => {
                let _ = data;
            }
            MirrorMetricData::Sum(data) => {
                let _ = data;
            }
            MirrorMetricData::Histogram(data) => {
                let _ = data;
            }
            MirrorMetricData::ExponentialHistogram(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn sum_metric_data<T: Copy>(value: T, is_monotonic: bool) -> MetricData<T> {
        let sum = MirrorSum {
            data_points: vec![sum_data_point(value)],
            start_time: UNIX_EPOCH,
            time: UNIX_EPOCH + Duration::from_secs(1),
            temporality: Temporality::Cumulative,
            is_monotonic,
        };
        let _ = (
            &sum.data_points,
            sum.start_time,
            sum.time,
            sum.temporality,
            sum.is_monotonic,
        );
        let mirror = MirrorMetricData::Sum(sum);
        match &mirror {
            MirrorMetricData::Gauge(data) => {
                let _ = data;
            }
            MirrorMetricData::Sum(data) => {
                let _ = data;
            }
            MirrorMetricData::Histogram(data) => {
                let _ = data;
            }
            MirrorMetricData::ExponentialHistogram(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn histogram_metric_data<T: Copy>(sum: T, min: Option<T>, max: Option<T>) -> MetricData<T> {
        let histogram = MirrorHistogram {
            data_points: vec![histogram_data_point(sum, min, max)],
            start_time: UNIX_EPOCH,
            time: UNIX_EPOCH + Duration::from_secs(1),
            temporality: Temporality::Cumulative,
        };
        let _ = (
            &histogram.data_points,
            histogram.start_time,
            histogram.time,
            histogram.temporality,
        );
        let mirror = MirrorMetricData::Histogram(histogram);
        match &mirror {
            MirrorMetricData::Gauge(data) => {
                let _ = data;
            }
            MirrorMetricData::Sum(data) => {
                let _ = data;
            }
            MirrorMetricData::Histogram(data) => {
                let _ = data;
            }
            MirrorMetricData::ExponentialHistogram(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn exponential_histogram_metric_data<T: Copy>() -> MetricData<T> {
        let exponential: MirrorExponentialHistogram<T> = MirrorExponentialHistogram {
            data_points: Vec::new(),
            start_time: UNIX_EPOCH,
            time: UNIX_EPOCH + Duration::from_secs(1),
            temporality: Temporality::Cumulative,
        };
        let _ = (
            &exponential.data_points,
            exponential.start_time,
            exponential.time,
            exponential.temporality,
        );
        let mirror = MirrorMetricData::ExponentialHistogram(exponential);
        match &mirror {
            MirrorMetricData::Gauge(data) => {
                let _ = data;
            }
            MirrorMetricData::Sum(data) => {
                let _ = data;
            }
            MirrorMetricData::Histogram(data) => {
                let _ = data;
            }
            MirrorMetricData::ExponentialHistogram(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn aggregated_f64(data: MetricData<f64>) -> AggregatedMetrics {
        let mirror = MirrorAggregatedMetrics::F64(data);
        match &mirror {
            MirrorAggregatedMetrics::F64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::U64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::I64(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn aggregated_u64(data: MetricData<u64>) -> AggregatedMetrics {
        let mirror = MirrorAggregatedMetrics::U64(data);
        match &mirror {
            MirrorAggregatedMetrics::F64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::U64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::I64(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn aggregated_i64(data: MetricData<i64>) -> AggregatedMetrics {
        let mirror = MirrorAggregatedMetrics::I64(data);
        match &mirror {
            MirrorAggregatedMetrics::F64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::U64(data) => {
                let _ = data;
            }
            MirrorAggregatedMetrics::I64(data) => {
                let _ = data;
            }
        }
        with_same_layout(mirror)
    }

    fn fabricated_metric(name: &str, data: AggregatedMetrics) -> Metric {
        let mirror = MirrorMetric {
            name: Cow::Owned(name.to_owned()),
            description: Cow::Borrowed("description"),
            unit: Cow::Borrowed("unit"),
            data,
        };
        let _ = (
            &mirror.name,
            &mirror.description,
            &mirror.unit,
            &mirror.data,
        );
        with_same_layout(mirror)
    }

    fn fabricated_resource_metrics(metrics: Vec<Metric>) -> ResourceMetrics {
        let scope_mirror = MirrorScopeMetrics {
            scope: InstrumentationScope::builder("scope").build(),
            metrics,
        };
        let _ = (&scope_mirror.scope, &scope_mirror.metrics);
        let scope: ScopeMetrics = with_same_layout(scope_mirror);
        let mirror = MirrorResourceMetrics {
            resource: Resource::builder_empty().build(),
            scope_metrics: vec![scope],
        };
        let _ = (&mirror.resource, &mirror.scope_metrics);
        with_same_layout(mirror)
    }

    fn ready_result<F: Future>(future: F) -> F::Output {
        let mut context = TaskContext::from_waker(Waker::noop());
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("future unexpectedly pending"),
        }
    }

    #[test]
    fn any_value_string_formats_all_variants() {
        assert_eq!(
            any_value_string(&AnyValue::String("hello".into()))
                .to_str()
                .unwrap(),
            "hello"
        );
        assert_eq!(any_value_string(&AnyValue::Int(-4)).to_str().unwrap(), "-4");
        assert_eq!(
            any_value_string(&AnyValue::Double(1.5)).to_str().unwrap(),
            "1.5"
        );
        assert_eq!(
            any_value_string(&AnyValue::Boolean(true)).to_str().unwrap(),
            "true"
        );
        assert_eq!(
            any_value_string(&AnyValue::Bytes(vec![b'a', 0xff].into()))
                .to_str()
                .unwrap(),
            "a\u{fffd}"
        );
        assert_eq!(
            any_value_string(&AnyValue::String("a\0b".into())).as_bytes(),
            b""
        );
        assert!(
            !any_value_string(&AnyValue::ListAny(Vec::new().into()))
                .to_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn status_view_reports_code_and_description() {
        let (status, message) = status_view(&Status::Unset);
        assert_eq!(status, otel_span_status_t::OTEL_SPAN_STATUS_UNSET);
        assert!(message.as_bytes().is_empty());
        let (status, message) = status_view(&Status::Ok);
        assert_eq!(status, otel_span_status_t::OTEL_SPAN_STATUS_OK);
        assert!(message.as_bytes().is_empty());
        let (status, message) = status_view(&Status::Error {
            description: "a\0b".into(),
        });
        assert_eq!(status, otel_span_status_t::OTEL_SPAN_STATUS_ERROR);
        assert!(message.as_bytes().is_empty());
        let (_, message) = status_view(&Status::Error {
            description: "boom".into(),
        });
        assert_eq!(message.to_str().unwrap(), "boom");
    }

    #[test]
    fn metric_value_views_preserve_kind_and_number() {
        let (kind, value) = 1.25f64.view();
        assert_eq!(kind, otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64);
        // SAFETY: `value_kind` selects the active union member.
        assert_eq!(unsafe { value.as_f64 }, 1.25);
        assert_eq!(1.25f64.to_f64(), 1.25);
        let (kind, value) = 9u64.view();
        assert_eq!(kind, otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64);
        // SAFETY: `value_kind` selects the active union member.
        assert_eq!(unsafe { value.as_u64 }, 9);
        assert_eq!(9u64.to_f64(), 9.0);
        let (kind, value) = (-3i64).view();
        assert_eq!(kind, otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64);
        // SAFETY: `value_kind` selects the active union member.
        assert_eq!(unsafe { value.as_i64 }, -3);
        assert_eq!((-3i64).to_f64(), -3.0);
    }

    fn sample_span(sampled: bool, status: Status) -> SpanData {
        let flags = if sampled {
            TraceFlags::SAMPLED
        } else {
            TraceFlags::default()
        };
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
                SpanId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
                flags,
                false,
                TraceState::NONE,
            ),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: opentelemetry::trace::SpanKind::Server,
            name: "operation".into(),
            start_time: UNIX_EPOCH + Duration::from_secs(1),
            end_time: UNIX_EPOCH + Duration::from_secs(2),
            attributes: vec![KeyValue::new("key", "value")],
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status,
            instrumentation_scope: InstrumentationScope::builder("scope").build(),
        }
    }

    #[test]
    fn span_processor_emits_sampled_spans_only() {
        let capture = Capture::default();
        let mut processor = ItemSpanProcessor::new(capture.transport());
        processor.set_resource(
            &Resource::builder()
                .with_attributes(vec![KeyValue::new("service.name", "test")])
                .build(),
        );
        assert!(processor.force_flush().is_ok());
        assert!(processor.shutdown_with_timeout(Duration::ZERO).is_ok());
        assert!(format!("{processor:?}").contains("ItemSpanProcessor"));

        processor.on_end(sample_span(
            true,
            Status::Error {
                description: "boom".into(),
            },
        ));
        assert_eq!(capture.spans.get(), 1);
        assert_eq!(capture.statuses.borrow()[0].0, 2);
        assert_eq!(capture.statuses.borrow()[0].1, "boom");
        assert!(capture.resource_counts.borrow()[0] > 0);
        assert_eq!(capture.encoded.borrow().len(), 1);
        assert_eq!(capture.encoded.borrow()[0].0, 0);
        assert!(capture.encoded.borrow()[0].1 > 0);

        processor.on_end(sample_span(true, Status::Ok));
        assert_eq!(capture.spans.get(), 2);
        assert_eq!(capture.statuses.borrow()[1].0, 1);
        assert!(capture.statuses.borrow()[1].1.is_empty());

        processor.on_end(sample_span(false, Status::Unset));
        assert_eq!(capture.spans.get(), 2);
        assert_eq!(capture.encoded.borrow().len(), 2);
    }

    #[test]
    fn log_processor_emits_typed_and_encoded_records() {
        let capture = Capture::default();
        let mut processor = ItemLogProcessor::new(capture.transport());
        processor.set_resource(&Resource::builder_empty().build());
        assert!(processor.force_flush().is_ok());
        assert!(processor.shutdown_with_timeout(Duration::ZERO).is_ok());
        assert!(format!("{processor:?}").contains("ItemLogProcessor"));

        let provider = SdkLoggerProvider::builder().build();
        let logger = provider.logger("scope");
        let scope = InstrumentationScope::builder("scope").build();
        let mut record = logger.create_log_record();
        record.set_severity_number(opentelemetry::logs::Severity::Warn);
        record.set_severity_text("WARN");
        record.set_body(AnyValue::Int(42));
        record.set_timestamp(UNIX_EPOCH + Duration::from_secs(1));
        record.set_observed_timestamp(UNIX_EPOCH + Duration::from_secs(2));
        record.set_trace_context(
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
            Some(TraceFlags::SAMPLED),
        );
        record.add_attribute("attr", "value");
        processor.emit(&mut record, &scope);
        assert_eq!(capture.logs.get(), 1);
        assert_eq!(capture.bodies.borrow()[0], "42");
        assert_eq!(
            capture.statuses.borrow()[0].0,
            otel_severity_t::OTEL_SEVERITY_WARN as i32
        );
        assert_eq!(capture.encoded.borrow()[0].0, 2);
        assert!(capture.encoded.borrow()[0].1 > 0);

        let mut empty = logger.create_log_record();
        empty.set_observed_timestamp(UNIX_EPOCH + Duration::from_secs(3));
        processor.emit(&mut empty, &scope);
        assert_eq!(capture.logs.get(), 2);
        assert!(capture.bodies.borrow()[1].is_empty());
        assert_eq!(
            capture.statuses.borrow()[1].0,
            otel_severity_t::OTEL_SEVERITY_INFO as i32
        );
    }

    #[test]
    fn metric_exporter_exposes_reader_contract_and_handles_empty_metrics() {
        let capture = Capture::default();
        let exporter = ItemMetricExporter::new(capture.transport());
        assert_eq!(exporter.temporality(), Temporality::Cumulative);
        assert!(exporter.force_flush().is_ok());
        assert!(format!("{exporter:?}").contains("ItemMetricExporter"));

        exporter.send(&ResourceMetrics::default()).unwrap();
        assert_eq!(capture.metrics.get(), 0);
        assert_eq!(capture.encoded.borrow().len(), 1);
        assert_eq!(capture.encoded.borrow()[0].0, 1);
        assert!(capture.encoded.borrow()[0].1 > 0);

        assert!(exporter.shutdown_with_timeout(Duration::ZERO).is_ok());
    }

    #[test]
    fn span_processor_emits_events_links_and_parent_span() {
        let capture = Capture::default();
        let processor = ItemSpanProcessor::new(capture.transport());

        let tracer_provider = SdkTracerProvider::builder().build();
        let tracer = tracer_provider.tracer("test");
        let suppressed = Context::new().with_telemetry_suppressed();
        let mut started = tracer.start_with_context("started", &suppressed);
        processor.on_start(&mut started, &Context::new());

        let mut span = sample_span(true, Status::Unset);
        span.parent_span_id = SpanId::from_bytes([9, 8, 7, 6, 5, 4, 3, 2]);
        let mut events = SpanEvents::default();
        events.events.push(Event::new(
            "event",
            UNIX_EPOCH + Duration::from_secs(1),
            vec![KeyValue::new("event-key", true)],
            0,
        ));
        span.events = events;
        let mut links = SpanLinks::default();
        links.links.push(Link::new(
            SpanContext::new(
                TraceId::from_bytes([3; 16]),
                SpanId::from_bytes([4; 8]),
                TraceFlags::SAMPLED,
                false,
                TraceState::NONE,
            ),
            vec![KeyValue::new("link-key", 5i64)],
            0,
        ));
        span.links = links;
        processor.on_end(span);

        assert_eq!(capture.spans.get(), 1);
        assert_eq!(capture.span_names.borrow()[0], "operation");
        assert_eq!(capture.event_names.borrow().as_slice(), &["event"]);
        assert_eq!(capture.link_counts.borrow()[0], 1);
        assert_eq!(
            capture.parent_span_ids.borrow()[0],
            u64::from_be_bytes([9, 8, 7, 6, 5, 4, 3, 2])
        );
        assert_eq!(capture.encoded.borrow().len(), 1);
    }

    #[test]
    fn metric_exporter_emits_all_metric_data_variants() {
        let capture = Capture::default();
        let exporter = ItemMetricExporter::new(capture.transport());
        let metrics = fabricated_resource_metrics(vec![
            fabricated_metric("f64-gauge", aggregated_f64(gauge_metric_data(1.5))),
            fabricated_metric("u64-gauge", aggregated_u64(gauge_metric_data(2u64))),
            fabricated_metric("i64-gauge", aggregated_i64(gauge_metric_data(-3i64))),
            fabricated_metric("f64-sum", aggregated_f64(sum_metric_data(4.5, true))),
            fabricated_metric("u64-sum", aggregated_u64(sum_metric_data(5u64, true))),
            fabricated_metric("i64-updown", aggregated_i64(sum_metric_data(-6i64, false))),
            fabricated_metric(
                "f64-histogram",
                aggregated_f64(histogram_metric_data(7.5, Some(1.0), Some(2.0))),
            ),
            fabricated_metric(
                "u64-histogram",
                aggregated_u64(histogram_metric_data(8u64, None, None)),
            ),
            fabricated_metric(
                "exponential-histogram",
                aggregated_f64(exponential_histogram_metric_data()),
            ),
        ]);

        exporter.send(&metrics).unwrap();
        assert!(ready_result(exporter.export(&metrics)).is_ok());

        assert_eq!(capture.metrics.get(), 18);
        assert_eq!(capture.encoded.borrow().len(), 2);
        assert_eq!(capture.metric_names.borrow()[0], "f64-gauge");
        assert_eq!(
            capture.metric_kinds.borrow()[..3],
            [
                otel_metric_item_type_t::OTEL_METRIC_ITEM_GAUGE as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_GAUGE as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_GAUGE as i32,
            ]
        );
        assert_eq!(
            capture.metric_kinds.borrow()[3..6],
            [
                otel_metric_item_type_t::OTEL_METRIC_ITEM_COUNTER as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_COUNTER as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_UPDOWN_COUNTER as i32,
            ]
        );
        assert_eq!(
            capture.metric_kinds.borrow()[6..9],
            [
                otel_metric_item_type_t::OTEL_METRIC_ITEM_HISTOGRAM as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_HISTOGRAM as i32,
                otel_metric_item_type_t::OTEL_METRIC_ITEM_HISTOGRAM as i32,
            ]
        );
        assert_eq!(capture.point_kinds.borrow().len(), 12);
        assert_eq!(
            capture.point_kinds.borrow()[..6],
            [
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64 as i32,
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64 as i32,
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64 as i32,
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64 as i32,
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64 as i32,
                otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64 as i32,
            ]
        );
        assert_eq!(capture.point_values.borrow().len(), 12);
        assert_eq!(
            capture.point_values.borrow()[..6],
            [
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64 as i32, 1.5),
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64 as i32, 2.0),
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64 as i32, -3.0),
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_F64 as i32, 4.5),
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_U64 as i32, 5.0),
                (otel_metric_value_kind_t::OTEL_METRIC_VALUE_I64 as i32, -6.0),
            ]
        );
        assert_eq!(capture.point_counts.borrow()[8], 0);
        assert_eq!(capture.bucket_counts.borrow()[0], 2);
        assert_eq!(capture.boundaries.borrow()[0], vec![1.0, 2.5]);
        assert_eq!(capture.bucket_values.borrow()[0], vec![1, 1, 1]);
        assert_eq!(
            capture.histogram_values.borrow()[0],
            HistogramView {
                count: 3,
                sum: 7.5,
                has_min: true,
                min: 1.0,
                has_max: true,
                max: 2.0,
            }
        );
        assert_eq!(
            capture.histogram_values.borrow()[1],
            HistogramView {
                count: 3,
                sum: 8.0,
                has_min: false,
                min: 0.0,
                has_max: false,
                max: 0.0,
            }
        );
    }
}
