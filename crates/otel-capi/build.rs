use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Caller-owned handle structs. This is the single source of truth for the
/// generated `OTEL_*_T_INIT`, `OTEL_AUTO_*`, and cleanup helpers.
const HANDLES: &[&str] = &[
    "otel_context_t",
    "otel_span_t",
    "otel_context_scope_t",
    "otel_resource_t",
    "otel_runtime_t",
    "otel_tracer_provider_t",
    "otel_meter_provider_t",
    "otel_logger_provider_t",
    "otel_tracer_t",
    "otel_meter_t",
    "otel_logger_t",
    "otel_counter_u64_t",
    "otel_counter_f64_t",
    "otel_updown_counter_i64_t",
    "otel_histogram_f64_t",
    "otel_gauge_f64_t",
    "otel_observable_gauge_f64_t",
    "otel_observable_counter_u64_t",
];

/// Convenience constructors for the C `otel_attribute_t` struct. cbindgen
/// cannot parse macro-generated Rust functions, so these are appended here.
const ATTRIBUTE_MACROS: &str = r#"
/* Length-aware views. OTEL_STR uses literal sizes at compile time;
 * OTEL_STR_VIEW measures a NUL-terminated string at runtime. */
#define OTEL_STR(literal) \
  ((otel_str_t){ .ptr = (const char *)(literal), .len = sizeof(literal) - 1 })
#define OTEL_STR_VIEW(text) \
  ((otel_str_t){ .ptr = (const char *)(text), .len = (text) ? strlen(text) : 0 })
#define OTEL_STR_NULL ((otel_str_t){ .ptr = NULL, .len = 0 })

/* Attribute builders. Pass an OTEL_ATTR_END-terminated list of these to any
 * function that takes variadic attributes, for example:
 *
 *   otel_span_set_attributes(span,
 *       otel_attr_string("endpoint", "/checkout"),
 *       otel_attr_i64("retries", 2),
 *       OTEL_ATTR_END);
 *
 * The _n forms take otel_str_t views, so keys and string values may contain
 * embedded bytes and need not be NUL-terminated.
 */
#define otel_attr_string_n(key_, value_) \
  (&(otel_attribute_t){ .kind = OTEL_ATTRIBUTE_TYPE_STRING, \
                       .key = (key_).ptr, .key_len = (key_).len, \
                       .value = { .string = (value_).ptr }, .value_len = (value_).len })
#define otel_attr_bool_n(key_, value_) \
  (&(otel_attribute_t){ .kind = OTEL_ATTRIBUTE_TYPE_BOOL, \
                       .key = (key_).ptr, .key_len = (key_).len, \
                       .value = { .boolean = !!(value_) }, .value_len = 0 })
#define otel_attr_i64_n(key_, value_) \
  (&(otel_attribute_t){ .kind = OTEL_ATTRIBUTE_TYPE_I64, \
                       .key = (key_).ptr, .key_len = (key_).len, \
                       .value = { .integer = (int64_t)(value_) }, .value_len = 0 })
#define otel_attr_f64_n(key_, value_) \
  (&(otel_attribute_t){ .kind = OTEL_ATTRIBUTE_TYPE_F64, \
                       .key = (key_).ptr, .key_len = (key_).len, \
                       .value = { .double_value = (double)(value_) }, .value_len = 0 })
#define otel_attr_string(key_, value_) \
  otel_attr_string_n(OTEL_STR_VIEW(key_), OTEL_STR_VIEW(value_))
#define otel_attr_bool(key_, value_) \
  otel_attr_bool_n(OTEL_STR_VIEW(key_), (value_))
#define otel_attr_i64(key_, value_) \
  otel_attr_i64_n(OTEL_STR_VIEW(key_), (value_))
#define otel_attr_f64(key_, value_) \
  otel_attr_f64_n(OTEL_STR_VIEW(key_), (value_))
"#;

const LOG_MACROS: &str = r#"
/* One helper per severity level. Each appends OTEL_ATTR_END automatically, so
 * pass zero or more attribute builders as trailing arguments. The _N forms
 * take an otel_str_t body, for example OTEL_STR("message") or OTEL_STR_VIEW(s).
 * The _REC forms wrap otel_logger_log_record and override its level.
 */
static inline otel_log_record_options_t *otel_log_options_with_level(
    const otel_log_record_options_t *options_, int32_t level_,
    otel_log_record_options_t *slot_) {
  *slot_ = options_ ? *options_
                    : (otel_log_record_options_t)OTEL_LOG_RECORD_OPTIONS_INIT;
  slot_->size = sizeof(*slot_);
  slot_->level = level_;
  return slot_;
}
#define OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, level_)                        \
  otel_log_options_with_level((options_), (level_),                           \
                              &(otel_log_record_options_t){ .size = 0 })

