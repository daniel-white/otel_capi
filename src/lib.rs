//! C ABI bindings for the OpenTelemetry Rust SDK.
//!
//! All handles are caller-allocated structs holding an opaque pointer to an
//! `Arc`. Use the matching `otel_*_retain` and `otel_*_release` functions to
//! manage ownership. Strings are borrowed only for the duration of a call.

#![allow(non_camel_case_types, non_upper_case_globals)]

pub mod attributes;
pub mod error;
pub mod handles;

pub use error::otel_result_t;
