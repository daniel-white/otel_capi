//! C ABI bindings for the OpenTelemetry Rust SDK.
//!
//! All handles are caller-allocated structs holding an opaque pointer to an
//! `Arc`. Use the matching `otel_*_retain` and `otel_*_release` functions to
//! manage ownership. Strings are borrowed only for the duration of a call.

#![allow(non_camel_case_types, non_upper_case_globals)]

pub mod attributes;
mod config;
pub mod context;
pub mod error;
pub mod handles;
pub mod logs;
pub mod metrics;
pub mod propagation;
pub mod resource;
pub mod runtime;
pub mod trace;
mod util;

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

pub use error::otel_result_t;

static VERSION: OnceLock<CString> = OnceLock::new();

/// Returns the crate version as a NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn otel_version() -> *const c_char {
    VERSION
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).expect("version has no NUL"))
        .as_ptr()
}