#define OTEL_LOG_TRACE(logger_, span_, body_, ...)                             \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_TRACE, (body_)              \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_DEBUG(logger_, span_, body_, ...)                             \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_DEBUG, (body_)              \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_INFO(logger_, span_, body_, ...)                              \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_INFO, (body_)               \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_WARN(logger_, span_, body_, ...)                              \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_WARN, (body_)               \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_ERROR(logger_, span_, body_, ...)                             \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_ERROR, (body_)              \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_FATAL(logger_, span_, body_, ...)                             \
  otel_logger_log((logger_), (span_), OTEL_SEVERITY_FATAL, (body_)              \
                  __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_TRACE_N(logger_, span_, body_, ...)                           \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_TRACE, (body_)            \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_DEBUG_N(logger_, span_, body_, ...)                           \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_DEBUG, (body_)            \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_INFO_N(logger_, span_, body_, ...)                            \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_INFO, (body_)             \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_WARN_N(logger_, span_, body_, ...)                            \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_WARN, (body_)             \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_ERROR_N(logger_, span_, body_, ...)                           \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_ERROR, (body_)            \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_FATAL_N(logger_, span_, body_, ...)                           \
  otel_logger_log_n((logger_), (span_), OTEL_SEVERITY_FATAL, (body_)            \
                    __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)

#define OTEL_LOG_TRACE_REC(logger_, options_, body_, ...)                      \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_TRACE), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_DEBUG_REC(logger_, options_, body_, ...)                      \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_DEBUG), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_INFO_REC(logger_, options_, body_, ...)                       \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_INFO), (body_)       \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_WARN_REC(logger_, options_, body_, ...)                       \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_WARN), (body_)       \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_ERROR_REC(logger_, options_, body_, ...)                      \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_ERROR), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_FATAL_REC(logger_, options_, body_, ...)                      \
  otel_logger_log_record((logger_),                                            \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_FATAL), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_TRACE_REC_N(logger_, options_, body_, ...)                    \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_TRACE), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_DEBUG_REC_N(logger_, options_, body_, ...)                    \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_DEBUG), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_INFO_REC_N(logger_, options_, body_, ...)                     \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_INFO), (body_)       \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_WARN_REC_N(logger_, options_, body_, ...)                     \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_WARN), (body_)       \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_ERROR_REC_N(logger_, options_, body_, ...)                    \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_ERROR), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_FATAL_REC_N(logger_, options_, body_, ...)                    \
  otel_logger_log_record_n((logger_),                                          \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_FATAL), (body_)      \
      __VA_OPT__(, ) __VA_ARGS__, OTEL_ATTR_END)
#define OTEL_LOG_TRACE_REC_ARR(logger_, options_, body_, attrs_, count_)       \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_TRACE), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_DEBUG_REC_ARR(logger_, options_, body_, attrs_, count_)       \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_DEBUG), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_INFO_REC_ARR(logger_, options_, body_, attrs_, count_)        \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_INFO), (body_),      \
      (attrs_), (count_))
#define OTEL_LOG_WARN_REC_ARR(logger_, options_, body_, attrs_, count_)        \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_WARN), (body_),      \
      (attrs_), (count_))
#define OTEL_LOG_ERROR_REC_ARR(logger_, options_, body_, attrs_, count_)       \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_ERROR), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_FATAL_REC_ARR(logger_, options_, body_, attrs_, count_)       \
  otel_logger_log_record_arr((logger_),                                        \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_FATAL), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_TRACE_REC_ARR_N(logger_, options_, body_, attrs_, count_)     \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_TRACE), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_DEBUG_REC_ARR_N(logger_, options_, body_, attrs_, count_)     \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_DEBUG), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_INFO_REC_ARR_N(logger_, options_, body_, attrs_, count_)      \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_INFO), (body_),      \
      (attrs_), (count_))
#define OTEL_LOG_WARN_REC_ARR_N(logger_, options_, body_, attrs_, count_)      \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_WARN), (body_),      \
      (attrs_), (count_))
#define OTEL_LOG_ERROR_REC_ARR_N(logger_, options_, body_, attrs_, count_)     \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_ERROR), (body_),     \
      (attrs_), (count_))
#define OTEL_LOG_FATAL_REC_ARR_N(logger_, options_, body_, attrs_, count_)     \
  otel_logger_log_record_arr_n((logger_),                                      \
      OTEL_LOG_OPTIONS_WITH_LEVEL_(options_, OTEL_SEVERITY_FATAL), (body_),     \
      (attrs_), (count_))
"#;

const SEMCONV_MODULES: [&str; 4] = ["attribute", "metric", "resource", "trace"];

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let workspace = root
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&root)
        .to_path_buf();
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace.join("Cargo.lock").display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));

    let semconv = generate_semconv_header(&root);

    let config = cbindgen::Config::from_file(root.join("cbindgen.toml"))
        .expect("cbindgen.toml should be valid");

    let mut bytes = Vec::new();
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&root)
        .generate()
        .expect("cbindgen should generate the C header")
        .write(&mut bytes);
    let mut header = String::from_utf8(bytes).expect("generated header should be UTF-8");
    header = annotate_exports(&header);

    // Split the generated declarations into area headers and compose the
    // umbrella header from them. All files remain build outputs.
    let headers = compose_headers(&header, &semconv);
    let include = workspace.join("include");
    fs::create_dir_all(&include).expect("should create include/");
    for (name, contents) in headers {
        fs::write(out_dir.join(&name), &contents)
            .unwrap_or_else(|_| panic!("should write OUT_DIR/{name}"));
        fs::write(include.join(&name), &contents)
            .unwrap_or_else(|_| panic!("should write include/{name}"));
    }
}

