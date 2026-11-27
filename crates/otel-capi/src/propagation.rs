//! Strongly typed buffers for W3C propagation values.
//!
//! Each type owns fixed-size storage sized for its header, so C callers cannot
//! accidentally pass a baggage value where a traceparent is expected.

use std::ffi::c_char;
use std::ptr;
use std::slice;
use std::str;

use crate::error::{Error, Result, c_str, guard, otel_result_t, utf8};
use crate::strings::otel_str_t;

/// Full `traceparent` header: 55 characters plus a NUL terminator.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_traceparent_t {
    pub value: [c_char; 56],
}

/// Full `tracestate` header: 512 characters plus a NUL terminator.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_tracestate_t {
    pub value: [c_char; 513],
}

/// Full `baggage` header. The 1024-byte cap keeps it usable as stack storage.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_baggage_t {
    pub value: [c_char; 1025],
}

/// A single baggage value, capped at 256 bytes to keep it stack-friendly.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct otel_baggage_value_t {
    pub value: [c_char; 257],
}

pub(crate) trait TypedBuffer {
    const NAME: &'static str;
    const CAPACITY: usize;

    fn bytes(&self) -> &[c_char];
    fn bytes_mut(&mut self) -> &mut [c_char];

    fn validate_text(_text: &str) -> Result<()> {
        Ok(())
    }

    fn text(&self) -> Result<&str> {
        let bytes = self.bytes();
        let len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| Error::invalid(format!("{} is not NUL-terminated", Self::NAME)))?;
        // SAFETY: `c_char` and `u8` have the same layout, and the slice is
        // bounded by the first NUL byte.
        let raw = unsafe { slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), len) };
        str::from_utf8(raw).map_err(|_| {
            Error::new(
                otel_result_t::OTEL_ERR_INVALID_UTF8,
                format!("{} is not valid UTF-8", Self::NAME),
            )
        })
    }

    fn validate(&self) -> Result<()> {
        Self::validate_text(self.text()?)
    }
}

impl TypedBuffer for otel_traceparent_t {
    const NAME: &'static str = "traceparent";
    const CAPACITY: usize = 56;

    fn bytes(&self) -> &[c_char] {
        &self.value
    }

    fn bytes_mut(&mut self) -> &mut [c_char] {
        &mut self.value
    }

    fn validate_text(text: &str) -> Result<()> {
        let bytes = text.as_bytes();
        let hex = |range: &[u8]| range.iter().all(|byte| byte.is_ascii_hexdigit());
        if bytes.len() != 55
            || &bytes[..2] != b"00"
            || bytes[2] != b'-'
            || !hex(&bytes[3..35])
            || bytes[35] != b'-'
            || !hex(&bytes[36..52])
            || bytes[52] != b'-'
            || !hex(&bytes[53..55])
        {
            return Err(Error::invalid(
                "traceparent must be 00-<32 hex trace id>-<16 hex span id>-<2 hex flags>",
            ));
        }
        Ok(())
    }
}

macro_rules! impl_typed_buffer {
    ($ty:ty, $name:literal, $capacity:expr) => {
        impl TypedBuffer for $ty {
            const NAME: &'static str = $name;
            const CAPACITY: usize = $capacity;

            fn bytes(&self) -> &[c_char] {
                &self.value
            }

            fn bytes_mut(&mut self) -> &mut [c_char] {
                &mut self.value
            }
        }
    };
}

impl_typed_buffer!(otel_tracestate_t, "tracestate", 513);
impl_typed_buffer!(otel_baggage_t, "baggage", 1025);
impl_typed_buffer!(otel_baggage_value_t, "baggage value", 257);

