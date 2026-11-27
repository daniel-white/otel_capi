//! Standard OpenTelemetry environment configuration and exporter selection.

use std::env;
use std::fmt::Display;
use std::time::Duration;

use crate::error::{Error, Result, otel_result_t};
use opentelemetry::KeyValue;
use opentelemetry_otlp::Protocol;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;

/// Registry name used by the core when a loadable transport implements OTLP HTTP.
pub(crate) const OTLP_HTTP_TRANSPORT: &str = "http";
/// Registry name used by the core when a loadable transport implements OTLP gRPC.
pub(crate) const OTLP_GRPC_TRANSPORT: &str = "grpc";
/// Registry name used by the core when a loadable transport implements stdout export.
pub(crate) const STDOUT_TRANSPORT: &str = "stdout";

/// Exporter selected by `OTEL_<SIGNAL>_EXPORTER`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExporterKind {
    /// OTLP over the protocol/endpoint described by `OTEL_EXPORTER_OTLP_*`.
    Otlp,
    /// In-memory item transport registered on the runtime.
    Item,
    /// Human-readable exporter writing to stdout.
    Stdout,
    /// No exporter; telemetry is created but not exported.
    None,
}

/// OTLP wire protocol selected by `OTEL_EXPORTER_OTLP_*_PROTOCOL`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OtlpProtocol {
    Grpc,
    HttpBinary,
    HttpJson,
}