const TYPED_BUFFER_MACROS: &str = r#"
/* Initializers for typed IDs and fixed-size propagation buffers. The *_STR
 * forms are checked by the compiler: a literal longer than the fixed storage
 * is a hard error. Trace and span IDs are numeric, so use *_from_str at
 * runtime to parse hex text. */
#define OTEL_TRACE_ID_INIT { .hi = 0, .lo = 0 }
#define OTEL_SPAN_ID_INIT { .value = 0 }
#define OTEL_TRACEPARENT_INIT { .value = { 0 } }
#define OTEL_TRACESTATE_INIT { .value = { 0 } }
#define OTEL_BAGGAGE_INIT { .value = { 0 } }
#define OTEL_BAGGAGE_VALUE_INIT { .value = { 0 } }
#define OTEL_TRACEPARENT_INIT_STR(s) { .value = s }
#define OTEL_TRACESTATE_INIT_STR(s) { .value = s }
#define OTEL_BAGGAGE_INIT_STR(s) { .value = s }
#define OTEL_BAGGAGE_VALUE_INIT_STR(s) { .value = s }
"#;

const TRANSPORT_MACROS: &str = r#"
/* Initializers for transport descriptors and application views. */
#define OTEL_TRANSPORT_INIT { .size = sizeof(otel_transport_t), .name = { NULL, 0 }, .send = NULL, .user_data = NULL }
#define OTEL_TRANSPORT_HEADER_INIT { .name = { NULL, 0 }, .value = { NULL, 0 } }
#define OTEL_TRANSPORT_REQUEST_INIT { .method = NULL, .url = NULL, .headers = NULL, .header_count = 0, .body = NULL, .body_len = 0, .timeout_ms = 0 }
#define OTEL_TRANSPORT_RESPONSE_INIT { .status_code = 0, .body = NULL, .body_len = 0, .headers = NULL, .header_count = 0, .error = { NULL, 0 } }
"#;

const ITEM_MACROS: &str = r#"
/* Initializers for in-memory item transport views. All data pointers are
 * borrowed and valid only for the duration of the callback. */
#define OTEL_ENCODED_ITEM_INIT { .signal = OTEL_ITEM_SIGNAL_TRACE, .data = NULL, .len = 0 }
#define OTEL_SPAN_EVENT_ITEM_INIT { .name = { NULL, 0 }, .timestamp_unix_nano = 0, .attributes = NULL, .attribute_count = 0 }
#define OTEL_SPAN_LINK_ITEM_INIT { .trace_id = OTEL_TRACE_ID_INIT, .span_id = OTEL_SPAN_ID_INIT, .attributes = NULL, .attribute_count = 0 }
#define OTEL_SPAN_ITEM_INIT { .trace_id = OTEL_TRACE_ID_INIT, .span_id = OTEL_SPAN_ID_INIT, .parent_span_id = OTEL_SPAN_ID_INIT, .name = { NULL, 0 }, .kind = OTEL_SPAN_KIND_INTERNAL, .status = OTEL_SPAN_STATUS_UNSET, .status_message = { NULL, 0 }, .start_time_unix_nano = 0, .end_time_unix_nano = 0, .attributes = NULL, .attribute_count = 0, .events = NULL, .event_count = 0, .links = NULL, .link_count = 0, .resource_attributes = NULL, .resource_attribute_count = 0 }
#define OTEL_LOG_ITEM_INIT { .timestamp_unix_nano = 0, .observed_timestamp_unix_nano = 0, .severity = OTEL_SEVERITY_INFO, .severity_text = { NULL, 0 }, .body = { NULL, 0 }, .trace_id = OTEL_TRACE_ID_INIT, .span_id = OTEL_SPAN_ID_INIT, .attributes = NULL, .attribute_count = 0, .resource_attributes = NULL, .resource_attribute_count = 0 }
#define OTEL_METRIC_POINT_INIT { .attributes = NULL, .attribute_count = 0, .value_kind = OTEL_METRIC_VALUE_U64, .value = { .as_u64 = 0 } }
#define OTEL_HISTOGRAM_POINT_INIT { .attributes = NULL, .attribute_count = 0, .count = 0, .sum = 0, .has_min = false, .min = 0, .has_max = false, .max = 0, .bucket_boundaries = NULL, .bucket_counts = NULL, .bucket_count = 0 }
#define OTEL_METRIC_ITEM_INIT { .name = { NULL, 0 }, .description = { NULL, 0 }, .unit = { NULL, 0 }, .kind = OTEL_METRIC_ITEM_GAUGE, .points = NULL, .point_count = 0, .histograms = NULL, .histogram_count = 0, .resource_attributes = NULL, .resource_attribute_count = 0 }
#define OTEL_ITEM_TRANSPORT_INIT { .size = sizeof(otel_item_transport_t), .on_span = NULL, .on_log = NULL, .on_metric = NULL, .on_encoded = NULL, .user_data = NULL }
"#;

const OWNERSHIP_DOC: &str = r#"/* Ownership convention
 *
 * Handles are caller-allocated. Declare `otel_foo_t foo = {0};` and pass a
 * pointer to the matching `otel_foo_new` or `otel_foo_retain` function; the
 * library writes into that storage and never allocates the struct. On success
 * the struct owns one reference, so pair it with `otel_foo_release`, which
 * clears the struct and is safe to call on a zero-initialized one.
 *
 * All strings are borrowed for the duration of a call only.
 */
