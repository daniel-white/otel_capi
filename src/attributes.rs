//! C-friendly attributes and variadic attribute parsing.
//!
//! C callers build attributes with the `otel_attr_*` macros defined at the
//! bottom of the generated header and pass a `NULL`-terminated list to any
//! function that accepts attributes.

use std::ffi::{VaList, c_char, c_int};

use opentelemetry::{Key, KeyValue, Value};

use crate::error::{Error, Result, c_str, utf8};

/// Discriminator for the value stored in an [`otel_attribute_t`].
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_attribute_type_t {
    /// A borrowed NUL-terminated UTF-8 string in `string_value`.
    OTEL_ATTRIBUTE_TYPE_STRING,
    /// A boolean stored as zero or non-zero in `bool_value`.
    OTEL_ATTRIBUTE_TYPE_BOOL,
    /// A signed 64-bit integer in `int_value`.
    OTEL_ATTRIBUTE_TYPE_I64,
    /// A double-precision float in `double_value`.
    OTEL_ATTRIBUTE_TYPE_F64,
}

/// A single attribute. Use the `otel_attr_*` macros to initialize it.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_attribute_t {
    /// Which value field is valid.
    pub kind: otel_attribute_type_t,
    /// NUL-terminated UTF-8 attribute key. Must not be NULL.
    pub key: *const c_char,
    /// String value, used only for `OTEL_ATTRIBUTE_TYPE_STRING`.
    pub string_value: *const c_char,
    /// Boolean value, used only for `OTEL_ATTRIBUTE_TYPE_BOOL`.
    pub bool_value: c_int,
    /// Integer value, used only for `OTEL_ATTRIBUTE_TYPE_I64`.
    pub int_value: i64,
    /// Float value, used only for `OTEL_ATTRIBUTE_TYPE_F64`.
    pub double_value: f64,
}

impl otel_attribute_t {
    /// Converts one C attribute into an OpenTelemetry `KeyValue`.
    ///
    /// # Safety
    /// `key` and the relevant value pointer must reference NUL-terminated
    /// strings that remain valid for the duration of the call.
    pub(crate) unsafe fn to_key_value(&self) -> Result<KeyValue> {
        let key = unsafe { c_str(self.key, "attribute key") }?;
        let key = Key::new(utf8(key, "attribute key")?);
        let value = match self.kind {
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING => {
                let raw = unsafe { c_str(self.string_value, "string attribute value") }?;
                Value::String(utf8(raw, "string attribute value")?.into())
            }
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL => Value::Bool(self.bool_value != 0),
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64 => Value::I64(self.int_value),
            otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64 => Value::F64(self.double_value),
        };
        Ok(KeyValue::new(key, value))
    }
}

/// Consumes a `NULL`-terminated variadic list of attribute pointers.
///
/// # Safety
/// Every non-NULL pointer must reference a live, initialized
/// [`otel_attribute_t`] for the duration of the call.
pub(crate) unsafe fn from_varargs(mut args: VaList) -> Result<Vec<KeyValue>> {
    let mut attributes = Vec::new();
    loop {
        // SAFETY: caller guarantees each argument is an attribute pointer and
        // that the list is terminated by NULL.
        let raw = unsafe { args.next_arg::<*const otel_attribute_t>() };
        if raw.is_null() {
            return Ok(attributes);
        }
        if attributes.len() >= 128 {
            return Err(Error::invalid("at most 128 attributes are supported"));
        }
        // SAFETY: caller guarantees `raw` points to a live attribute.
        let attribute = unsafe { &*raw };
        attributes.push(unsafe { attribute.to_key_value()? });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(key: &std::ffi::CStr, value: &std::ffi::CStr) -> otel_attribute_t {
        otel_attribute_t {
            kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING,
            key: key.as_ptr(),
            string_value: value.as_ptr(),
            bool_value: 0,
            int_value: 0,
            double_value: 0.0,
        }
    }

    #[test]
    fn converts_string_attribute() {
        let attr = attribute(c"service.name", c"checkout");
        let kv = unsafe { attr.to_key_value() }.unwrap();
        assert_eq!(kv.key.as_str(), "service.name");
        assert_eq!(kv.value.to_string(), "checkout");
    }

    #[test]
    fn converts_numeric_and_boolean_attributes() {
        let mut attr = attribute(c"k", c"unused");
        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL;
        attr.bool_value = 1;
        assert_eq!(
            unsafe { attr.to_key_value() }.unwrap().value,
            Value::Bool(true)
        );

        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64;
        attr.int_value = -42;
        assert_eq!(unsafe { attr.to_key_value() }.unwrap().value, Value::I64(-42));

        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64;
        attr.double_value = 1.5;
        assert_eq!(unsafe { attr.to_key_value() }.unwrap().value, Value::F64(1.5));
    }

    #[test]
    fn rejects_null_key() {
        let attr = otel_attribute_t {
            key: std::ptr::null(),
            ..attribute(c"k", c"v")
        };
        assert!(unsafe { attr.to_key_value() }.is_err());
    }
}
