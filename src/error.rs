use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

/// Status returned by every fallible `otel-capi` function.
#[non_exhaustive]
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum otel_result_t {
    OTEL_OK = 0,
    OTEL_ERR_INVALID_ARGUMENT = 1,
    OTEL_ERR_INVALID_UTF8 = 2,
    OTEL_ERR_CONFIG = 3,
    OTEL_ERR_EXPORT = 4,
    OTEL_ERR_SHUTDOWN = 5,
    OTEL_ERR_INTERNAL = 6,
    OTEL_ERR_PANIC = 7,
    /// Handles are still live at an explicit cleanup check.
    OTEL_ERR_LEAK = 8,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Internal error with a status and a human-readable message for C callers.
#[derive(Debug)]
pub(crate) struct Error {
    pub(crate) status: otel_result_t,
    pub(crate) message: String,
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn new(status: otel_result_t, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(otel_result_t::OTEL_ERR_INVALID_ARGUMENT, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(otel_result_t::OTEL_ERR_INTERNAL, message)
    }

    pub(crate) fn report(self) -> otel_result_t {
        set_error(self.status, &self.message)
    }
}

impl From<std::env::VarError> for Error {
    fn from(err: std::env::VarError) -> Self {
        Self::new(otel_result_t::OTEL_ERR_CONFIG, err.to_string())
    }
}

pub(crate) fn ok() -> otel_result_t {
    otel_result_t::OTEL_OK
}

pub(crate) fn set_error(status: otel_result_t, message: &str) -> otel_result_t {
    let safe = message.replace('\0', " ");
    let c_message = CString::new(safe).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    LAST_ERROR.with(|last| *last.borrow_mut() = Some(c_message));
    status
}

/// Keeps Rust panics from unwinding across the C ABI boundary and reports
/// internal errors to the calling thread.
pub(crate) fn guard<F>(f: F) -> otel_result_t
where
    F: FnOnce() -> Result<()>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => ok(),
        Ok(Err(err)) => err.report(),
        Err(_) => set_error(otel_result_t::OTEL_ERR_PANIC, "panic while calling otel-capi"),
    }
}

/// Borrow a NUL-terminated C string for the duration of a call.
pub(crate) unsafe fn c_str<'a>(ptr: *const c_char, name: &str) -> Result<&'a CStr> {
    if ptr.is_null() {
        return Err(Error::invalid(format!("{name} must not be NULL")));
    }
    // SAFETY: caller guarantees `ptr` points to a NUL-terminated string that
    // remains valid for the duration of the call.
    Ok(unsafe { CStr::from_ptr(ptr) })
}

pub(crate) fn utf8(value: &CStr, name: &str) -> Result<String> {
    value.to_str().map(str::to_owned).map_err(|_| {
        Error::new(
            otel_result_t::OTEL_ERR_INVALID_UTF8,
            format!("{name} is not valid UTF-8"),
        )
    })
}

/// Borrows an optional C string; NULL yields `None`.
pub(crate) unsafe fn optional_utf8(ptr: *const c_char, name: &str) -> Result<Option<String>> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: caller guarantees `ptr` is NULL or a NUL-terminated string.
    let raw = unsafe { c_str(ptr, name)? };
    Ok(Some(utf8(raw, name)?))
}

/// Returns the last error message for the calling thread. The pointer remains
/// valid until another `otel-capi` call on the same thread records an error.
#[unsafe(no_mangle)]
pub extern "C" fn otel_last_error() -> *const c_char {
    LAST_ERROR.with(|last| match last.borrow().as_ref() {
        Some(message) => message.as_ptr(),
        None => c"".as_ptr(),
    })
}

/// Returns a stable description for a result code. Unknown codes produce
/// `"unknown status"`.
#[unsafe(no_mangle)]
pub extern "C" fn otel_strerror(code: i32) -> *const c_char {
    match code {
        0 => c"ok",
        1 => c"invalid argument",
        2 => c"invalid UTF-8",
        3 => c"configuration error",
        4 => c"export failed",
        5 => c"shutdown failed",
        6 => c"internal error",
        7 => c"panic",
        8 => c"handles still live",
        _ => c"unknown status",
    }
    .as_ptr()
}

/// Copies the last error message, including its NUL terminator, into a
/// caller-owned buffer. Returns the required size (including NUL) if `buf` is
/// NULL or `capacity` is too small; otherwise returns the bytes written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_copy_last_error(buf: *mut c_char, capacity: usize) -> usize {
    let message = LAST_ERROR.with(|last| {
        last.borrow()
            .as_ref()
            .map(|message| message.as_bytes_with_nul().to_vec())
            .unwrap_or_else(|| vec![0])
    });

    if buf.is_null() || capacity < message.len() {
        return message.len();
    }

    // SAFETY: caller guarantees `buf` can hold at least `message.len()` bytes.
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), buf, message.len());
    }
    message.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_error_is_thread_local_and_copied() {
        set_error(otel_result_t::OTEL_ERR_CONFIG, "bad config");
        let mut buf = [0i8; 32];
        let written = unsafe { otel_copy_last_error(buf.as_mut_ptr(), buf.len()) };
        assert_eq!(written, 11);
        assert_eq!(
            unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(),
            "bad config"
        );
    }
}
