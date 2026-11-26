//! Resource construction from standard OpenTelemetry environment variables.

use crate::config::resource_from_env;
use crate::error::{guard, otel_result_t};
use crate::handles::{init, slot};
use crate::handles::otel_resource_t;

/// Creates a resource from `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new(handle: *mut otel_resource_t) -> otel_result_t {
    guard(|| {
        unsafe { init(slot(handle), resource_from_env()) }?;
        Ok(())
    })
}
