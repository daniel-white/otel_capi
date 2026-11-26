//! Resource construction from standard OpenTelemetry environment variables.

use crate::config::resource_from_env;
use crate::error::{guard, otel_result_t};
use crate::handles::{init, slot};
use crate::handles::otel_resource_t;

/// Creates a resource from `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new(out: *mut otel_resource_t) -> otel_result_t {
    guard(|| {
        unsafe { init(slot(out), resource_from_env()) }?;
        Ok(())
    })
}
