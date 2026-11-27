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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::{otel_attribute_type_t, otel_attribute_value_t};
    use crate::config::tests::EnvGuard;
    use crate::handles::{ResourceInner, arc, const_slot, otel_resource_release};
    use opentelemetry::{Key, Value};
    use std::ffi::c_char;
    use std::ptr;

    #[test]
    fn resource_new_reads_environment() {
        let mut env = EnvGuard::new();
        env.set("OTEL_SERVICE_NAME", "integration-test-service");
        env.set("OTEL_RESOURCE_ATTRIBUTES", "custom.attr=from-env");
        let mut handle = otel_resource_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new(&mut handle) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let resource = unsafe { arc::<ResourceInner>(const_slot(&handle)) }.unwrap();
        assert_eq!(
            resource.get(&Key::new("service.name")),
            Some(Value::String("integration-test-service".to_owned().into()))
        );
        assert_eq!(
            resource.get(&Key::new("custom.attr")),
            Some(Value::String("from-env".to_owned().into()))
        );
        drop(resource);
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_resource_release(&mut handle) };
        assert!(handle.inner.is_null());
    }

    #[test]
    fn resource_new_with_attributes_arr_adds_custom_attribute() {
        let mut handle = otel_resource_t {
            inner: ptr::null_mut(),
        };
        let key = c"custom.key";
        let value = c"custom-value";
        let attribute = otel_attribute_t {
            kind: otel_attribute_type_t::OTEL_ATTRIBUTE_TYPE_STRING as i32,
            key: key.as_ptr(),
            key_len: key.to_bytes().len(),
            value: otel_attribute_value_t {
                string: value.as_ptr().cast::<c_char>(),
            },
            value_len: value.to_bytes().len(),
        };
        let attributes = [&attribute as *const otel_attribute_t];
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new_with_attributes_arr(&mut handle, attributes.as_ptr(), 1) },
            otel_result_t::OTEL_OK
        );
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        let resource = unsafe { arc::<ResourceInner>(const_slot(&handle)) }.unwrap();
        assert_eq!(
            resource.get(&Key::new("custom.key")),
            Some(Value::String("custom-value".to_owned().into()))
        );
        drop(resource);
        // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
        unsafe { otel_resource_release(&mut handle) };
    }

    #[test]
    fn resource_constructors_reject_invalid_inputs() {
        let mut handle = otel_resource_t {
            inner: ptr::null_mut(),
        };
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new(ptr::null_mut()) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new_with_attributes_arr(ptr::null_mut(), ptr::null(), 0) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new_with_attributes_arr(&mut handle, ptr::null(), 1) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());

        let nulls = [ptr::null()];
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new_with_attributes_arr(&mut handle, nulls.as_ptr(), 1) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());

        let key = c"key";
        let value = c"value";
        let bad = otel_attribute_t {
            kind: 99,
            key: key.as_ptr(),
            key_len: key.to_bytes().len(),
            value: otel_attribute_value_t {
                string: value.as_ptr().cast::<c_char>(),
            },
            value_len: value.to_bytes().len(),
        };
        let attributes = [&raw const bad];
        assert_eq!(
            // SAFETY: Test fixtures are local or intentionally invalid inputs that are validated before use.
            unsafe { otel_resource_new_with_attributes_arr(&mut handle, attributes.as_ptr(), 1) },
            otel_result_t::OTEL_ERR_INVALID_ARGUMENT
        );
        assert!(handle.inner.is_null());
    }
}