"#;

/// Builds the public header set from a single cbindgen header. Types live in
/// `otel_capi_common.h`; function declarations are grouped by area.
fn compose_headers(header: &str, semconv: &str) -> Vec<(String, String)> {
    let first_api = header
        .match_indices("\nOTEL_API ")
        .map(|(offset, _)| offset + 1)
        .next()
        .expect("generated header should have exported functions");
    let first_type = header
        .find("typedef")
        .expect("generated header should have types");
    let functions_start = header[first_type..first_api]
        .rfind("/**")
        .map(|offset| first_type + offset)
        .unwrap_or(first_api);
    let types = &header[first_type..functions_start];
    let functions_end = header[functions_start..]
        .find("#endif")
        .map(|offset| functions_start + offset)
        .unwrap_or(header.len());
    let functions = &header[functions_start..functions_end];

    let mut common_body = String::new();
    common_body.push_str("#include <stdarg.h>\n#include <stdbool.h>\n#include <stddef.h>\n");
    common_body.push_str("#include <stdint.h>\n#include <stdlib.h>\n#include <string.h>\n");
    common_body
        .push_str("#ifndef OTEL_API\n#  if defined(_WIN32) && defined(OTEL_CAPI_BUILDING_DLL)\n");
    common_body.push_str("#    define OTEL_API __declspec(dllexport)\n#  elif defined(_WIN32)\n");
    common_body.push_str("#    define OTEL_API __declspec(dllimport)\n#  elif defined(__GNUC__)\n");
    common_body.push_str("#    define OTEL_API __attribute__((visibility(\"default\")))\n");
    common_body.push_str("#  else\n#    define OTEL_API\n#  endif\n#endif\n\n");
    common_body.push_str(OWNERSHIP_DOC);
    common_body.push('\n');
    common_body.push_str(types);
    common_body.push_str(ATTRIBUTE_MACROS);
    common_body.push_str(TRANSPORT_MACROS);
    common_body.push_str(ITEM_MACROS);
    common_body.push_str("\n/* Sentinel values, handle initializers, and varargs terminator. */\n");
    common_body.push_str(&sentinel_macros(header));
    common_body.push_str(&handle_init_macros());
    common_body.push_str(
        "#define OTEL_SPAN_OPTIONS_INIT { .size = sizeof(otel_span_options_t), .kind = -1, .start_time_unix_nano = 0, .parent = NULL, .end_on_release = 0 }\n",
    );
    common_body.push_str("#define OTEL_LOG_RECORD_OPTIONS_INIT { .size = sizeof(otel_log_record_options_t), .level = OTEL_SEVERITY_INFO, .event_name = NULL, .target = NULL, .severity_text = NULL, .timestamp_unix_nano = 0, .observed_timestamp_unix_nano = 0, .span = NULL }\n");
    common_body.push_str("#define OTEL_ATTR_END ((otel_attribute_t *)0)\n");
    common_body.push_str(LOG_MACROS);
    common_body.push_str(TYPED_BUFFER_MACROS);
    common_body.push_str("\n/* Compile-time version and error helpers. */\n");
    common_body.push_str(&version_macros());
    common_body
        .push_str("#define OTEL_RETURN_IF_ERROR(call)                                        \\\n");
    common_body
        .push_str("  do {                                                                   \\\n");
    common_body
        .push_str("    otel_result_t otel_result__ = (call);                               \\\n");
    common_body
        .push_str("    if (otel_result__ != OTEL_OK) return otel_result__;                 \\\n");
    common_body.push_str("  } while (0)\n");
    common_body.push_str("\n/* Generated semantic-convention attribute helpers. */\n");
    common_body.push_str("#include \"otel_capi_semconv_attributes.h\"\n");

    let mut areas: BTreeMap<&str, String> = BTreeMap::new();
    for block in function_blocks(functions) {
        let area = area_for_block(&block);
        let body = areas.entry(area).or_default();
        body.push_str(&block);
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
    }
    if let Some(handles) = areas.get_mut("otel_capi_handles.h") {
        handles.push_str("\n/* Scope cleanup helpers for caller-owned handles. */\n");
        handles.push_str(&cleanup_macros());
    }
    if let Some(metrics) = areas.get_mut("otel_capi_metrics.h") {
        metrics.push_str("\n/* Generated semantic-convention metric helpers. */\n");
        metrics.push_str("#include \"otel_capi_semconv_metrics.h\"\n");
    }

    let (semconv_attributes, semconv_metrics) = split_semconv(semconv);
    let semconv_umbrella = concat!(
        "/* Generated by build.rs from opentelemetry-semantic-conventions. */\n",
        "#ifndef OTEL_CAPI_SEMCONV_H\n#define OTEL_CAPI_SEMCONV_H\n\n",
        "#include \"otel_capi_semconv_attributes.h\"\n",
        "#include \"otel_capi_semconv_metrics.h\"\n\n",
        "#endif /* OTEL_CAPI_SEMCONV_H */\n"
    );
    let mut headers = vec![
        ("otel_capi_semconv.h".to_string(), semconv_umbrella.to_string()),
        ("otel_capi_semconv_attributes.h".to_string(), semconv_attributes),
        ("otel_capi_semconv_metrics.h".to_string(), semconv_metrics),
        (
            "otel_capi_common.h".to_string(),
            wrap_header("OTEL_CAPI_COMMON_H", &[], &common_body),
        ),
    ];
    for (file, body) in areas {
        let guard = file
            .strip_prefix("otel_capi_")
            .and_then(|name| name.strip_suffix(".h"))
            .expect("area header name")
            .to_ascii_uppercase();
        let guard = format!("OTEL_CAPI_{guard}_H");
        headers.push((
            file.to_string(),
            wrap_header(&guard, &["otel_capi_common.h"], &body),
        ));
    }

    let umbrella = concat!(
        "/* Generated by otel-capi/build.rs. Do not edit; edit the Rust sources instead. */\n",
        "#ifndef OTEL_CAPI_H\n#define OTEL_CAPI_H\n\n",
        "#include \"otel_capi_common.h\"\n",
        "#include \"otel_capi_error.h\"\n",
        "#include \"otel_capi_handles.h\"\n",
        "#include \"otel_capi_resource.h\"\n",
        "#include \"otel_capi_runtime.h\"\n",
        "#include \"otel_capi_context.h\"\n",
        "#include \"otel_capi_trace.h\"\n",
        "#include \"otel_capi_metrics.h\"\n",
        "#include \"otel_capi_logs.h\"\n",
        "#include \"otel_capi_grpc_transport.h\"\n",
        "#include \"otel_capi_http_transport.h\"\n",
        "#include \"otel_capi_stdout_transport.h\"\n\n",
        "#endif /* OTEL_CAPI_H */\n"
    );
    headers.push(("otel_capi.h".to_string(), umbrella.to_string()));
    headers
}

