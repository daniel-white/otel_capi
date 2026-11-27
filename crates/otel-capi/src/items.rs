//! In-memory item transport types for applications that want to batch or
//! forward telemetry themselves.
//!
//! The typed callbacks receive borrowed SDK items; the encoded callback
//! receives OTLP protobuf bytes. Both are optional and may be registered
//! together on `otel_runtime_t`.

use std::ffi::{CString, c_void};
use std::ptr;

use opentelemetry::logs::AnyValue;
use opentelemetry::{Key, KeyValue};

use crate::attributes::{otel_attribute_t, otel_attribute_type_t, otel_attribute_value_t};
use crate::error::Error;
use crate::logs::otel_severity_t;
use crate::strings::otel_str_t;
use crate::trace::{otel_span_id_t, otel_span_kind_t, otel_span_status_t, otel_trace_id_t};

/// Signal carried by an encoded item.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_item_signal_t {
    OTEL_ITEM_SIGNAL_TRACE = 0,
    OTEL_ITEM_SIGNAL_METRIC = 1,
    OTEL_ITEM_SIGNAL_LOG = 2,
}

/// OTLP-encoded telemetry handed to the application in memory.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_encoded_item_t {
    pub signal: otel_item_signal_t,
    pub data: *const u8,
    pub len: usize,
}

/// A span event carried by [`otel_span_item_t`].
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_span_event_item_t {
    pub name: otel_str_t,
    pub timestamp_unix_nano: u64,
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
}

/// A span link carried by [`otel_span_item_t`].
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_span_link_item_t {
    pub trace_id: otel_trace_id_t,
    pub span_id: otel_span_id_t,
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
}

/// A finished span handed to the application.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_span_item_t {
    pub trace_id: otel_trace_id_t,
    pub span_id: otel_span_id_t,
    pub parent_span_id: otel_span_id_t,
    pub name: otel_str_t,
    pub kind: otel_span_kind_t,
    pub status: otel_span_status_t,
    pub status_message: otel_str_t,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
    pub events: *const otel_span_event_item_t,
    pub event_count: usize,
    pub links: *const otel_span_link_item_t,
    pub link_count: usize,
    pub resource_attributes: *const otel_attribute_t,
    pub resource_attribute_count: usize,
}

/// A log record handed to the application.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_log_item_t {
    pub timestamp_unix_nano: u64,
    pub observed_timestamp_unix_nano: u64,
    pub severity: otel_severity_t,
    pub severity_text: otel_str_t,
    pub body: otel_str_t,
    pub trace_id: otel_trace_id_t,
    pub span_id: otel_span_id_t,
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
    pub resource_attributes: *const otel_attribute_t,
    pub resource_attribute_count: usize,
}

/// Kind of a numeric metric data point.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_metric_value_kind_t {
    OTEL_METRIC_VALUE_U64 = 0,
    OTEL_METRIC_VALUE_F64 = 1,
    OTEL_METRIC_VALUE_I64 = 2,
}

/// Numeric value of a metric data point. Read the member named by
/// [`otel_metric_point_t::value_kind`].
#[repr(C)]
#[derive(Copy, Clone)]
pub union otel_metric_value_t {
    pub as_u64: u64,
    pub as_f64: f64,
    pub as_i64: i64,
}

/// One numeric metric data point.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_metric_point_t {
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
    pub value_kind: otel_metric_value_kind_t,
    pub value: otel_metric_value_t,
}

/// One histogram metric data point. `bucket_counts` has `bucket_count + 1`
/// entries and `bucket_boundaries` has `bucket_count` entries.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_histogram_point_t {
    pub attributes: *const otel_attribute_t,
    pub attribute_count: usize,
    pub count: u64,
    pub sum: f64,
    pub has_min: bool,
    pub min: f64,
    pub has_max: bool,
    pub max: f64,
    pub bucket_boundaries: *const f64,
    pub bucket_counts: *const u64,
    pub bucket_count: usize,
}

