//! Resource construction from standard OpenTelemetry environment variables.

use crate::attributes::{from_array, from_varargs, otel_attribute_t};
use crate::config::{resource_from_env, resource_from_env_with};
use crate::error::{guard, otel_result_t};
use crate::handles::otel_resource_t;
use crate::handles::{init, slot};

/// Creates a resource from `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`.
/// Writes into caller-owned `handle`; initialize it with `{0}` before calling.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new(handle: *mut otel_resource_t) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller provides writable, aligned storage for one handle pointer.
        unsafe { init(slot(handle), resource_from_env()) }?;
        Ok(())
    })
}

unsafe fn resource_from_attributes(
    handle: *mut otel_resource_t,
    attributes: Vec<opentelemetry::KeyValue>,
) -> crate::error::Result<()> {
    // SAFETY: The caller provides writable, aligned storage for one handle pointer.
    unsafe { init(slot(handle), resource_from_env_with(attributes)) }?;
    Ok(())
}

/// Creates a resource with caller-supplied attributes in addition to the
/// standard environment configuration.
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new_with_attributes(
    handle: *mut otel_resource_t,
    attributes: ...
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_varargs(attributes) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { resource_from_attributes(handle, attributes) }
    })
}

/// Array-based variant of [`otel_resource_new_with_attributes`].
///
/// # Safety
/// The output pointer must reference writable, aligned storage for this handle type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otel_resource_new_with_attributes_arr(
    handle: *mut otel_resource_t,
    attributes: *const *const otel_attribute_t,
    count: usize,
) -> otel_result_t {
    guard(|| {
        // SAFETY: The caller guarantees every pointer is initialized and borrowed strings outlive the call.
        let attributes = unsafe { from_array(attributes, count) }?;
        // SAFETY: The caller guarantees the preconditions of this unsafe operation.
        unsafe { resource_from_attributes(handle, attributes) }
    })
}