/// Splits the generated semantic-convention header into attribute helpers and
/// metric helpers so metric declarations are available before metric helpers.
fn split_semconv(semconv: &str) -> (String, String) {
    let metric_marker = "/* Metric helpers using the semconv instrument, unit, and description. */";
    let metric_start = semconv
        .find(metric_marker)
        .expect("semconv header should have metric helpers");
    let end = semconv
        .find("#endif /* OTEL_CAPI_SEMCONV_H */")
        .expect("semconv header should end with a guard");

    let attributes =
        semconv[..metric_start].replace("OTEL_CAPI_SEMCONV_H", "OTEL_CAPI_SEMCONV_ATTRIBUTES_H");
    let attributes = format!("{attributes}#endif /* OTEL_CAPI_SEMCONV_ATTRIBUTES_H */\n");

    let mut metrics =
        String::from("/* Generated by build.rs from opentelemetry-semantic-conventions. */\n");
    metrics.push_str("#ifndef OTEL_CAPI_SEMCONV_METRICS_H\n");
    metrics.push_str("#define OTEL_CAPI_SEMCONV_METRICS_H\n\n");
    metrics.push_str("#include \"otel_capi_common.h\"\n#include \"otel_capi_metrics.h\"\n\n");
    metrics.push_str(&semconv[metric_start..end]);
    metrics.push_str("#endif /* OTEL_CAPI_SEMCONV_METRICS_H */\n");

    (attributes, metrics)
}

fn wrap_header(guard: &str, includes: &[&str], body: &str) -> String {
    let mut result = String::from(
        "/* Generated by otel-capi/build.rs. Do not edit; edit the Rust sources instead. */\n",
    );
    result.push_str(&format!("#ifndef {guard}\n#define {guard}\n\n"));
    for include in includes {
        result.push_str(&format!("#include \"{include}\"\n"));
    }
    if !includes.is_empty() {
        result.push('\n');
    }
    result.push_str(body);
    if !body.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&format!("#endif /* {guard} */\n"));
    result
}

fn function_blocks(functions: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for line in functions.lines() {
        if line.trim_start().starts_with("/**") && !current.is_empty() {
            blocks.push(current.join("\n"));
            current.clear();
        }
        current.push(line.to_string());
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    blocks
}

fn exported_name(block: &str) -> Option<String> {
    let api = block.find("OTEL_API")?;
    let open = block[api..].find('(')? + api;
    let prefix = &block[api..open];
    let token = prefix.rsplit(char::is_whitespace).next()?;
    let name = token.trim_start_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
    (!name.is_empty()).then(|| name.to_string())
}

fn area_for_block(block: &str) -> &'static str {
    let Some(name) = exported_name(block) else {
        return "otel_capi_error.h";
    };
    if name.starts_with("otel_trace_id_") || name.starts_with("otel_span_id_") {
        return "otel_capi_trace.h";
    }
    if name.ends_with("_retain") || name.ends_with("_release") || name.ends_with("_is_valid") {
        return "otel_capi_handles.h";
    }
    if name.starts_with("otel_handle_")
        || matches!(
            name.as_str(),
            "otel_version"
                | "otel_last_error"
                | "otel_strerror"
                | "otel_copy_last_error"
                | "otel_live_handle_count"
                | "otel_ensure_all_released"
        )
    {
        return "otel_capi_error.h";
    }
    if name.starts_with("otel_context_")
        || name.starts_with("otel_traceparent_")
        || name.starts_with("otel_tracestate_")
        || name.starts_with("otel_baggage_")
    {
        return "otel_capi_context.h";
    }
    if name.starts_with("otel_logger_") {
        return "otel_capi_logs.h";
    }
    if name.starts_with("otel_resource_") {
        return "otel_capi_resource.h";
    }
    if name.starts_with("otel_runtime_") {
        return "otel_capi_runtime.h";
    }
    if name.starts_with("otel_meter_")
        || name.starts_with("otel_metric_")
        || name.starts_with("otel_counter_")
        || name.starts_with("otel_updown_counter_")
        || name.starts_with("otel_histogram_")
        || name.starts_with("otel_gauge_")
        || name.starts_with("otel_observable_")
        || name.starts_with("otel_observe_")
    {
        return "otel_capi_metrics.h";
    }
    "otel_capi_trace.h"
}