unsafe fn fill<T: TypedBuffer>(src: *const c_char, out: *mut T) -> otel_result_t {
    guard(|| {
        if out.is_null() {
            return Err(Error::invalid(format!(
                "{} output must not be NULL",
                T::NAME
            )));
        }
        // SAFETY: The caller guarantees the string pointer is valid and its declared length or NUL terminator is accurate.
        let src = unsafe { c_str(src, T::NAME)? };
        let text = utf8(src, T::NAME)?;
        T::validate_text(&text)?;
        let bytes = text.as_bytes();
        if bytes.len() >= T::CAPACITY {
            return Err(Error::invalid(format!(
                "{} is too long (max {} bytes)",
                T::NAME,
                T::CAPACITY - 1
            )));
        }
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        let out = unsafe { &mut *out };
        let dest = out.bytes_mut();
        // SAFETY: `c_char` and `u8` have the same layout.
        let source = unsafe { slice::from_raw_parts(bytes.as_ptr().cast::<c_char>(), bytes.len()) };
        dest[..bytes.len()].copy_from_slice(source);
        dest[bytes.len()] = 0;
        Ok(())
    })
}

pub(crate) unsafe fn buffer_utf8<T: TypedBuffer>(value: *const T) -> Result<String> {
    if value.is_null() {
        return Err(Error::invalid(format!("{} must not be NULL", T::NAME)));
    }
    // SAFETY: the caller guarantees `value` points to a typed buffer that
    // remains valid for the duration of the call.
    let value = unsafe { &*value };
    value.validate()?;
    Ok(value.text()?.to_owned())
}

pub(crate) unsafe fn optional_buffer_utf8<T: TypedBuffer>(
    value: *const T,
) -> Result<Option<String>> {
    if value.is_null() {
        return Ok(None);
    }
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    Ok(Some(unsafe { buffer_utf8(value)? }))
}

pub(crate) unsafe fn copy_into_buffer<T: TypedBuffer>(text: &str, out: *mut T) -> Result<()> {
    if out.is_null() {
        return Err(Error::invalid(format!(
            "{} output must not be NULL",
            T::NAME
        )));
    }
    // SAFETY: the caller guarantees `out` points to writable typed storage.
    let out = unsafe { &mut *out };
    crate::util::copy_to_buffer(text, out.bytes_mut().as_mut_ptr(), T::CAPACITY)
}

pub(crate) unsafe fn buffer_str<T: TypedBuffer>(value: *const T) -> *const c_char {
    if value.is_null() {
        return ptr::null();
    }
    // SAFETY: the caller guarantees `value` points to a typed buffer that
    // remains valid for at least the lifetime of the returned pointer.
    let value = unsafe { &*value };
    if value.text().is_err() {
        return ptr::null();
    }
    value.bytes().as_ptr()
}

pub(crate) unsafe fn buffer_view<T: TypedBuffer>(value: *const T) -> otel_str_t {
    if value.is_null() {
        return otel_str_t {
            ptr: ptr::null(),
            len: 0,
        };
    }
    // SAFETY: the caller guarantees `value` points to a typed buffer that
    // remains valid for at least the lifetime of the returned view.
    let value = unsafe { &*value };
    match value.text() {
        Ok(text) => otel_str_t {
            ptr: value.bytes().as_ptr(),
            len: text.len(),
        },
        Err(_) => otel_str_t {
            ptr: ptr::null(),
            len: 0,
        },
    }
}

/// Copies a NUL-terminated string into typed `traceparent` storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_from_str(
    src: *const c_char,
    out: *mut otel_traceparent_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { fill(src, out) }
}

/// Copies a NUL-terminated string into typed `tracestate` storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracestate_from_str(
    src: *const c_char,
    out: *mut otel_tracestate_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { fill(src, out) }
}

/// Copies a NUL-terminated string into typed `baggage` header storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_from_str(
    src: *const c_char,
    out: *mut otel_baggage_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { fill(src, out) }
}

/// Copies a NUL-terminated string into typed baggage-value storage.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_value_from_str(
    src: *const c_char,
    out: *mut otel_baggage_value_t,
) -> otel_result_t {
    // SAFETY: The caller guarantees the preconditions of this unsafe operation.
    unsafe { fill(src, out) }
}