impl From<OtlpProtocol> for Protocol {
    fn from(protocol: OtlpProtocol) -> Self {
        match protocol {
            // gRPC is emitted through the callback transport as protobuf bytes.
            OtlpProtocol::Grpc => Protocol::HttpBinary,
            OtlpProtocol::HttpBinary => Protocol::HttpBinary,
            OtlpProtocol::HttpJson => Protocol::HttpJson,
        }
    }
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
        "item" | "memory" => Ok(ExporterKind::Item),
        "stdout" | "console" => Ok(ExporterKind::Stdout),
        "none" => Ok(ExporterKind::None),
        other => Err(Error::new(
            otel_result_t::OTEL_ERR_CONFIG,
            format!("{name}={other} is not supported (use otlp, item, stdout, or none)"),
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

/// Reads the OTLP protocol, preferring the signal-specific variable.
///
/// This matches `opentelemetry-otlp`'s feature default (`http/json` here), but
/// reports invalid values instead of silently falling back.
pub(crate) fn otlp_protocol(signal: &str) -> Result<OtlpProtocol> {
    let signal_var = format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL");
    for name in [signal_var.as_str(), "OTEL_EXPORTER_OTLP_PROTOCOL"] {
        let value = env::var(name).unwrap_or_default();
        if value.trim().is_empty() {
            continue;
        }
        return parse_protocol(&value).ok_or_else(|| {
            Error::new(
                otel_result_t::OTEL_ERR_CONFIG,
                format!("{name}={value} is not supported (use grpc, http/protobuf, or http/json)"),
            )
        });
    }
    Ok(OtlpProtocol::HttpJson)
}

/// Reads the OTLP request timeout in milliseconds, preferring the
/// signal-specific variable. Defaults to 10 seconds.
pub(crate) fn otlp_timeout(signal: &str) -> Result<Duration> {
    let signal_var = format!("OTEL_EXPORTER_OTLP_{signal}_TIMEOUT");
    for name in [signal_var.as_str(), "OTEL_EXPORTER_OTLP_TIMEOUT"] {
        let value = env::var(name).unwrap_or_default();
        if value.trim().is_empty() {
            continue;
        }
        let millis: u64 = value.trim().parse().map_err(|_| {
            Error::new(
                otel_result_t::OTEL_ERR_CONFIG,
                format!("{name} must be a non-negative integer number of milliseconds"),
            )
        })?;
        return Ok(Duration::from_millis(millis));
    }
    Ok(Duration::from_secs(10))
}

fn parse_protocol(value: &str) -> Option<OtlpProtocol> {
    match value.trim().to_ascii_lowercase().as_str() {
        "grpc" => Some(OtlpProtocol::Grpc),
        "http/protobuf" => Some(OtlpProtocol::HttpBinary),
        "http/json" => Some(OtlpProtocol::HttpJson),
        _ => None,
    }
}

/// Returns true when `OTEL_SDK_DISABLED` is set to a truthy value.
pub(crate) fn sdk_disabled() -> bool {
    matches!(
        env::var("OTEL_SDK_DISABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn sdk_error(context: &str, err: OTelSdkError) -> Error {
    Error::new(otel_result_t::OTEL_ERR_EXPORT, format!("{context}: {err}"))
}

pub(crate) fn build_error<E: Display>(context: &str, err: E) -> Error {
    Error::new(otel_result_t::OTEL_ERR_CONFIG, format!("{context}: {err}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes process-wide environment changes made by tests and restores
    /// every touched variable when dropped.
    pub(crate) struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        pub(crate) fn new() -> Self {
            Self {
                _lock: ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner),
                saved: Vec::new(),
            }
        }

        fn remember(&mut self, name: &'static str) {
            if !self.saved.iter().any(|(saved, _)| *saved == name) {
                self.saved.push((name, env::var_os(name)));
            }
        }

        pub(crate) fn set(&mut self, name: &'static str, value: &str) {
            self.remember(name);
            // SAFETY: the ENV_LOCK guard is held for the lifetime of `self`, so
            // no other test in this crate observes or mutates the environment.
            unsafe { env::set_var(name, value) };
        }

        pub(crate) fn remove(&mut self, name: &'static str) {
            self.remember(name);
            // SAFETY: see `set` above; access is serialized by ENV_LOCK.
            unsafe { env::remove_var(name) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, previous) in self.saved.iter().rev() {
                // SAFETY: the ENV_LOCK guard is still held while `self` drops.
                unsafe {
                    match previous {
                        Some(value) => env::set_var(name, value),
                        None => env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn exporter_kind_accepts_known_values() {
        let mut env = EnvGuard::new();
        // Tests set variables one at a time; use unique signals per case.
        env.set("OTEL_TEST_EXPORTER", "stdout");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Stdout);
        env.set("OTEL_TEST_EXPORTER", "CONSOLE");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Stdout);
        env.set("OTEL_TEST_EXPORTER", "item");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Item);
        env.set("OTEL_TEST_EXPORTER", "MEMORY");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Item);
        env.set("OTEL_TEST_EXPORTER", "none");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::None);
        env.set("OTEL_TEST_EXPORTER", "bogus");
        assert_eq!(
            exporter_kind("TEST").unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.remove("OTEL_TEST_EXPORTER");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Otlp);
    }

    #[test]
    fn duration_ms_parses_and_defaults() {
        let mut env = EnvGuard::new();
        env.set("OTEL_TEST_DELAY", "250");
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap(),
            Duration::from_millis(250)
        );
        env.set("OTEL_TEST_DELAY", "nope");
        assert!(duration_ms("OTEL_TEST_DELAY", 100).is_err());
        env.set("OTEL_TEST_DELAY", "-1");
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.set("OTEL_TEST_DELAY", "  0 ");
        assert_eq!(duration_ms("OTEL_TEST_DELAY", 100).unwrap(), Duration::ZERO);
        env.set("OTEL_TEST_DELAY", "  ");
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap(),
            Duration::from_millis(100)
        );
        env.remove("OTEL_TEST_DELAY");
        assert_eq!(
            duration_ms("OTEL_TEST_DELAY", 100).unwrap(),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn parse_protocol_accepts_standard_values() {
        assert_eq!(parse_protocol("grpc"), Some(OtlpProtocol::Grpc));
        assert_eq!(
            parse_protocol("HTTP/Protobuf"),
            Some(OtlpProtocol::HttpBinary)
        );
        assert_eq!(parse_protocol(" http/json "), Some(OtlpProtocol::HttpJson));
        assert_eq!(parse_protocol("bogus"), None);
        assert_eq!(parse_protocol(""), None);
    }

    #[test]
    fn otlp_timeout_prefers_signal_variable() {
        let mut env = EnvGuard::new();
        env.set("OTEL_EXPORTER_OTLP_TEST_TIMEOUT", "250");
        env.set("OTEL_EXPORTER_OTLP_TIMEOUT", "999");
        assert_eq!(otlp_timeout("TEST").unwrap(), Duration::from_millis(250));
        env.remove("OTEL_EXPORTER_OTLP_TEST_TIMEOUT");
        assert_eq!(otlp_timeout("TEST").unwrap(), Duration::from_millis(999));
        env.set("OTEL_EXPORTER_OTLP_TIMEOUT", "bad");
        assert_eq!(
            otlp_timeout("TEST").unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
        env.remove("OTEL_EXPORTER_OTLP_TIMEOUT");
        assert_eq!(otlp_timeout("TEST").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn exporter_kind_trims_whitespace_and_reports_bad_values() {
        let mut env = EnvGuard::new();
        env.set("OTEL_TEST_EXPORTER", "  OTLP  ");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Otlp);
        env.set("OTEL_TEST_EXPORTER", " stdout ");
        assert_eq!(exporter_kind("TEST").unwrap(), ExporterKind::Stdout);
        env.set("OTEL_TEST_EXPORTER", "BOGUS ");
        let error = exporter_kind("TEST").unwrap_err();
        assert_eq!(error.status, otel_result_t::OTEL_ERR_CONFIG);
    }

    #[test]
    fn otlp_protocol_prefers_signal_variable_and_defaults_to_json() {
        let mut env = EnvGuard::new();
        env.set("OTEL_EXPORTER_OTLP_TEST_PROTOCOL", "grpc");
        env.set("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json");
        assert_eq!(otlp_protocol("TEST").unwrap(), OtlpProtocol::Grpc);
        env.set("OTEL_EXPORTER_OTLP_TEST_PROTOCOL", "  ");
        assert_eq!(otlp_protocol("TEST").unwrap(), OtlpProtocol::HttpJson);
        env.set("OTEL_EXPORTER_OTLP_TEST_PROTOCOL", "http/protobuf");
        assert_eq!(otlp_protocol("TEST").unwrap(), OtlpProtocol::HttpBinary);
        env.remove("OTEL_EXPORTER_OTLP_TEST_PROTOCOL");
        assert_eq!(otlp_protocol("TEST").unwrap(), OtlpProtocol::HttpJson);
        env.set("OTEL_EXPORTER_OTLP_TEST_PROTOCOL", "bogus");
        assert_eq!(
            otlp_protocol("TEST").unwrap_err().status,
            otel_result_t::OTEL_ERR_CONFIG
        );
    }

    #[test]
    fn sdk_disabled_recognizes_truthy_values() {
        let mut env = EnvGuard::new();
        for value in ["1", "TRUE", " yes ", "On"] {
            env.set("OTEL_SDK_DISABLED", value);
            assert!(sdk_disabled(), "{value} should disable the SDK");
        }
        for value in ["0", "false", "no", "off", ""] {
            env.set("OTEL_SDK_DISABLED", value);
            assert!(!sdk_disabled(), "{value} should not disable the SDK");
        }
        env.remove("OTEL_SDK_DISABLED");
        assert!(!sdk_disabled());
    }

    #[test]
    fn resource_from_env_includes_caller_attributes() {
        let resource = resource_from_env_with(vec![KeyValue::new("caller", "value")]);
        let attributes: Vec<_> = resource.iter().collect();
        assert!(
            attributes
                .iter()
                .any(|(key, value)| key.as_str() == "caller" && value.to_string() == "value")
        );
    }

    #[test]
    fn error_helpers_keep_context() {
        let error = sdk_error("flush", OTelSdkError::InternalFailure("broken".into()));
        assert_eq!(error.status, otel_result_t::OTEL_ERR_EXPORT);
        assert!(error.message.starts_with("flush: "));
        let error = build_error("config", std::fmt::Error);
        assert_eq!(error.status, otel_result_t::OTEL_ERR_CONFIG);
        assert!(error.message.starts_with("config: "));
        let error = Error::from(env::VarError::NotPresent);
        assert_eq!(error.status, otel_result_t::OTEL_ERR_CONFIG);
    }
}
