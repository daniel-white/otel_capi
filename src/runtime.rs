//! Explicit async runtime handle created by the C caller.
//!
//! The async engine is an implementation detail; C code creates a runtime once
//! with `otel_runtime_new` and passes it to provider constructors that need it.
//! Providers retain their own reference, so the C handle may be released at any
//! time.

use tokio::runtime::Builder;

use crate::error::{Error, guard, otel_result_t};
use crate::handles::{RuntimeInner, const_slot, init, otel_runtime_t, slot, valid};

/// Creates a multi-threaded async runtime used by OTLP exporters.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_new(handle: *mut otel_runtime_t) -> otel_result_t {
    guard(|| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cotel-worker")
            .enable_all()
            .build()
            .map_err(|err| {
                Error::new(
                    otel_result_t::OTEL_ERR_INTERNAL,
                    format!("failed to create async runtime: {err}"),
                )
            })?;
        unsafe { init(slot(handle), runtime) }?;
        Ok(())
    })
}

/// Returns whether the runtime handle is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_runtime_is_valid(runtime: *const otel_runtime_t) -> bool {
    unsafe { valid::<RuntimeInner>(const_slot(runtime)) }
}