/// Kind of an aggregated metric item.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_metric_item_type_t {
    OTEL_METRIC_ITEM_GAUGE = 0,
    OTEL_METRIC_ITEM_COUNTER = 1,
    OTEL_METRIC_ITEM_UPDOWN_COUNTER = 2,
    OTEL_METRIC_ITEM_HISTOGRAM = 3,
}

/// One aggregated metric stream handed to the application.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_metric_item_t {
    pub name: otel_str_t,
    pub description: otel_str_t,
    pub unit: otel_str_t,
    pub kind: otel_metric_item_type_t,
    pub points: *const otel_metric_point_t,
    pub point_count: usize,
    pub histograms: *const otel_histogram_point_t,
    pub histogram_count: usize,
    pub resource_attributes: *const otel_attribute_t,
    pub resource_attribute_count: usize,
}

/// Receives one finished span. Borrowed for the duration of the callback.
pub type otel_span_item_fn = Option<unsafe extern "C" fn(*const otel_span_item_t, *mut c_void)>;
/// Receives one log record. Borrowed for the duration of the callback.
pub type otel_log_item_fn = Option<unsafe extern "C" fn(*const otel_log_item_t, *mut c_void)>;
/// Receives one aggregated metric stream. Borrowed for the duration of the callback.
pub type otel_metric_item_fn = Option<unsafe extern "C" fn(*const otel_metric_item_t, *mut c_void)>;
/// Receives an OTLP-encoded batch for one signal.
pub type otel_encoded_item_fn =
    Option<unsafe extern "C" fn(*const otel_encoded_item_t, *mut c_void)>;

/// Callbacks registered by the application for in-memory export.
///
/// Initialize with `OTEL_ITEM_TRANSPORT_INIT`, set the callbacks you want, and
/// call `otel_runtime_set_item_transport`. The struct is copied; only
/// `user_data` is borrowed and must remain valid until the runtime is released.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_item_transport_t {
    pub size: usize,
    pub on_span: otel_span_item_fn,
    pub on_log: otel_log_item_fn,
    pub on_metric: otel_metric_item_fn,
    pub on_encoded: otel_encoded_item_fn,
    pub user_data: *mut c_void,
}

impl otel_item_transport_t {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.size < size_of::<Self>() {
            return Err(Error::invalid(format!(
                "item transport size {} is smaller than {}",
                self.size,
                size_of::<Self>()
            )));
        }
        Ok(())
    }
}

/// Copied callback table stored on the runtime.
#[derive(Copy, Clone, Default)]
pub(crate) struct ItemTransport {
    pub(crate) on_span: otel_span_item_fn,
    pub(crate) on_log: otel_log_item_fn,
    pub(crate) on_metric: otel_metric_item_fn,
    pub(crate) on_encoded: otel_encoded_item_fn,
    pub(crate) user_data: *mut c_void,
}

impl From<otel_item_transport_t> for ItemTransport {
    fn from(value: otel_item_transport_t) -> Self {
        Self {
            on_span: value.on_span,
            on_log: value.on_log,
            on_metric: value.on_metric,
            on_encoded: value.on_encoded,
            user_data: value.user_data,
        }
    }
}

// SAFETY: the C caller keeps `user_data` valid and thread-safe until the
// runtime is released. The transport is invoked from exporter threads.
unsafe impl Send for ItemTransport {}
// SAFETY: callbacks and `user_data` are safe to invoke concurrently; the C
// caller keeps them valid until the runtime is released.
unsafe impl Sync for ItemTransport {}

impl ItemTransport {
    pub(crate) fn emit_span(&self, item: &otel_span_item_t) {
        let Some(callback) = self.on_span else {
            return;
        };
        // SAFETY: `item` is borrowed for this call and `user_data` is valid.
        unsafe { callback(item, self.user_data) };
    }

