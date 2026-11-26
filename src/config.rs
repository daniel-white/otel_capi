//! Standard OpenTelemetry environment configuration and exporter selection.

use std::env;
use std::fmt::Display;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use crate::error::{Error, Result, otel_result_t};

/// Exporter selected by `OTEL_<SIGNAL>_EXPORTER`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExporterKind {
    /// OTLP over the protocol/endpoint described by `OTEL_EXPORTER_OTLP_*`.
    Otlp,
    /// Human-readable exporter writing to stdout.
    Stdout,
    /// No exporter; telemetry is created but not exported.
    None,
}

/// Builds a resource from `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`,
/// and the SDK's built-in detectors.
pub(crate) fn resource_from_env() -> Resource {
    resource_from_env_with(Vec::new())
}

/// Builds a resource from the environment plus caller-supplied attributes.
pub(crate) fn resource_from_env_with(attributes: Vec<KeyValue>) -> Resource {
    Resource::builder().with_attributes(attributes).build()
}

/// Reads `OTEL_<SIGNAL>_EXPORTER`. Unset and empty mean `otlp`; `console` is
/// accepted as an alias for `stdout`.
pub(crate) fn exporter_kind(signal: &str) -> Result<ExporterKind> {
    let name = format!("OTEL_{signal}_EXPORTER");
    let value = env::var(&name).unwrap_or_default();
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "otlp" => Ok(ExporterKind::Otlp),
        "stdout" | "console" => Ok(ExporterKind::Stdout),
        "none" => Ok(ExporterKind::None),
        other => Err(Error::new(
            otel_result_t::OTEL_ERR_CONFIG,
            format!("{name}={other} is not supported (use otlp, stdout, or none)"),
        )),
    }
}

/// Reads a duration in milliseconds from an OTel environment variable.
pub(crate) fn duration_ms(name: &str, default: u64) -> Result<Duration> {
    let value = env::var(name).unwrap_or_default();
    if value.trim().is_empty() {
        return Ok(Duration::from_millis(default));
    }
    let millis: u64 = value.trim().parse().map_err(|_| {
        Error::new(
            otel_result_t::OTEL_ERR_CONFIG,
            format!("{name} must be a non-negative integer number of milliseconds"),
        )
    })?;
    Ok(Duration::from_millis(millis))
}

/// Returns true when `OTEL_SDK_DISABLED` is set to a truthy value.
pub(crate) fn sdk_disabled() -> bool {
    matches!(
        env::var("OTEL_SDK_DISABLED").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn sdk_error(context: &str, err: OTelSdkError) -> Error {
    Error::new(
        otel_result_t::OTEL_ERR_EXPORT,
        format!("{context}: {err}"),
    )
}

pub(crate) fn build_error<E: Display>(context: &str, err: E) -> Error {
    Error::new(
        otel_result_t::OTEL_ERR_CONFIG,
        format!("{context}: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exporter_kind_accepts_known_values() {
        // Tests set variables one at a time; use unique signals per case.
        unsafe {
            env::set_var("OTEL_TEST_EXPORTER", "stdout");
        }
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Stdout);
        unsafe {
            env::set_var("OTEL_TEST_EXPORTER", "CONSOLE");
        }
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Stdout);
        unsafe {
            env::set_var("OTEL_TEST_EXPORTER", "none");
        }
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::None);
        unsafe {
            env::set_var("OTEL_TEST_EXPORTER", "bogus");
        }
        assert_eq!(
            exporter_kind("TEST").unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        unsafe {
            env::remove_var("OTEL_TEST_EXPORTER");
        }
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Otlp);
    }

    #[test]
    fn duration_ms_parses_and_defaults() {
        unsafe {
            env::set_var("OTEL_TEST_DELAY", "250");
        }
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap(),
            Duration::from_millis(250)
        );
        unsafe {
            env::set_var("OTEL_TEST_DELAY", "nope");
        }
        assert!(duration_ms("OTEL_TEST_DELAY", 100).is_err());
        unsafe {
            env::remove_var("OTEL_TEST_DELAY");
        }
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap(),
            Duration::from_millis(100)
        );
    }
}
