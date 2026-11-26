use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Convenience constructors for the C `otel_attribute_t` struct. cbindgen
/// cannot parse macro-generated Rust functions, so these are appended here.
const ATTRIBUTE_MACROS: &str = r#"
/* Attribute builders. Pass an OTEL_ATTR_END-terminated list of these to any
 * function that takes variadic attributes, for example:
 *
 *   otel_span_set_attributes(span,
 *       otel_attr_string("endpoint", "/checkout"),
 *       otel_attr_i64("retries", 2),
 *       OTEL_ATTR_END);
 */
#define otel_attr_string(key, value) \
  (&(otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_STRING, \
                       (const char *)(key), \
                       { .string = (const char *)(value) } })
#define otel_attr_bool(key, value) \
  (&(otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_BOOL, \
                       (const char *)(key), \
                       { .boolean = !!(value) } })
#define otel_attr_i64(key, value) \
  (&(otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_I64, \
                       (const char *)(key), \
                       { .integer = (int64_t)(value) } })
#define otel_attr_f64(key, value) \
  (&(otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_F64, \
                       (const char *)(key), \
                       { .double_value = (double)(value) } })
"#;

const SEMCONV_MODULES: [&str; 4] = ["attribute", "metric", "resource", "trace"];

fn main() {
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
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

    // Insert helper macros before the include guard closes so they are part
    // of the guarded header content.
    let guard_end = header
        .rfind("#endif")
        .expect("generated header should have an include guard");
    let mut injected = String::from(ATTRIBUTE_MACROS);
    injected.push_str("\n/* Sentinel values, handle initializers, and varargs terminator. */\n");
    injected.push_str(&sentinel_macros(&header));
    injected.push_str(&handle_init_macros(&header));
    injected.push_str("#define OTEL_SPAN_OPTIONS_INIT { sizeof(otel_span_options_t), -1, 0, NULL, 0 }\n");
    injected.push_str("#define OTEL_LOG_RECORD_OPTIONS_INIT { sizeof(otel_log_record_options_t), OTEL_SEVERITY_INFO, NULL, NULL, NULL, 0, 0, NULL }\n");
    injected.push_str("#define OTEL_ATTR_END ((otel_attribute_t *)0)\n");
    injected.push_str("\n/* Generated semantic-convention helpers. */\n");
    injected.push_str("#include \"cotel_semconv.h\"\n");
    injected.push_str("\n#ifdef __cplusplus\n}\n#endif\n");
    header.insert_str(guard_end, &injected);

    // Expose the headers to dependents via OUT_DIR, and keep the checked-in
    // copies used by C consumers current.
    fs::write(out_dir.join("cotel.h"), &header).expect("should write OUT_DIR/cotel.h");
    fs::write(out_dir.join("cotel_semconv.h"), &semconv)
        .expect("should write OUT_DIR/cotel_semconv.h");

    let include = root.join("include");
    fs::create_dir_all(&include).expect("should create include/");
    fs::write(include.join("cotel.h"), &header).expect("should write include/cotel.h");
    fs::write(include.join("cotel_semconv.h"), &semconv)
        .expect("should write include/cotel_semconv.h");
}

/// Prefixes exported declarations with `OTEL_API` for shared-library use.
fn annotate_exports(header: &str) -> String {
    let mut result = String::with_capacity(header.len());
    for line in header.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let exported = [
            "otel_result_t otel_",
            "const char *otel_",
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
    ] {
        let marker = format!("}} {ty};");
        let Some(end) = header.find(&marker) else { continue };
        let Some(start) = header[..end].rfind("typedef enum {") else { continue };
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

/// Generates `OTEL_<TYPE>_INIT` macros from every caller-owned handle struct
/// in the generated header.
fn handle_init_macros(header: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    while let Some(rel) = header[pos..].find("typedef struct {") {
        let start = pos + rel;
        let Some(end_rel) = header[start..].find("} otel_") else { break };
        let body = &header[start + "typedef struct {".len()..start + end_rel];
        let type_start = start + end_rel + 2;
        let Some(suffix_rel) = header[type_start..].find("_t;") else { break };
        let ty = &header[type_start..type_start + suffix_rel + 2];
        if body.contains("void *inner;") {
            result.push_str(&format!("#define {}_INIT {{ NULL }}\n", ty.to_ascii_uppercase()));
        }
        pos = type_start + suffix_rel + 3;
    }
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
    let src = manifest.parent().expect("manifest should have a parent").join("src");
    println!(
        "cargo:rerun-if-changed={}",
        src.display()
    );

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
    assert!(!macros.is_empty(), "semantic convention constants should exist");
    assert!(!helpers.is_empty(), "typed attribute helpers should exist");
    assert!(!metric_helpers.is_empty(), "metric helpers should exist");

    let mut header = String::new();
    header.push_str("/* Generated by build.rs from opentelemetry-semantic-conventions. */\n");
    header.push_str("#ifndef COTEL_SEMCONV_H\n");
    header.push_str("#define COTEL_SEMCONV_H\n\n");
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
        let value_helper = format!("otel_attr_value_{}", helper.trim_start_matches("otel_attr_"));
        header.push_str(&format!(
            "static inline const otel_attribute_t *{}({}, otel_attribute_t *slot) {{\n    *slot = *otel_attr_{}({}, (value));\n    return slot;\n}}\n\n",
            value_helper, parameter, builder, constant
        ));
        header.push_str(&format!(
            "#define {}(value) {}((value), &(otel_attribute_t){{0}})\n\n",
            helper, value_helper
        ));
    }
    header.push_str("\n/* Metric helpers using the semconv instrument, unit, and description. */\n");
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
    }
    header.push_str("\n#endif /* COTEL_SEMCONV_H */\n");

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
