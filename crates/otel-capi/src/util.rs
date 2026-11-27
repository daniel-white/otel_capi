//! Shared helpers for FFI time values and output buffers.

use std::collections::HashSet;
use std::ffi::{CStr, c_char};
use std::ptr;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

/// Interns a string for OpenTelemetry APIs that require `&'static str`.
///
/// The leaked allocation stays reachable from this static registry for the
/// process lifetime, and repeated values reuse the same allocation.
pub(crate) fn interned(text: String) -> &'static str {
    let mut set = INTERNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = set.get(text.as_str()).copied() {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.into_boxed_str());
    set.insert(leaked);
    leaked
}

/// Converts a Unix timestamp in nanoseconds into `SystemTime`. Zero means
/// "use the current time" and yields `None`.
pub(crate) fn system_time(nanos: i64) -> Result<Option<SystemTime>> {
    if nanos == 0 {
        return Ok(None);
    }
    let nanos = u64::try_from(nanos).map_err(|_| {
        Error::invalid("timestamp must be zero or a non-negative Unix nanosecond value")
    })?;
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
    let text = std::str::from_utf8(raw.to_bytes()).map_err(|_| {
        Error::new(
            crate::error::otel_result_t::OTEL_ERR_INVALID_UTF8,
            format!("{name} is not valid UTF-8"),
        )
    })?;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::otel_result_t;

    #[test]
    fn system_time_interprets_zero_and_positive_nanoseconds() {
        assert_eq!(system_time(0).unwrap(), None);
        let time = system_time(1_000_000_000).unwrap().unwrap();
        assert_eq!(
            time.duration_since(UNIX_EPOCH).unwrap(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn system_time_rejects_negative_values() {
        let error = system_time(-1).unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_ARGUMENT);
        assert!(system_time(i64::MIN).is_err());
    }

    #[test]
    fn system_time_accepts_the_largest_positive_timestamp() {
        let time = system_time(i64::MAX).unwrap().unwrap();
        assert!(time > UNIX_EPOCH + Duration::from_secs(i64::MAX as u64 / 1_000_000_000));
    }

    #[test]
    fn copy_to_buffer_writes_terminator_and_checks_capacity() {
        let mut buf = [0i8; 4];
        assert!(copy_to_buffer("abc", buf.as_mut_ptr(), buf.len()).is_ok());
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(),
            "abc"
        );
        assert_eq!(buf[3], 0);

        let mut small = [b'x' as c_char; 3];
        assert_eq!(
            copy_to_buffer("abc", small.as_mut_ptr(), small.len())
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(small, [b'x' as c_char; 3]);
        assert_eq!(
            copy_to_buffer("a", ptr::null_mut(), 4).unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            copy_to_buffer("a", buf.as_mut_ptr(), 0).unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn optional_str_handles_null_valid_and_invalid_utf8() {
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { optional_str(ptr::null(), "value") }.unwrap(),
            None
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { optional_str(c"hello".as_ptr(), "value") }.unwrap(),
            Some("hello")
        );

        let invalid = [0xffu8 as c_char, 0];
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let error = unsafe { optional_str(invalid.as_ptr(), "value") }.unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_INVALID_UTF8);
    }

    #[test]
    fn interned_reuses_equal_strings() {
        let first = interned(String::from("shared"));
        let second = interned(String::from("shared"));
        let other = interned(String::from("different"));
        assert_eq!(first, "shared");
        assert_eq!(first.as_ptr(), second.as_ptr());
        assert_ne!(first.as_ptr(), other.as_ptr());
    }
}
