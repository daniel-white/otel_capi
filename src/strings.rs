//! Length-aware string views for the C ABI.

use std::ffi::{CString, c_char};
use std::slice;
use std::str;

use crate::error::{Error, otel_result_t};

/// A borrowed UTF-8 byte range. Unlike `const char *`, this does not require a
/// NUL terminator and can therefore represent embedded bytes safely.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_str_t {
    pub ptr: *const c_char,
    pub len: usize,
}

/// Borrows a required string view. The pointer must be non-NULL for `len` bytes.
pub(crate) unsafe fn str_slice<'a>(value: otel_str_t, name: &str) -> crate::error::Result<&'a str> {
    if value.ptr.is_null() {
        return Err(Error::invalid(format!("{name} must not be NULL")));
    }
    // SAFETY: the caller guarantees `ptr` points to at least `len` readable
    // bytes that remain valid for the duration of the call.
    let bytes = unsafe { slice::from_raw_parts(value.ptr.cast::<u8>(), value.len) };
    str::from_utf8(bytes).map_err(|_| {
        Error::new(
            otel_result_t::OTEL_ERR_INVALID_UTF8,
            format!("{name} is not valid UTF-8"),
        )
    })
}

/// Borrows an optional string view. NULL with zero length yields `None`.
pub(crate) unsafe fn optional_str_slice<'a>(
    value: otel_str_t,
    name: &str,
) -> crate::error::Result<Option<&'a str>> {
    if value.ptr.is_null() && value.len == 0 {
        return Ok(None);
    }
    Ok(Some(unsafe { str_slice(value, name)? }))
}

/// Builds an owned C string for a length-aware view, for forwarding to an
/// existing NUL-terminated API. NULL with zero length yields `None`.
pub(crate) unsafe fn owned_c_string(
    value: otel_str_t,
    name: &str,
) -> crate::error::Result<Option<CString>> {
    let Some(text) = (unsafe { optional_str_slice(value, name)? }) else {
        return Ok(None);
    };
    CString::new(text)
        .map(Some)
        .map_err(|_| Error::invalid(format!("{name} must not contain NUL bytes")))
}