/// Prefixes exported declarations with `OTEL_API` for shared-library use.
fn annotate_exports(header: &str) -> String {
    let mut result = String::with_capacity(header.len());
    for line in header.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let exported = [
            "otel_result_t otel_",
            "const char *otel_",
            "otel_str_t otel_",
            "void otel_",
            "size_t otel_",
            "bool otel_",
        ]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix));
        if exported {
            let indent = &line[..line.len() - trimmed.len()];
            result.push_str(indent);
            result.push_str("OTEL_API ");
            result.push_str(trimmed);
        } else {
            result.push_str(line);
        }
    }
    result
}

/// Generates `OTEL_<ENUM>_SENTINEL` macros one past each exported enum's
/// largest value. C callers can use these in exhaustive switches.
fn sentinel_macros(header: &str) -> String {
    let mut result = String::new();
    for ty in [
        "otel_result_t",
        "otel_severity_t",
        "otel_span_status_t",
        "otel_span_kind_t",
        "otel_attribute_type_t",
        "otel_item_signal_t",
        "otel_metric_item_type_t",
        "otel_metric_value_kind_t",
    ] {
        let marker = format!("}} {ty};");
        let Some(end) = header.find(&marker) else {
            continue;
        };
        let Some(start) = header[..end].rfind("typedef enum {") else {
            continue;
        };
        let body = &header[start + "typedef enum {".len()..end];
        let mut max = -1i64;
        let mut next = 0i64;
        for part in body.split(',') {
            let part = part.trim();
            if let Some(eq) = part.find('=') {
                let digits: String = part[eq + 1..]
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Ok(value) = digits.parse::<i64>() {
                    max = max.max(value);
                    next = value + 1;
                }
            } else if part.starts_with("OTEL_") {
                max = max.max(next);
                next += 1;
            }
        }
        let macro_name = ty
            .strip_prefix("otel_")
            .and_then(|name| name.strip_suffix("_t"))
            .unwrap_or(ty)
            .to_ascii_uppercase();
        result.push_str(&format!("#define OTEL_{macro_name}_SENTINEL {}\n", max + 1));
    }
    result
}

/// Generates `OTEL_<TYPE>_INIT` macros from [`HANDLES`].
fn handle_init_macros() -> String {
    HANDLES
        .iter()
        .map(|ty| format!("#define {}_INIT {{ .inner = NULL }}\n", ty.to_ascii_uppercase()))
        .collect()
}

/// Returns the short name (without `otel_` / `_t`) for a handle type.
fn handle_short_name(ty: &str) -> &str {
    ty.strip_prefix("otel_")
        .and_then(|name| name.strip_suffix("_t"))
        .unwrap_or(ty)
}

