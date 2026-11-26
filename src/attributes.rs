//! C-friendly tagged attributes and variadic attribute parsing.
//!
//! `otel_attribute_t` is a tagged union: `kind` selects which member of
//! `value` is active. C callers use the `otel_attr_*` macros to initialize it
//! and pass a `NULL`-terminated list to any function that accepts attributes.

use std::ffi::{VaList, c_char, c_int};

use opentelemetry::{Key, KeyValue, Value};

use crate::error::{Error, Result, c_str, utf8};

/// Discriminator for the active member of [`otel_attribute_value_t`].
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_attribute_type_t {
    /// The `string` member is active.
    OTEL_ATTRIBUTE_TYPE_STRING = 0,
    /// The `boolean` member is active.
    OTEL_ATTRIBUTE_TYPE_BOOL = 1,
    /// The `integer` member is active.
    OTEL_ATTRIBUTE_TYPE_I64 = 2,
    /// The `double_value` member is active.
    OTEL_ATTRIBUTE_TYPE_F64 = 3,
}

/// Tagged value payload. Only the member named by `kind` may be read.
#[repr(C)]
#[derive(Copy, Clone)]
pub union otel_attribute_value_t {
    /// Borrowed NUL-terminated UTF-8 string.
    pub string: *const c_char,
    /// Boolean stored as zero or non-zero.
    pub boolean: c_int,
    /// Signed 64-bit integer.
    pub integer: i64,
    /// Double-precision float.
    pub double_value: f64,
}

/// A tagged attribute. Use the `otel_attr_*` macros to initialize it.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_attribute_t {
    /// Which member of `value` is active.
    pub kind: i32,
    /// NUL-terminated UTF-8 attribute key. Must not be NULL.
    pub key: *const c_char,
    /// Tagged value payload.
    pub value: otel_attribute_value_t,
}

impl otel_attribute_t {
    /// Converts one C attribute into an OpenTelemetry `KeyValue`.
    ///
    /// # Safety
    /// `key` and the active string pointer must reference NUL-terminated
    /// strings that remain valid for the duration of the call. `kind` must
    /// name the union member that was initialized.
    pub(crate) unsafe fn to_key_value(&self) -> Result<KeyValue> {
        let key = unsafe { c_str(self.key, "attribute key") }?;
        let key = Key::new(utf8(key, "attribute key")?);
        let value = match self.kind {
            0 => {
                // SAFETY: caller guarantees `string` is active and valid.
                let raw = unsafe { c_str(self.value.string, "string attribute value") }?;
                Value::String(utf8(raw, "string attribute value")?.into())
            }
            1 => {
                // SAFETY: caller guarantees `boolean` is active.
                Value::Bool(unsafe { self.value.boolean } != 0)
            }
            2 => {
                // SAFETY: caller guarantees `integer` is active.
                Value::I64(unsafe { self.value.integer })
            }
            3 => {
                // SAFETY: caller guarantees `double_value` is active.
                Value::F64(unsafe { self.value.double_value })
            }
            other => {
                return Err(Error::invalid(format!("invalid attribute type {other}")));
            }
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
            kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
            key: key.as_ptr(),
            value: otel_attribute_value_t {
                string: value.as_ptr(),
            },
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
        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_BOOL as i32;
        attr.value = otel_attribute_value_t { boolean: 1 };
        assert_eq!(
            unsafe { attr.to_key_value() }.unwrap().value,
            Value::Bool(true)
        );

        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_I64 as i32;
        attr.value = otel_attribute_value_t { integer: -42 };
        assert_eq!(unsafe { attr.to_key_value() }.unwrap().value, Value::I64(-42));

        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64 as i32;
        attr.value = otel_attribute_value_t { double_value: 1.5 };
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