/// Returns the stored `traceparent` string, or NULL if it is not terminated.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_str(value: *const otel_traceparent_t) -> *const c_char {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_str(value) }
}

/// Returns the stored `tracestate` string, or NULL if it is not terminated.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracestate_str(value: *const otel_tracestate_t) -> *const c_char {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_str(value) }
}

/// Returns the stored `baggage` header, or NULL if it is not terminated.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_str(value: *const otel_baggage_t) -> *const c_char {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_str(value) }
}

/// Returns the stored baggage value, or NULL if it is not terminated.
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_value_str(
    value: *const otel_baggage_value_t,
) -> *const c_char {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_str(value) }
}

/// Returns a length-aware view of a `traceparent` header.
///
/// # Safety
/// The pointer must reference an initialized value; the returned view is valid only while that value remains alive and unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_traceparent_view(value: *const otel_traceparent_t) -> otel_str_t {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_view(value) }
}

/// Returns a length-aware view of a `tracestate` header.
///
/// # Safety
/// The pointer must reference an initialized value; the returned view is valid only while that value remains alive and unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_tracestate_view(value: *const otel_tracestate_t) -> otel_str_t {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_view(value) }
}

/// Returns a length-aware view of a `baggage` header.
///
/// # Safety
/// The pointer must reference an initialized value; the returned view is valid only while that value remains alive and unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_view(value: *const otel_baggage_t) -> otel_str_t {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_view(value) }
}

