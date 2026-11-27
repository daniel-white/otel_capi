//! Resource construction from standard OpenTelemetry environment variables.

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config::{resource_from_env, resource_from_env_with};
use crate::error::{guard, otel_result_t};
use crate::handles::otel_resource_t;
use crate::handles::{init, slot};

/// Creates a resource from `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new(handle: *mut otel_resource_t) -> otel_result_t {
    guard(|| {
        unsafe { init(slot(handle), resource_from_env()) }?;
        Ok(())
    })
}

unsafe fn resource_from_attributes(
    handle: *mut otel_resource_t,
    attributes: Vec<opentelemetry::KeyValue>,
) -> crate::error::Result<()> {
    unsafe { init(slot(handle), resource_from_env_with(attributes)) }?;
    Ok(())
}

/// Creates a resource with caller-supplied attributes in addition to the
/// standard environment configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new_with_attributes(
    handle: *mut otel_resource_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_varargs(attributes) }?;
        unsafe { resource_from_attributes(handle, attributes) }
    })
}

/// Array-based variant of [`otel_resource_new_with_attributes`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new_with_attributes_arr(
    handle: *mut otel_resource_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        let attributes = unsafe { from_array(attributes, count) }?;
        unsafe { resource_from_attributes(handle, attributes) }
    })
}
