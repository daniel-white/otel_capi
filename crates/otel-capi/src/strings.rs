//! Length-aware string views for the C ABI.

use std::ffi::CString;
use std::slice;
use std::str;

pub use crate::abi::otel_str_t;
use crate::error::{Error, otel_result_t};

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
    // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
    Ok(Some(unsafe { str_slice(value, name)? }))
}

/// Builds an owned C string for a length-aware view, for forwarding to an
/// existing NUL-terminated API. NULL with zero length yields `None`.
pub(crate) unsafe fn owned_c_string(
    value: otel_str_t,
    name: &str,
) -> crate::error::Result<Option<CString>> {
    // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
    let Some(text) = (unsafe { optional_str_slice(value, name)? }) else {
        return Ok(None);
    };
    CString::new(text)
        .map(Some)
        .map_err(|_| Error::invalid(format!("{name} must not contain NUL bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn view(bytes: &[u8]) -> otel_str_t {
        otel_str_t {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        }
    }

    #[test]
    fn str_slice_borrows_valid_utf8() {
        let text = b"hello";
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { str_slice(view(text), "value") }.unwrap(),
            "hello"
        );
    }

    #[test]
    fn str_slice_allows_embedded_nul() {
        let text = b"a\0b";
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let borrowed = unsafe { str_slice(view(text), "value") }.unwrap();
        assert_eq!(borrowed, "a\0b");
        assert_eq!(borrowed.len(), 3);
    }

    #[test]
    fn str_slice_rejects_null_and_invalid_utf8() {
        let null = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let error = unsafe { str_slice(null, "value") }.unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let error = unsafe { str_slice(view(&[0xff, 0xfe]), "value") }.unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_UTF8);
    }

    #[test]
    fn optional_str_slice_distinguishes_null_from_empty() {
        let null = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { optional_str_slice(null, "value") }.unwrap(),
            None
        );

        let empty = b"";
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { optional_str_slice(view(empty), "value") }.unwrap(),
            Some("")
        );

        let null_with_length = otel_str_t {
            ptr: ptr::null(),
            len: 1,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { optional_str_slice(null_with_length, "value") }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn owned_c_string_handles_null_empty_and_invalid_input() {
        let null = otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { owned_c_string(null, "value") }.unwrap(),
            None
        );

        let empty = b"";
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let owned = unsafe { owned_c_string(view(empty), "value") }
            .unwrap()
            .unwrap();
        assert_eq!(owned.to_bytes(), b"");

        let text = b"abc";
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let owned = unsafe { owned_c_string(view(text), "value") }
            .unwrap()
            .unwrap();
        assert_eq!(owned.to_bytes(), b"abc");

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let error = unsafe { owned_c_string(view(&[0xff]), "value") }.unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_UTF8);

        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let error = unsafe { owned_c_string(view(b"a\0b"), "value") }.unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
    }
}