/// Returns a length-aware view of a baggage value.
///
/// # Safety
/// The pointer must reference an initialized value; the returned view is valid only while that value remains alive and unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_baggage_value_view(value: *const otel_baggage_value_t) -> otel_str_t {
    // SAFETY: The caller guarantees the destination is writable and the source stays alive for this call.
    unsafe { buffer_view(value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn traceparent_validation_accepts_w3c_format() {
        assert!(
            otel_traceparent_t::validate_text(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            )
            .is_ok()
        );
    }

    #[test]
    fn traceparent_validation_rejects_bad_ids() {
        assert!(
            otel_traceparent_t::validate_text(
                "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01"
            )
            .is_err()
        );
    }

    #[test]
    fn traceparent_validation_requires_exact_layout() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(otel_traceparent_t::validate_text(valid).is_ok());
        assert!(otel_traceparent_t::validate_text(&valid[1..]).is_err());
        assert!(otel_traceparent_t::validate_text(&valid.replace('0', "g")).is_err());
        assert!(otel_traceparent_t::validate_text(&valid.replace('-', "x")).is_err());
        assert!(otel_traceparent_t::validate_text(&format!("01{}", &valid[2..])).is_err());
    }

    #[test]
    fn fill_traceparent_and_read_it_back() {
        let source = c"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut out = otel_traceparent_t { value: [0; 56] };
        assert_eq!(
            // SAFETY: `source` is NUL-terminated and `out` is writable typed storage.
            unsafe { otel_traceparent_from_str(source.as_ptr(), &mut out) },
            otel_result_t::OTEL_OK
        );
        assert_eq!(
            // SAFETY: `out` is initialized and stays alive for these calls.
            unsafe { CStr::from_ptr(otel_traceparent_str(&out)) }.to_bytes(),
            source.to_bytes()
        );
        // SAFETY: `out` is initialized and stays alive for this call.
        let view = unsafe { otel_traceparent_view(&out) };
        assert_eq!(view.len, source.to_bytes().len());
        assert_eq!(view.ptr, out.value.as_ptr());
        assert_eq!(
            // SAFETY: `out` is initialized and stays alive for this call.
            unsafe { buffer_utf8(&out) }.unwrap(),
            source.to_str().unwrap()
        );
        assert!(out.validate().is_ok());
    }

    #[test]
    fn fill_rejects_invalid_and_oversized_values() {
        let mut parent = otel_traceparent_t { value: [0; 56] };
        assert_eq!(
            // SAFETY: the source is NUL-terminated and the output is writable.
            unsafe { otel_traceparent_from_str(c"bad".as_ptr(), &mut parent) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        let mut state = otel_tracestate_t { value: [0; 513] };
        let long = std::ffi::CString::new("a".repeat(513)).unwrap();
        assert_eq!(
            // SAFETY: the source is NUL-terminated and the output is writable.
            unsafe { otel_tracestate_from_str(long.as_ptr(), &mut state) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        let mut baggage = otel_baggage_t { value: [0; 1025] };
        let long = std::ffi::CString::new("b".repeat(1025)).unwrap();
        assert_eq!(
            // SAFETY: the source is NUL-terminated and the output is writable.
            unsafe { otel_baggage_from_str(long.as_ptr(), &mut baggage) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        let mut value = otel_baggage_value_t { value: [0; 257] };
        let long = std::ffi::CString::new("c".repeat(257)).unwrap();
        assert_eq!(
            // SAFETY: the source is NUL-terminated and the output is writable.
            unsafe { otel_baggage_value_from_str(long.as_ptr(), &mut value) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: NULL is an intentionally invalid output pointer.
            unsafe { otel_tracestate_from_str(c"x".as_ptr(), std::ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: NULL is an intentionally invalid input pointer.
            unsafe { otel_baggage_from_str(std::ptr::null(), &mut baggage) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn typed_buffers_copy_and_expose_all_four_kinds() {
        let mut parent = otel_traceparent_t { value: [0; 56] };
        let mut state = otel_tracestate_t { value: [0; 513] };
        let mut baggage = otel_baggage_t { value: [0; 1025] };
        let mut value = otel_baggage_value_t { value: [0; 257] };
        // SAFETY: sources are NUL-terminated and outputs are writable typed storage.
        unsafe {
            assert_eq!(
                otel_traceparent_from_str(
                    c"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".as_ptr(),
                    &mut parent
                ),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracestate_from_str(c"vendor=value".as_ptr(), &mut state),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_baggage_from_str(c"key=value".as_ptr(), &mut baggage),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_baggage_value_from_str(c"value".as_ptr(), &mut value),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                CStr::from_ptr(otel_tracestate_str(&state)).to_bytes(),
                b"vendor=value"
            );
            assert_eq!(
                CStr::from_ptr(otel_baggage_str(&baggage)).to_bytes(),
                b"key=value"
            );
            assert_eq!(
                CStr::from_ptr(otel_baggage_value_str(&value)).to_bytes(),
                b"value"
            );
            assert_eq!(otel_tracestate_view(&state).len, 12);
            assert_eq!(otel_baggage_view(&baggage).len, 9);
            assert_eq!(otel_baggage_value_view(&value).len, 5);
            assert_eq!(buffer_utf8(&state).unwrap(), "vendor=value");
            assert_eq!(buffer_utf8(&baggage).unwrap(), "key=value");
            assert_eq!(buffer_utf8(&value).unwrap(), "value");
            assert_eq!(
                optional_buffer_utf8(&value).unwrap(),
                Some("value".to_owned())
            );
            assert_eq!(
                optional_buffer_utf8::<otel_baggage_value_t>(std::ptr::null()).unwrap(),
                None
            );
        }
    }

    #[test]
    fn unterminated_and_non_utf8_buffers_are_rejected() {
        let unterminated = otel_baggage_value_t {
            value: [b'x' as c_char; 257],
        };
        // SAFETY: the buffer stays alive and is read-only for these calls.
        unsafe {
            assert_eq!(
                buffer_utf8(&unterminated).unwrap_err().status,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert!(otel_baggage_value_str(&unterminated).is_null());
            assert_eq!(otel_baggage_value_view(&unterminated).len, 0);
            assert!(otel_baggage_value_view(&unterminated).ptr.is_null());
            assert!(otel_baggage_value_str(std::ptr::null()).is_null());
            assert_eq!(otel_baggage_value_view(std::ptr::null()).len, 0);
        }

        let mut invalid = otel_baggage_value_t { value: [0; 257] };
        invalid.value[0] = -1;
        // SAFETY: the buffer stays alive and is read-only for these calls.
        unsafe {
            assert_eq!(
                buffer_utf8(&invalid).unwrap_err().status,
                otel_result_t::OTEL_ERR_INVALID_UTF8
            );
            assert!(otel_baggage_value_str(&invalid).is_null());
        }
    }

    #[test]
    fn embedded_nul_truncates_the_stored_value() {
        let source = b"first\0second\0"
            .iter()
            .map(|byte| *byte as c_char)
            .collect::<Vec<_>>();
        let mut out = otel_baggage_value_t { value: [0; 257] };
        // SAFETY: `source` is NUL-terminated and `out` is writable typed storage.
        unsafe {
            assert_eq!(
                otel_baggage_value_from_str(source.as_ptr(), &mut out),
                otel_result_t::OTEL_OK
            );
            assert_eq!(buffer_utf8(&out).unwrap(), "first");
            assert_eq!(otel_baggage_value_view(&out).len, 5);
        }
    }

    #[test]
    fn copy_into_buffer_checks_capacity() {
        let mut out = otel_tracestate_t { value: [0; 513] };
        // SAFETY: `out` is writable typed storage and the text is short.
        unsafe {
            assert!(copy_into_buffer("a=b", &mut out).is_ok());
            assert_eq!(buffer_utf8(&out).unwrap(), "a=b");
            assert_eq!(
                copy_into_buffer(&"x".repeat(513), &mut out)
                    .unwrap_err()
                    .status,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
            assert_eq!(
                copy_into_buffer::<otel_tracestate_t>("x", std::ptr::null_mut())
                    .unwrap_err()
                    .status,
                otel_result_t::OTEL_ERR_INVALID_ARGUMENT
            );
        }
    }

    #[test]
    fn buffer_utf8_rejects_null_typed_buffer() {
        assert_eq!(
            // SAFETY: NULL is intentionally invalid input and is checked before dereferencing.
            unsafe { buffer_utf8::<otel_baggage_value_t>(ptr::null()) }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn public_wrappers_cover_every_typed_buffer_kind() {
        const TRACEPARENT: &CStr = c"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut parent = otel_traceparent_t { value: [0; 56] };
        let mut state = otel_tracestate_t { value: [0; 513] };
        let mut baggage = otel_baggage_t { value: [0; 1025] };
        let mut value = otel_baggage_value_t { value: [0; 257] };

        // SAFETY: every source is NUL-terminated and every output stays alive for these calls.
        unsafe {
            assert_eq!(
                otel_traceparent_from_str(TRACEPARENT.as_ptr(), &mut parent),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_tracestate_from_str(c"vendor=value".as_ptr(), &mut state),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_baggage_from_str(c"key=value".as_ptr(), &mut baggage),
                otel_result_t::OTEL_OK
            );
            assert_eq!(
                otel_baggage_value_from_str(c"member".as_ptr(), &mut value),
                otel_result_t::OTEL_OK
            );

            assert_eq!(
                CStr::from_ptr(otel_traceparent_str(&parent)).to_bytes(),
                TRACEPARENT.to_bytes()
            );
            assert_eq!(
                CStr::from_ptr(otel_tracestate_str(&state)).to_bytes(),
                b"vendor=value"
            );
            assert_eq!(
                CStr::from_ptr(otel_baggage_str(&baggage)).to_bytes(),
                b"key=value"
            );
            assert_eq!(
                CStr::from_ptr(otel_baggage_value_str(&value)).to_bytes(),
                b"member"
            );

            assert_eq!(
                otel_traceparent_view(&parent).len,
                TRACEPARENT.to_bytes().len()
            );
            assert_eq!(otel_tracestate_view(&state).len, 12);
            assert_eq!(otel_baggage_view(&baggage).len, 9);
            let view = otel_baggage_value_view(&value);
            assert_eq!(view.len, 6);
            assert_eq!(view.ptr, value.value.as_ptr());
        }
    }
}
