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
}
