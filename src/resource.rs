//! Resource construction from standard OpenTelemetry environment variables.

use crate::attributes::from_varargs;
use crate::config::{resource_from_env, resource_from_env_with};
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

/// Creates a resource with caller-supplied attributes in addition to the
/// standard environment configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new_with_attributes(
    handle: *mut otel_resource_t,
    attributes: ...,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        unsafe { init(slot(handle), resource_from_env_with(attributes)) }?;
        Ok(())
    })
}
