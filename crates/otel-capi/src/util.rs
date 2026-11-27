//! Shared helpers for FFI time values and output buffers.

use std::ffi::{CStr, c_char};
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Converts a Unix timestamp in nanoseconds into `SystemTime`. Zero means
/// "use the current time" and yields `None`.
pub(crate) fn system_time(nanos: i64) -> Result<Option<SystemTime>> {
    if nanos == 0 {
        return Ok(None);
    }
    let nanos = u64::try_from(nanos)
        .map_err(|_| Error::invalid("timestamp must be zero or a non-negative Unix nanosecond value"))?;
    UNIX_EPOCH
        .checked_add(Duration::from_nanos(nanos))
        .ok_or_else(|| Error::invalid("timestamp is out of range"))
        .map(Some)
}

/// Copies a string into a caller-provided buffer, including its terminator.
pub(crate) fn copy_to_buffer(text: &str, buf: *mut c_char, capacity: usize) -> Result<()> {
    if buf.is_null() || capacity == 0 {
        return Err(Error::invalid("output buffer must not be NULL or empty"));
    }
    let bytes = text.as_bytes();
    if bytes.len() + 1 > capacity {
        return Err(Error::invalid("output buffer is too small"));
    }
    // SAFETY: caller guarantees `buf` points to at least `capacity` writable bytes.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, bytes.len());
        *buf.add(bytes.len()) = 0;
    }
    Ok(())
}

/// Borrows a NUL-terminated C string, returning `None` for NULL.
pub(crate) unsafe fn optional_str<'a>(value: *const c_char, name: &str) -> Result<Option<&'a str>> {
    if value.is_null() {
        return Ok(None);
    }
    // SAFETY: caller guarantees `value` points to a NUL-terminated string that
    // stays valid for the duration of the call.
    let raw = unsafe { CStr::from_ptr(value) };
    let text = std::str::from_utf8(raw.to_bytes())
        .map_err(|_| Error::new(crate::error::otel_result_t::OTEL_ERR_INVALID_UTF8, format!("{name} is not valid UTF-8")))?;
    Ok(Some(text))
}