    pub(crate) fn emit_log(&self, item: &otel_log_item_t) {
        let Some(callback) = self.on_log else {
            return;
        };
        // SAFETY: `item` is borrowed for this call and `user_data` is valid.
        unsafe { callback(item, self.user_data) };
    }

    pub(crate) fn emit_metric(&self, item: &otel_metric_item_t) {
        let Some(callback) = self.on_metric else {
            return;
        };
        // SAFETY: `item` is borrowed for this call and `user_data` is valid.
        unsafe { callback(item, self.user_data) };
    }

    pub(crate) fn emit_encoded(&self, signal: otel_item_signal_t, data: &[u8]) {
        let Some(callback) = self.on_encoded else {
            return;
        };
        let item = otel_encoded_item_t {
            signal,
            data: data.as_ptr(),
            len: data.len(),
        };
        // SAFETY: `data` is borrowed for this call and `user_data` is valid.
        unsafe { callback(&item, self.user_data) };
    }
}

/// Owns C strings and builds borrowed `otel_attribute_t` views.
/// Owned storage backing the borrowed pointers in `views`.
#[derive(Default)]
pub(crate) struct AttributeViews {
    keys: Vec<CString>,
    strings: Vec<Option<CString>>,
    views: Vec<otel_attribute_t>,
}

impl AttributeViews {
    pub(crate) fn new<'a>(values: impl IntoIterator<Item = &'a KeyValue>) -> Self {
        let mut keys: Vec<CString> = Vec::new();
        let mut strings: Vec<Option<CString>> = Vec::new();
        let mut kinds: Vec<(otel_attribute_type_t, otel_attribute_value_t, Option<usize>)> =
            Vec::new();
        for kv in values {
            keys.push(CString::new(kv.key.as_str()).unwrap_or_default());
            let (kind, string) = match &kv.value {
                opentelemetry::Value::Bool(_) => {
                    (otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL, None)
                }
                opentelemetry::Value::I64(_) => {
                    (otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64, None)
                }
                opentelemetry::Value::F64(_) => {
                    (otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64, None)
                }
                opentelemetry::Value::String(value) => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING,
                    Some(CString::new(value.as_str()).unwrap_or_default()),
                ),
                other => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING,
                    Some(CString::new(other.to_string()).unwrap_or_default()),
                ),
            };
            let string_index = string.map(|owned| {
                let index = strings.len();
                strings.push(Some(owned));
                index
            });
            let value = match &kv.value {
                opentelemetry::Value::Bool(value) => otel_attribute_value_t {
                    boolean: i32::from(*value),
                },
                opentelemetry::Value::I64(value) => otel_attribute_value_t { integer: *value },
                opentelemetry::Value::F64(value) => otel_attribute_value_t {
                    double_value: *value,
                },
                _ => otel_attribute_value_t {
                    string: ptr::null(),
                },
            };
            kinds.push((kind, value, string_index));
        }

        let views = kinds
            .iter()
            .enumerate()
            .map(|(index, (kind, base_value, string_index))| {
                let mut value = *base_value;
                let len = match string_index {
                    Some(string_index) => {
                        let owned = strings[*string_index]
                            .as_ref()
                            .expect("string attribute must be stored");
                        value = otel_attribute_value_t {
                            string: owned.as_ptr(),
                        };
                        owned.as_bytes().len()
                    }
                    None => 0,
                };
                otel_attribute_t {
                    kind: *kind as i32,
                    key: keys[index].as_ptr(),
                    key_len: keys[index].as_bytes().len(),
                    value,
                    value_len: len,
                }
            })
            .collect();

        Self {
            keys,
            strings,
            views,
        }
    }

    pub(crate) fn from_any<'a>(values: impl IntoIterator<Item = &'a (Key, AnyValue)>) -> Self {
        let mut keys: Vec<CString> = Vec::new();
        let mut strings: Vec<Option<CString>> = Vec::new();
        let mut kinds: Vec<(otel_attribute_type_t, otel_attribute_value_t, Option<usize>)> =
            Vec::new();
        for (key, value) in values {
            keys.push(CString::new(key.as_str()).unwrap_or_default());
            let (kind, string, base) = match value {
                AnyValue::Boolean(value) => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL,
                    None,
                    otel_attribute_value_t {
                        boolean: i32::from(*value),
                    },
                ),
                AnyValue::Int(value) => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64,
                    None,
                    otel_attribute_value_t { integer: *value },
                ),
                AnyValue::Double(value) => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64,
                    None,
                    otel_attribute_value_t {
                        double_value: *value,
                    },
                ),
                AnyValue::String(value) => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING,
                    Some(CString::new(value.as_str()).unwrap_or_default()),
                    otel_attribute_value_t {
                        string: ptr::null(),
                    },
                ),
                other => (
                    otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING,
                    Some(CString::new(format!("{other:?}")).unwrap_or_default()),
                    otel_attribute_value_t {
                        string: ptr::null(),
                    },
                ),
            };
            let string_index = string.map(|owned| {
                let index = strings.len();
                strings.push(Some(owned));
                index
            });
            kinds.push((kind, base, string_index));
        }

        let views = kinds
            .iter()
            .enumerate()
            .map(|(index, (kind, base_value, string_index))| {
                let mut value = *base_value;
                let len = match string_index {
                    Some(string_index) => {
                        let owned = strings[*string_index]
                            .as_ref()
                            .expect("string attribute must be stored");
                        value = otel_attribute_value_t {
                            string: owned.as_ptr(),
                        };
                        owned.as_bytes().len()
                    }
                    None => 0,
                };
                otel_attribute_t {
                    kind: *kind as i32,
                    key: keys[index].as_ptr(),
                    key_len: keys[index].as_bytes().len(),
                    value,
                    value_len: len,
                }
            })
            .collect();

        Self {
            keys,
            strings,
            views,
        }
    }

    pub(crate) fn as_slice(&self) -> (*const otel_attribute_t, usize) {
        debug_assert_eq!(self.keys.len(), self.views.len());
        debug_assert!(self.strings.len() <= self.views.len());
        (self.views.as_ptr(), self.views.len())
    }
}