/// Emits compile-time version constants derived from `CARGO_PKG_VERSION`.
fn version_macros() -> String {
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set");
    let mut parts = version.split('.');
    let mut number = || -> u32 {
        parts
            .next()
            .unwrap_or("0")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    let major = number();
    let minor = number();
    let patch = number();
    format!(
        "#define OTEL_VERSION_STRING \"{version}\"\n\
         #define OTEL_VERSION_MAJOR {major}\n\
         #define OTEL_VERSION_MINOR {minor}\n\
         #define OTEL_VERSION_PATCH {patch}\n\
         #define OTEL_VERSION_CODE ((OTEL_VERSION_MAJOR << 16) | (OTEL_VERSION_MINOR << 8) | OTEL_VERSION_PATCH)\n\
         #define OTEL_VERSION_AT_LEAST(major, minor, patch) \\\n\
           (OTEL_VERSION_CODE >= (((major) << 16) | ((minor) << 8) | (patch)))\n"
    )
}

/// Emits `OTEL_AUTO_<TYPE>` declarations from [`HANDLES`]. GCC and Clang get
/// scope cleanup via `__attribute__((cleanup))`; other compilers get no-op
/// macros so source stays portable.
fn cleanup_macros() -> String {
    let mut result = String::from("#if defined(__GNUC__) || defined(__clang__)\n");
    result.push_str("#define OTEL_CLEANUP(fn) __attribute__((cleanup(fn)))\n");
    for ty in HANDLES {
        let short = handle_short_name(ty);
        let helper = format!("otel_cleanup_{short}");
        result.push_str(&format!("static inline void {helper}({ty} *handle) {{\n"));
        result.push_str("  if (handle != NULL) {\n");
        result.push_str(&format!("    otel_{short}_release(handle);\n"));
        result.push_str("  }\n}\n");
        result.push_str(&format!(
            "#define OTEL_AUTO_{} OTEL_CLEANUP({helper})\n",
            short.to_ascii_uppercase()
        ));
    }
    result.push_str("#else\n#define OTEL_CLEANUP(fn)\n");
    for ty in HANDLES {
        let short = handle_short_name(ty);
        result.push_str(&format!(
            "#define OTEL_AUTO_{}\n",
            short.to_ascii_uppercase()
        ));
    }
    result.push_str("#endif\n");
    result
}

/// Locates the semantic-conventions crate through `cargo metadata` (rather
/// than a hard-coded registry path) and turns its `&str` constants into C
/// macros named `OTEL_SEMCONV_<MODULE>_<CONSTANT>`.
fn generate_semconv_header(root: &Path) -> String {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let metadata = Command::new(&cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(root)
        .output()
        .expect("cargo metadata should run");
    assert!(
        metadata.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata.stdout).expect("cargo metadata should be valid JSON");
    let package = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages.iter().find(|package| {
                package["name"].as_str() == Some("opentelemetry-semantic-conventions")
                    && package["version"].as_str() == Some("0.33.1")
            })
        })
        .expect("opentelemetry-semantic-conventions 0.33.1 should be a build dependency");

    let manifest = PathBuf::from(
        package["manifest_path"]
            .as_str()
            .expect("manifest_path should be a string"),
    );
    let src = manifest
        .parent()
        .expect("manifest should have a parent")
        .join("src");
    println!("cargo:rerun-if-changed={}", src.display());

    // Use the crate itself for the schema URL so the build dependency is real.
    let schema_url = opentelemetry_semantic_conventions::SCHEMA_URL;

    let mut macros: BTreeSet<(String, String, String)> = BTreeSet::new();
    // (lowercase helper name, builder macro, constant macro)
    let mut helpers: BTreeSet<(String, String, String)> = BTreeSet::new();
    // (lowercase helper name, constructor, constant macro, unit, description)
    let mut metric_helpers: BTreeSet<(String, String, String, String, String)> = BTreeSet::new();
    for module in SEMCONV_MODULES {
        let source = fs::read_to_string(src.join(format!("{module}.rs")))
            .unwrap_or_else(|_| panic!("semconv {module}.rs should be readable"));
        let mut docs: Vec<String> = Vec::new();
        for line in source.lines() {
            if let Some(doc) = line.strip_prefix("///") {
                docs.push(doc.trim().to_string());
                continue;
            }
            if line.starts_with("#[") {
                continue;
            }
            let Some(rest) = line.strip_prefix("pub const ") else {
                if !line.trim().is_empty() {
                    docs.clear();
                }
                continue;
            };
            let Some((name, value)) = rest.split_once(": &str = ") else {
                docs.clear();
                continue;
            };
            let value = value.trim().trim_end_matches(';').trim();
            if !value.starts_with('"') || !value.ends_with('"') {
                docs.clear();
                continue;
            }
            let c_value = c_string(&unescape_rust(&value[1..value.len() - 1]));
            macros.insert((module.to_string(), name.to_string(), c_value));

            // The crate exposes only string keys; infer the value type from
            // the doc examples and emit a typed convenience macro when the
            // examples agree on exactly one type.
            if module == "attribute" {
                if let Some(builder) = infer_builder(&docs) {
                    let helper = format!("otel_attr_{}", name.to_ascii_lowercase());
                    if !matches!(
                        helper.as_str(),
                        "otel_attr_string" | "otel_attr_bool" | "otel_attr_i64" | "otel_attr_f64"
                    ) {
                        helpers.insert((
                            helper,
                            builder.to_string(),
                            format!("OTEL_ATTRIBUTE_{name}"),
                        ));
                    }
                }
            }
            if module == "metric" {
                if let Some((constructor, unit, description)) = metric_constructor(&docs) {
                    metric_helpers.insert((
                        format!("otel_metric_{}", name.to_ascii_lowercase()),
                        constructor.to_string(),
                        format!("OTEL_METRIC_{name}"),
                        c_string(&unit),
                        c_string(&description),
                    ));
                }
            }
            docs.clear();
        }
    }
    assert!(
        !macros.is_empty(),
        "semantic convention constants should exist"
    );
    assert!(!helpers.is_empty(), "typed attribute helpers should exist");
    assert!(!metric_helpers.is_empty(), "metric helpers should exist");

    let mut header = String::new();
    header.push_str("/* Generated by build.rs from opentelemetry-semantic-conventions. */\n");
    header.push_str("#ifndef OTEL_CAPI_SEMCONV_H\n");
    header.push_str("#define OTEL_CAPI_SEMCONV_H\n\n");
    header.push_str(&format!("/* Schema: {} */\n\n", c_string(&schema_url)));
    let mut previous_module = "";
    for (module, name, value) in &macros {
        if module != previous_module {
            header.push_str(&format!("/* {} */\n", module));
            previous_module = module;
        }
        header.push_str(&format!(
            "#define OTEL_{}_{} \"{}\"\n",
            module.to_ascii_uppercase(),
            name,
            value
        ));
    }
    header.push_str("\n/* Typed helpers inferred from semantic-convention examples. */\n");
    for (helper, builder, constant) in &helpers {
        let parameter = match builder.as_str() {
            "string" => "const char *value",
            "bool" => "int value",
            "i64" => "int64_t value",
            "f64" => "double value",
            other => panic!("unexpected attribute builder {other}"),
        };
        let parameter_n = match builder.as_str() {
            "string" => "otel_str_t value",
            _other => parameter,
        };
        let value_helper = format!(
            "otel_attr_value_{}",
            helper.trim_start_matches("otel_attr_")
        );
        header.push_str(&format!(
            "static inline const otel_attribute_t *{}({}, otel_attribute_t *slot) {{\n    *slot = *otel_attr_{}({}, (value));\n    return slot;\n}}\n\n",
            value_helper, parameter, builder, constant
        ));
        header.push_str(&format!(
            "#define {}(value) {}((value), &(otel_attribute_t){{0}})\n\n",
            helper, value_helper
        ));
        header.push_str(&format!(
            "static inline const otel_attribute_t *{}_n({}, otel_attribute_t *slot) {{\n    *slot = *otel_attr_{}_n(OTEL_STR({}), (value));\n    return slot;\n}}\n\n",
            value_helper, parameter_n, builder, constant
        ));
        header.push_str(&format!(
            "#define {}_n(value) {}_n((value), &(otel_attribute_t){{0}})\n\n",
            helper, value_helper
        ));
    }
    header
        .push_str("\n/* Metric helpers using the semconv instrument, unit, and description. */\n");
    for (helper, constructor, constant, unit, description) in &metric_helpers {
        let handle = match constructor.as_str() {
            "otel_meter_counter_u64" => "otel_counter_u64_t *handle",
            "otel_meter_updown_counter_i64" => "otel_updown_counter_i64_t *handle",
            "otel_meter_gauge_f64" => "otel_gauge_f64_t *handle",
            "otel_meter_histogram_f64" => "otel_histogram_f64_t *handle",
            other => panic!("unexpected metric constructor {other}"),
        };
        header.push_str(&format!(
            "static inline otel_result_t {}(const otel_meter_t *meter, {}) {{\n    return {}(meter, handle, {}, \"{}\", \"{}\");\n}}\n\n",
            helper, handle, constructor, constant, unit, description
        ));
        header.push_str(&format!(
            "static inline otel_result_t {}_n(const otel_meter_t *meter, {}) {{\n    return {}_n(meter, handle, OTEL_STR({}), OTEL_STR(\"{}\"), OTEL_STR(\"{}\"));\n}}\n\n",
            helper, handle, constructor, constant, unit, description
        ));
    }
    header.push_str("\n#endif /* OTEL_CAPI_SEMCONV_H */\n");

    header
}

