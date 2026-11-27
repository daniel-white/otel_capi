//! Shared allocation tracking exported for loadable transports.
//!
//! Transports resolve these symbols from the core library so one
//! `otel_ensure_all_released()` check sees handles from the core and from every
//! loadable transport in the process.

/// Records one caller-owned handle allocation.
#[unsafe(no_mangle)]
pub extern "C" fn otel_handle_created() {
    crate::abi::handle_created();
}

/// Records one caller-owned handle release.
#[unsafe(no_mangle)]
pub extern "C" fn otel_handle_destroyed() {
    crate::abi::handle_destroyed();
}