/// Builds a borrowed string view from an owned `CString`.
pub(crate) fn c_str_view(value: &CString) -> otel_str_t {
    otel_str_t {
        ptr: value.as_ptr(),
        len: value.as_bytes().len(),
    }
}

/// Converts a count to nanoseconds, saturating at `u64::MAX`.
pub(crate) fn nanos(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

/// Converts an SDK severity into the C severity enum.
pub(crate) fn severity(value: opentelemetry::logs::Severity) -> otel_severity_t {
    use opentelemetry::logs::Severity;
    match value {
        Severity::Trace => otel_severity_t::OTEL_SEVERITY_TRACE,
        Severity::Trace2 => otel_severity_t::OTEL_SEVERITY_TRACE,
        Severity::Trace3 => otel_severity_t::OTEL_SEVERITY_TRACE,
        Severity::Trace4 => otel_severity_t::OTEL_SEVERITY_TRACE,
        Severity::Debug => otel_severity_t::OTEL_SEVERITY_DEBUG,
        Severity::Debug2 => otel_severity_t::OTEL_SEVERITY_DEBUG,
        Severity::Debug3 => otel_severity_t::OTEL_SEVERITY_DEBUG,
        Severity::Debug4 => otel_severity_t::OTEL_SEVERITY_DEBUG,
        Severity::Info => otel_severity_t::OTEL_SEVERITY_INFO,
        Severity::Info2 => otel_severity_t::OTEL_SEVERITY_INFO,
        Severity::Info3 => otel_severity_t::OTEL_SEVERITY_INFO,
        Severity::Info4 => otel_severity_t::OTEL_SEVERITY_INFO,
        Severity::Warn => otel_severity_t::OTEL_SEVERITY_WARN,
        Severity::Warn2 => otel_severity_t::OTEL_SEVERITY_WARN,
        Severity::Warn3 => otel_severity_t::OTEL_SEVERITY_WARN,
        Severity::Warn4 => otel_severity_t::OTEL_SEVERITY_WARN,
        Severity::Error => otel_severity_t::OTEL_SEVERITY_ERROR,
        Severity::Error2 => otel_severity_t::OTEL_SEVERITY_ERROR,
        Severity::Error3 => otel_severity_t::OTEL_SEVERITY_ERROR,
        Severity::Error4 => otel_severity_t::OTEL_SEVERITY_ERROR,
        Severity::Fatal => otel_severity_t::OTEL_SEVERITY_FATAL,
        Severity::Fatal2 => otel_severity_t::OTEL_SEVERITY_FATAL,
        Severity::Fatal3 => otel_severity_t::OTEL_SEVERITY_FATAL,
        Severity::Fatal4 => otel_severity_t::OTEL_SEVERITY_FATAL,
    }
}

/// Converts an SDK span kind into the C span kind enum.
pub(crate) fn span_kind(kind: opentelemetry::trace::SpanKind) -> otel_span_kind_t {
    use opentelemetry::trace::SpanKind;
    match kind {
        SpanKind::Internal => otel_span_kind_t::OTEL_SPAN_KIND_INTERNAL,
        SpanKind::Server => otel_span_kind_t::OTEL_SPAN_KIND_SERVER,
        SpanKind::Client => otel_span_kind_t::OTEL_SPAN_KIND_CLIENT,
        SpanKind::Producer => otel_span_kind_t::OTEL_SPAN_KIND_PRODUCER,
        SpanKind::Consumer => otel_span_kind_t::OTEL_SPAN_KIND_CONSUMER,
    }
}

/// Converts a 16-byte trace ID into the C struct.
pub(crate) fn trace_id(bytes: [u8; 16]) -> otel_trace_id_t {
    let mut hi = [0u8; 8];
    let mut lo = [0u8; 8];
    hi.copy_from_slice(&bytes[..8]);
    lo.copy_from_slice(&bytes[8..]);
    otel_trace_id_t {
        hi: u64::from_be_bytes(hi),
        lo: u64::from_be_bytes(lo),
    }
}

/// Converts an 8-byte span ID into the C struct.
pub(crate) fn span_id(bytes: [u8; 8]) -> otel_span_id_t {
    otel_span_id_t {
        value: u64::from_be_bytes(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::KeyValue;

    #[test]
    fn attribute_views_keep_strings_alive() {
        let values = [
            KeyValue::new("text", "value"),
            KeyValue::new("flag", true),
            KeyValue::new("count", 7i64),
        ];
        let view = AttributeViews::new(values.iter());
        let (ptr, count) = view.as_slice();
        assert_eq!(count, 3);
        // SAFETY: the views stay alive until `view` is dropped.
        let attrs = unsafe { std::slice::from_raw_parts(ptr, count) };
        assert_eq!(
            attrs[0].kind,
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32
        );
        assert_eq!(attrs[0].key_len, 4);
        assert_eq!(attrs[0].value_len, 5);
        // SAFETY: kind selects the string member.
        let text = unsafe {
            std::str::from_utf8(std::slice::from_raw_parts(
                attrs[0].value.string.cast::<u8>(),
                attrs[0].value_len,
            ))
            .unwrap()
        };
        assert_eq!(text, "value");
        assert_eq!(
            attrs[1].kind,
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL as i32
        );
        // SAFETY: kind selects the boolean member.
        assert_eq!(unsafe { attrs[1].value.boolean }, 1);
        assert_eq!(
            attrs[2].kind,
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64 as i32
        );
        // SAFETY: kind selects the integer member.
        assert_eq!(unsafe { attrs[2].value.integer }, 7);
    }

    #[test]
    fn item_transport_rejects_short_struct() {
        let value = otel_item_transport_t {
            size: 1,
            on_span: None,
            on_log: None,
            on_metric: None,
            on_encoded: None,
            user_data: ptr::null_mut(),
        };
        assert!(value.validate().is_err());
    }

    #[test]
    fn severity_maps_levels() {
        assert_eq!(
            severity(opentelemetry::logs::Severity::Info2),
            otel_severity_t::OTEL_SEVERITY_INFO
        );
        assert_eq!(
            severity(opentelemetry::logs::Severity::Fatal3),
            otel_severity_t::OTEL_SEVERITY_FATAL
        );
    }

    #[test]
    fn signal_values_match_abi() {
        assert_eq!(otel_item_signal_t::OTEL_ITEM_SIGNAL_TRACE as i32, 0);
        assert_eq!(otel_item_signal_t::OTEL_ITEM_SIGNAL_METRIC as i32, 1);
        assert_eq!(otel_item_signal_t::OTEL_ITEM_SIGNAL_LOG as i32, 2);
    }

    #[test]
    fn transport_validates_size() {
        let value = otel_item_transport_t {
            size: size_of::<otel_item_transport_t>(),
            on_span: None,
            on_log: None,
            on_metric: None,
            on_encoded: None,
            user_data: ptr::null_mut(),
        };
        assert!(value.validate().is_ok());
        let short = otel_item_transport_t { size: 1, ..value };
        assert_eq!(
            short.validate().unwrap_err().status,
            crate::error::otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
    struct Counts {
        span: usize,
        log: usize,
        metric: usize,
        encoded: usize,
        signal: i32,
        len: usize,
    }

    unsafe extern "C" fn count_span(_item: *const otel_span_item_t, user_data: *mut c_void) {
        // SAFETY: `user_data` points to a live `Cell<Counts>` for this call.
        unsafe {
            let counts = &*user_data.cast::<std::cell::Cell<Counts>>();
            let mut value = counts.get();
            value.span += 1;
            counts.set(value);
        }
    }

    unsafe extern "C" fn count_log(_item: *const otel_log_item_t, user_data: *mut c_void) {
        // SAFETY: `user_data` points to a live `Cell<Counts>` for this call.
        unsafe {
            let counts = &*user_data.cast::<std::cell::Cell<Counts>>();
            let mut value = counts.get();
            value.log += 1;
            counts.set(value);
        }
    }

    unsafe extern "C" fn count_metric(_item: *const otel_metric_item_t, user_data: *mut c_void) {
        // SAFETY: `user_data` points to a live `Cell<Counts>` for this call.
        unsafe {
            let counts = &*user_data.cast::<std::cell::Cell<Counts>>();
            let mut value = counts.get();
            value.metric += 1;
            counts.set(value);
        }
    }

    unsafe extern "C" fn count_encoded(item: *const otel_encoded_item_t, user_data: *mut c_void) {
        // SAFETY: `item` and `user_data` are live for the duration of the callback.
        unsafe {
            let item = &*item;
            let counts = &*user_data.cast::<std::cell::Cell<Counts>>();
            let mut value = counts.get();
            value.encoded += 1;
            value.signal = item.signal as i32;
            value.len = item.len;
            counts.set(value);
        }
    }

    fn empty_str() -> otel_str_t {
        otel_str_t {
            ptr: ptr::null(),
            len: 0,
        }
    }

    fn sample_span() -> otel_span_item_t {
        otel_span_item_t {
            trace_id: otel_trace_id_t::default(),
            span_id: otel_span_id_t::default(),
            parent_span_id: otel_span_id_t::default(),
            name: empty_str(),
            kind: otel_span_kind_t::OTEL_SPAN_KIND_INTERNAL,
            status: otel_span_status_t::OTEL_SPAN_STATUS_UNSET,
            status_message: empty_str(),
            start_time_unix_nano: 0,
            end_time_unix_nano: 0,
            attributes: ptr::null(),
            attribute_count: 0,
            events: ptr::null(),
            event_count: 0,
            links: ptr::null(),
            link_count: 0,
            resource_attributes: ptr::null(),
            resource_attribute_count: 0,
        }
    }

    fn sample_log() -> otel_log_item_t {
        otel_log_item_t {
            timestamp_unix_nano: 0,
            observed_timestamp_unix_nano: 0,
            severity: otel_severity_t::OTEL_SEVERITY_INFO,
            severity_text: empty_str(),
            body: empty_str(),
            trace_id: otel_trace_id_t::default(),
            span_id: otel_span_id_t::default(),
            attributes: ptr::null(),
            attribute_count: 0,
            resource_attributes: ptr::null(),
            resource_attribute_count: 0,
        }
    }

    fn sample_metric() -> otel_metric_item_t {
        otel_metric_item_t {
            name: empty_str(),
            description: empty_str(),
            unit: empty_str(),
            kind: otel_metric_item_type_t::OTEL_METRIC_ITEM_GAUGE,
            points: ptr::null(),
            point_count: 0,
            histograms: ptr::null(),
            histogram_count: 0,
            resource_attributes: ptr::null(),
            resource_attribute_count: 0,
        }
    }

    #[test]
    fn transport_dispatches_callbacks() {
        let counts = std::cell::Cell::new(Counts::default());
        let user_data = (&counts as *const std::cell::Cell<Counts>)
            .cast_mut()
            .cast::<c_void>();
        let transport = otel_item_transport_t {
            size: size_of::<otel_item_transport_t>(),
            on_span: Some(count_span),
            on_log: Some(count_log),
            on_metric: Some(count_metric),
            on_encoded: Some(count_encoded),
            user_data,
        };
        let transport = ItemTransport::from(transport);
        let span = sample_span();
        let log = sample_log();
        let metric = sample_metric();
        transport.emit_span(&span);
        transport.emit_log(&log);
        transport.emit_metric(&metric);
        transport.emit_encoded(otel_item_signal_t::OTEL_ITEM_SIGNAL_LOG, &[1, 2, 3]);
        assert_eq!(
            counts.get(),
            Counts {
                span: 1,
                log: 1,
                metric: 1,
                encoded: 1,
                signal: 2,
                len: 3,
            }
        );

        counts.set(Counts::default());
        let empty = ItemTransport::default();
        empty.emit_span(&span);
        empty.emit_log(&log);
        empty.emit_metric(&metric);
        empty.emit_encoded(otel_item_signal_t::OTEL_ITEM_SIGNAL_TRACE, &[]);
        assert_eq!(counts.get(), Counts::default());
    }

    #[test]
    fn attribute_views_from_any_cover_all_value_kinds() {
        use opentelemetry::{Key, logs::AnyValue};
        let values = [
            (Key::new("bool"), AnyValue::Boolean(true)),
            (Key::new("int"), AnyValue::Int(-7)),
            (Key::new("double"), AnyValue::Double(1.5)),
            (Key::new("string"), AnyValue::String("hello".into())),
            (
                Key::new("bytes"),
                AnyValue::Bytes(Box::new(vec![b'a', b'b'])),
            ),
            (Key::new("bad\0key"), AnyValue::String("v".into())),
        ];
        let views = AttributeViews::from_any(values.iter());
        let (ptr, len) = views.as_slice();
        assert_eq!(len, values.len());
        // SAFETY: `views` owns the strings backing these pointers and outlives the slice.
        let attrs = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert_eq!(
            attrs[0].kind,
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL as i32
        );
        // SAFETY: the boolean member is selected by the attribute kind.
        assert_eq!(unsafe { attrs[0].value.boolean }, 1);
        // SAFETY: the integer member is selected by the attribute kind.
        assert_eq!(unsafe { attrs[1].value.integer }, -7);
        // SAFETY: the double member is selected by the attribute kind.
        assert_eq!(unsafe { attrs[2].value.double_value }, 1.5);
        assert_eq!(attrs[3].value_len, 5);
        // SAFETY: the string member is selected by the attribute kind.
        let text = unsafe {
            std::str::from_utf8(std::slice::from_raw_parts(
                attrs[3].value.string.cast::<u8>(),
                attrs[3].value_len,
            ))
            .unwrap()
        };
        assert_eq!(text, "hello");
        assert_eq!(
            attrs[4].kind,
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32
        );
        assert!(attrs[4].value_len > 0);
        assert_eq!(attrs[5].key_len, 0);
    }

    #[test]
    fn string_views_and_timestamp_helpers() {
        let owned = std::ffi::CString::new("abc").unwrap();
        let view = c_str_view(&owned);
        assert_eq!(view.len, 3);
        // SAFETY: `owned` outlives the view and holds at least `len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(view.ptr.cast::<u8>(), view.len) };
        assert_eq!(bytes, b"abc");

        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(nanos(epoch), 0);
        assert_eq!(
            nanos(epoch + std::time::Duration::new(1, 42)),
            1_000_000_042
        );
        assert_eq!(nanos(epoch - std::time::Duration::from_secs(1)), 0);
    }

    #[test]
    fn severity_and_span_kind_cover_every_variant() {
        use opentelemetry::logs::Severity;
        for value in [
            Severity::Trace,
            Severity::Trace2,
            Severity::Trace3,
            Severity::Trace4,
        ] {
            assert_eq!(severity(value), otel_severity_t::OTEL_SEVERITY_TRACE);
        }
        for value in [
            Severity::Debug,
            Severity::Debug2,
            Severity::Debug3,
            Severity::Debug4,
        ] {
            assert_eq!(severity(value), otel_severity_t::OTEL_SEVERITY_DEBUG);
        }
        for value in [
            Severity::Warn,
            Severity::Warn2,
            Severity::Warn3,
            Severity::Warn4,
        ] {
            assert_eq!(severity(value), otel_severity_t::OTEL_SEVERITY_WARN);
        }
        for value in [
            Severity::Error,
            Severity::Error2,
            Severity::Error3,
            Severity::Error4,
        ] {
            assert_eq!(severity(value), otel_severity_t::OTEL_SEVERITY_ERROR);
        }
        for value in [Severity::Fatal, Severity::Fatal2, Severity::Fatal4] {
            assert_eq!(severity(value), otel_severity_t::OTEL_SEVERITY_FATAL);
        }
        use opentelemetry::trace::SpanKind;
        assert_eq!(
            span_kind(SpanKind::Internal),
            otel_span_kind_t::OTEL_SPAN_KIND_INTERNAL
        );
        assert_eq!(
            span_kind(SpanKind::Server),
            otel_span_kind_t::OTEL_SPAN_KIND_SERVER
        );
        assert_eq!(
            span_kind(SpanKind::Client),
            otel_span_kind_t::OTEL_SPAN_KIND_CLIENT
        );
        assert_eq!(
            span_kind(SpanKind::Producer),
            otel_span_kind_t::OTEL_SPAN_KIND_PRODUCER
        );
        assert_eq!(
            span_kind(SpanKind::Consumer),
            otel_span_kind_t::OTEL_SPAN_KIND_CONSUMER
        );
    }

    #[test]
    fn id_helpers_use_big_endian_bytes() {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        bytes[8..].copy_from_slice(&0x1112_1314_1516_1718u64.to_be_bytes());
        let trace = trace_id(bytes);
        assert_eq!(trace.hi, 0x0102_0304_0506_0708);
        assert_eq!(trace.lo, 0x1112_1314_1516_1718);
        assert_eq!(
            span_id([1, 2, 3, 4, 5, 6, 7, 8]).value,
            0x0102_0304_0506_0708
        );
    }
}
