//! C API for OpenTelemetry.
//!
//! All handles are caller-owned structs holding an opaque pointer to an
//! `Arc`. Use the matching `otel_*_retain` and `otel_*_release` functions to
//! manage ownership. Strings are borrowed only for the duration of a call.
//!
//! Loadable transports depend on this crate with `default-features = false`
//! and `features = ["abi"]`. That exposes the shared C ABI without linking the
//! SDK or duplicating any `otel_*` exported symbols.

#![allow(non_camel_case_types, non_upper_case_globals)]

pub mod abi;

#[cfg(feature = "full")]
pub mod error;
#[cfg(feature = "full")]
pub mod strings;

#[cfg(feature = "full")]
pub mod attributes;
#[cfg(feature = "full")]
mod config;
#[cfg(feature = "full")]
pub mod context;
#[cfg(feature = "full")]
pub mod handles;
#[cfg(feature = "full")]
mod item_export;
#[cfg(feature = "full")]
pub mod items;
#[cfg(feature = "full")]
pub mod logs;
#[cfg(feature = "full")]
pub mod metrics;
#[cfg(feature = "full")]
pub mod propagation;
#[cfg(feature = "full")]
pub mod resource;
#[cfg(feature = "full")]
pub mod runtime;
#[cfg(feature = "full")]
pub mod trace;
#[cfg(feature = "full")]
pub mod tracking;
#[cfg(feature = "full")]
pub mod transport;
#[cfg(feature = "full")]
mod util;

#[cfg(feature = "full")]
use std::ffi::{CString, c_char};
#[cfg(feature = "full")]
use std::sync::OnceLock;

pub use abi::otel_result_t;

#[cfg(feature = "full")]
static VERSION: OnceLock<CString> = OnceLock::new();

/// Returns the crate version as a NUL-terminated string.
#[cfg(feature = "full")]
#[unsafe(no_mangle)]
pub extern "C" fn otel_version() -> *const c_char {
    VERSION
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).expect("version has no NUL"))
        .as_ptr()
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn otel_version_is_stable_and_nul_terminated() {
        let first = otel_version();
        assert_eq!(first, otel_version());
        assert!(!first.is_null());
        // SAFETY: `otel_version` returns a pointer to a static NUL-terminated C string.
        let version = unsafe { CStr::from_ptr(first) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }
}