/// Infers an attribute builder from the `# Examples` lines of a generated
/// semantic-convention constant. Returns `None` when examples are missing or
/// disagree on a type.
fn infer_builder(docs: &[String]) -> Option<&'static str> {
    let mut inferred = BTreeSet::new();
    for doc in docs.iter().filter_map(|line| line.strip_prefix("- ")) {
        let value = doc.trim().trim_matches('`').trim();
        let builder = if value.starts_with('"') && value.ends_with('"') {
            "string"
        } else if value == "true" || value == "false" {
            "bool"
        } else if value.parse::<i64>().is_ok() {
            "i64"
        } else if value.parse::<f64>().is_ok() && value.contains('.') {
            "f64"
        } else {
            continue;
        };
        inferred.insert(builder);
    }
    if inferred.len() == 1 {
        inferred.into_iter().next()
    } else {
        None
    }
}

/// Extracts the instrument, unit, and description from a metric constant's
/// generated doc comment. Deprecated aliases without metadata are skipped.
fn metric_constructor(docs: &[String]) -> Option<(&'static str, String, String)> {
    fn metadata(docs: &[String], key: &str) -> Option<String> {
        let prefix = format!("| {key}: | `");
        docs.iter()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|rest| rest.split_once('`'))
            .map(|(value, _)| value.to_string())
    }

    let instrument = metadata(docs, "Instrument")?;
    let constructor = match instrument.as_str() {
        "counter" => "otel_meter_counter_u64",
        "updowncounter" => "otel_meter_updown_counter_i64",
        "gauge" => "otel_meter_gauge_f64",
        "histogram" => "otel_meter_histogram_f64",
        _ => return None,
    };
    let unit = metadata(docs, "Unit")?;

    let start = docs.iter().position(|line| line == "## Description")?;
    let end = docs.iter().position(|line| line == "## Metadata")?;
    let description = docs[start + 1..end]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join(" ");
    if description.is_empty() {
        return None;
    }

    Some((constructor, unit, description))
}

fn unescape_rust(literal: &str) -> String {
    let mut result = String::new();
    let mut chars = literal.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('r') => result.push('\r'),
            Some('t') => result.push('\t'),
            Some('0') => result.push('\0'),
            Some('\\') => result.push('\\'),
            Some('"') => result.push('"'),
            Some('\'') => result.push('\''),
            Some('u') if chars.clone().next() == Some('{') => {
                chars.next();
                let mut hex = String::new();
                for ch in chars.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    hex.push(ch);
                }
                if let Ok(code) = u32::from_str_radix(&hex, 16) {
                    if let Some(value) = char::from_u32(code) {
                        result.push(value);
                    }
                }
            }
            Some(other) => result.push(other),
            None => result.push('\\'),
        }
    }
    result
}

fn c_string(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            other => result.push(other),
        }
    }
    result
}
