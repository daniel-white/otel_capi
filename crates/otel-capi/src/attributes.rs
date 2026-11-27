//! C-friendly tagged attributes and variadic attribute parsing.
//!
//! `otel_attribute_t` is a tagged union: `kind` selects which member of
//! `value` is active. C callers use the `otel_attr_*` macros to initialize it
//! and pass an `OTEL_ATTR_END`-terminated list to any function that accepts
//! attributes.

use std::ffi::{VaList, c_char, c_int};

use opentelemetry::{Key, KeyValue, Value};

use crate::error::{Error, Result};
use crate::strings::{otel_str_t, str_slice};

/// Maximum number of attributes accepted by one call.
pub(crate) const MAX_ATTRIBUTES: usize = 1024;

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
    /// Borrowed UTF-8 string bytes; length is in `otel_attribute_t::value_len`.
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
    /// UTF-8 attribute key bytes. Must not be NULL.
    pub key: *const c_char,
    /// Number of bytes in `key`.
    pub key_len: usize,
    /// Tagged value payload.
    pub value: otel_attribute_value_t,
    /// Number of bytes in the string value, when `kind` is string.
    pub value_len: usize,
}

impl otel_attribute_t {
    /// Converts one C attribute into an OpenTelemetry `KeyValue`.
    ///
    /// # Safety
    /// `key` and the active string pointer must reference `key_len` and
    /// `value_len` readable bytes respectively, valid for the duration of the
    /// call. `kind` must name the union member that was initialized.
    pub(crate) unsafe fn to_key_value(&self) -> Result<KeyValue> {
        let key = unsafe {
            str_slice(
                otel_str_t {
                    ptr: self.key,
                    len: self.key_len,
                },
                "attribute key",
            )?
        };
        let key = Key::new(key.to_owned());
        let value = match self.kind {
            0 => {
                // SAFETY: caller guarantees `string` is active and valid.
                let text = unsafe {
                    str_slice(
                        otel_str_t {
                            ptr: self.value.string,
                            len: self.value_len,
                        },
                        "string attribute value",
                    )?
                };
                Value::String(text.to_owned().into())
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

/// Consumes an `OTEL_ATTR_END`-terminated variadic list of attribute pointers.
///
/// # Safety
/// Every non-NULL argument must point to an initialized [`otel_attribute_t`],
/// and its borrowed strings must stay valid for the duration of the call.
pub(crate) unsafe fn from_varargs(mut args: VaList) -> Result<Vec<KeyValue>> {
    let mut attributes = Vec::new();
    loop {
        // SAFETY: caller guarantees each argument is an attribute pointer and
        // that the list is terminated by an `OTEL_ATTR_END` sentinel.
        let raw = unsafe { args.next_arg::<*const otel_attribute_t>() };
        if raw.is_null() {
            return Ok(attributes);
        }
        if attributes.len() >= MAX_ATTRIBUTES {
            return Err(Error::invalid(format!(
                "at most {MAX_ATTRIBUTES} attributes are supported"
            )));
        }
        // SAFETY: caller guarantees `raw` points to a live attribute.
        let attribute = unsafe { &*raw };
        attributes.push(unsafe { attribute.to_key_value()? });
    }
}

/// Converts a counted array of attribute pointers into OpenTelemetry
/// `KeyValue`s. This is the array-based counterpart of [`from_varargs`].
///
/// # Safety
/// When `count` is non-zero, `attrs` must point to `count` initialized
/// attribute pointers. Each attribute's borrowed strings must stay valid for
/// the duration of the call.
pub(crate) unsafe fn from_array(
    attrs: *const *const otel_attribute_t,
    count: usize,
) -> Result<Vec<KeyValue>> {
    if count > MAX_ATTRIBUTES {
        return Err(Error::invalid(format!(
            "at most {MAX_ATTRIBUTES} attributes are supported"
        )));
    }
    if count > 0 && attrs.is_null() {
        return Err(Error::invalid("attribute array must not be NULL"));
    }
    let mut attributes = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: caller guarantees `attrs` holds `count` attribute pointers.
        let raw = unsafe { *attrs.add(index) };
        if raw.is_null() {
            return Err(Error::invalid("attribute array contains a NULL entry"));
        }
        // SAFETY: caller guarantees `raw` points to a live attribute.
        attributes.push(unsafe { (*raw).to_key_value()? });
    }
    Ok(attributes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(key: &std::ffi::CStr, value: &std::ffi::CStr) -> otel_attribute_t {
        otel_attribute_t {
            kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
            key: key.as_ptr(),
            key_len: key.to_bytes().len(),
            value: otel_attribute_value_t {
                string: value.as_ptr(),
            },
            value_len: value.to_bytes().len(),
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
        assert_eq!(
            unsafe { attr.to_key_value() }.unwrap().value,
            Value::I64(-42)
        );

        attr.kind = otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_F64 as i32;
        attr.value = otel_attribute_value_t { double_value: 1.5 };
        assert_eq!(
            unsafe { attr.to_key_value() }.unwrap().value,
            Value::F64(1.5)
        );
    }

    #[test]
    fn converts_attribute_array() {
        let first = attribute(c"first", c"one");
        let second = attribute(c"second", c"two");
        let ptrs = [&first as *const _, &second as *const _];
        let values = unsafe { from_array(ptrs.as_ptr(), ptrs.len()) }.unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].key.as_str(), "first");
        assert_eq!(values[1].value.to_string(), "two");
    }

    #[test]
    fn rejects_null_and_oversized_attribute_arrays() {
        assert!(unsafe { from_array(std::ptr::null(), 1) }.is_err());
        assert!(unsafe { from_array(std::ptr::null(), MAX_ATTRIBUTES + 1) }.is_err());
    }

    #[test]
    fn rejects_invalid_kind() {
        let mut attr = attribute(c"k", c"v");
        attr.kind = 99;
        let error = unsafe { attr.to_key_value() }.unwrap_err();
        assert_eq!(
            error.status,
            crate::error::otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
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
