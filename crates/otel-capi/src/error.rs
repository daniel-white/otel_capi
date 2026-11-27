use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

pub use crate::abi::otel_result_t;

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
        Err(_) => set_error(
            otel_result_t::OTEL_ERR_PANIC,
            "panic while calling otel-capi",
        ),
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
///
/// # Safety
/// Callers must pass non-null, aligned pointers to initialized storage and keep all borrowed buffers alive for the duration of the call. Output slots must be writable; `otel_str_t` payloads must reference the stated number of readable bytes.
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
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let written = unsafe { otel_copy_last_error(buf.as_mut_ptr(), buf.len()) };
        assert_eq!(written, 11);
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(),
            "bad config"
        );
    }

    #[test]
    fn strerror_describes_every_status() {
        for (code, expected) in [
            (otel_result_t::OTEL_OK, "ok"),
            (otel_result_t::OTEL_ERR_INVALID_ARGUMENT, "invalid argument"),
            (otel_result_t::OTEL_ERR_INVALID_UTF8, "invalid UTF-8"),
            (otel_result_t::OTEL_ERR_CONFIG, "configuration error"),
            (otel_result_t::OTEL_ERR_EXPORT, "export failed"),
            (otel_result_t::OTEL_ERR_SHUTDOWN, "shutdown failed"),
            (otel_result_t::OTEL_ERR_INTERNAL, "internal error"),
            (otel_result_t::OTEL_ERR_PANIC, "panic"),
            (otel_result_t::OTEL_ERR_LEAK, "handles still live"),
        ] {
            // SAFETY: `otel_strerror` returns a static NUL-terminated literal.
            let text = unsafe { CStr::from_ptr(otel_strerror(code as i32)) }
                .to_str()
                .unwrap();
            assert_eq!(text, expected);
        }
        // SAFETY: `otel_strerror` returns a static NUL-terminated literal.
        assert_eq!(
            // SAFETY: `otel_strerror` returns a static NUL-terminated literal.
            unsafe { CStr::from_ptr(otel_strerror(999)) }
                .to_str()
                .unwrap(),
            "unknown status"
        );
    }

    #[test]
    fn set_error_sanitizes_nuls_and_last_error_tracks_updates() {
        assert_eq!(
            set_error(otel_result_t::OTEL_ERR_INTERNAL, "a\0b"),
            otel_result_t::OTEL_ERR_INTERNAL
        );
        // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
        let message = unsafe { CStr::from_ptr(otel_last_error()) }
            .to_str()
            .unwrap();
        assert_eq!(message, "a b");
        set_error(otel_result_t::OTEL_OK, "second");
        assert_eq!(
            // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
            unsafe { CStr::from_ptr(otel_last_error()) }
                .to_str()
                .unwrap(),
            "second"
        );
    }

    #[test]
    fn unset_last_error_is_empty_and_copies_one_terminator() {
        LAST_ERROR.with(|last| *last.borrow_mut() = None);
        // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
        let empty = unsafe { CStr::from_ptr(otel_last_error()) }
            .to_str()
            .unwrap();
        assert_eq!(empty, "");
        assert_eq!(
            // SAFETY: a NULL output buffer is explicitly accepted by the function.
            unsafe { otel_copy_last_error(std::ptr::null_mut(), 0) },
            1
        );
        let mut buf = [1i8; 2];
        assert_eq!(
            // SAFETY: the local array has room for the terminator.
            unsafe { otel_copy_last_error(buf.as_mut_ptr(), buf.len()) },
            1
        );
        assert_eq!(buf, [0, 1]);
    }

    #[test]
    fn copy_last_error_reports_required_size() {
        set_error(otel_result_t::OTEL_ERR_CONFIG, "12345");
        // SAFETY: a NULL output buffer is explicitly accepted by the function.
        assert_eq!(unsafe { otel_copy_last_error(std::ptr::null_mut(), 0) }, 6);
        let mut short = [0i8; 3];
        assert_eq!(
            // SAFETY: the local array is writable, but deliberately too small.
            unsafe { otel_copy_last_error(short.as_mut_ptr(), short.len()) },
            6
        );
        assert_eq!(short, [0; 3]);
        let mut exact = [0i8; 6];
        assert_eq!(
            // SAFETY: the local array has room for the message and its terminator.
            unsafe { otel_copy_last_error(exact.as_mut_ptr(), exact.len()) },
            6
        );
        assert_eq!(
            exact.map(|byte| byte as u8).as_slice(),
            c"12345".to_bytes_with_nul()
        );
    }

    #[test]
    fn guard_reports_ok_errors_and_panics() {
        assert_eq!(guard(|| Ok(())), ok());
        assert_eq!(
            guard(|| Err(Error::invalid("nope"))),
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
            unsafe { CStr::from_ptr(otel_last_error()) }
                .to_str()
                .unwrap(),
            "nope"
        );
        assert_eq!(guard(|| panic!("boom")), otel_result_t::OTEL_ERR_PANIC);
        assert_eq!(
            // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
            unsafe { CStr::from_ptr(otel_last_error()) }
                .to_str()
                .unwrap(),
            "panic while calling otel-capi"
        );
    }

    #[test]
    fn error_constructors_report_their_status() {
        let error = Error::new(otel_result_t::OTEL_ERR_EXPORT, "export");
        assert_eq!(error.status, otel_result_t::OTEL_ERR_EXPORT);
        assert_eq!(error.report(), otel_result_t::OTEL_ERR_EXPORT);
        assert_eq!(
            Error::invalid("x").status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            Error::internal("x").report(),
            otel_result_t::OTEL_ERR_INTERNAL
        );
        assert_eq!(
            // SAFETY: `otel_last_error` returns a pointer to a live thread-local C string.
            unsafe { CStr::from_ptr(otel_last_error()) }
                .to_str()
                .unwrap(),
            "x"
        );
    }

    #[test]
    fn c_string_helpers_reject_null_and_bad_utf8() {
        assert_eq!(
            // SAFETY: a NULL pointer is intentionally passed to test validation.
            unsafe { c_str(std::ptr::null(), "value") }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        // SAFETY: the local C string is valid for the duration of the call.
        let value = unsafe { c_str(c"text".as_ptr(), "value") }.unwrap();
        assert_eq!(value.to_bytes(), b"text");
        assert_eq!(utf8(value, "value").unwrap(), "text");
        let invalid = [0xffu8, 0];
        // SAFETY: `invalid` is a NUL-terminated byte buffer that lives for the call.
        let error = unsafe { c_str(invalid.as_ptr().cast(), "value") }.unwrap();
        assert_eq!(
            utf8(error, "value").unwrap_err().status,
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
        // SAFETY: NULL is a valid optional C string.
        assert_eq!(
            // SAFETY: NULL is a valid optional C string.
            unsafe { optional_utf8(std::ptr::null(), "value") }.unwrap(),
            None
        );
        assert_eq!(
            // SAFETY: the local C string is valid for the duration of the call.
            unsafe { optional_utf8(c"hi".as_ptr(), "value") }.unwrap(),
            Some("hi".to_owned())
        );
        assert_eq!(
            // SAFETY: `invalid` is a NUL-terminated byte buffer that lives for the call.
            unsafe { optional_utf8(invalid.as_ptr().cast(), "value") }
                .unwrap_err()
                .status,
            otel_result_t::OTEL_ERR_INVALID_UTF8
        );
    }
}
