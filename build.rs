use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Convenience constructors for the C `otel_attribute_t` struct. cbindgen
/// cannot parse macro-generated Rust functions, so these are appended here.
const ATTRIBUTE_MACROS: &str = r#"
/* Attribute builders. Pass a NULL-terminated list of these to any function
 * that takes variadic attributes, for example:
 *
 *   otel_span_set_attributes(span,
 *       otel_attr_string("endpoint", "/checkout"),
 *       otel_attr_i64("retries", 2),
 *       NULL);
 */
#define otel_attr_string(key, value) \
  ((otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_STRING, \
                       (const char *)(key), \
                       { .string = (const char *)(value) } })
#define otel_attr_bool(key, value) \
  ((otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_BOOL, \
                       (const char *)(key), \
                       { .boolean = !!(value) } })
#define otel_attr_i64(key, value) \
  ((otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_I64, \
                       (const char *)(key), \
                       { .integer = (int64_t)(value) } })
#define otel_attr_f64(key, value) \
  ((otel_attribute_t){ OTEL_ATTRIBUTE_TYPE_F64, \
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

    // Insert helper macros before the include guard closes so they are part
    // of the guarded header content.
    let guard_end = header
        .rfind("#endif")
        .expect("generated header should have an include guard");
    let mut injected = String::from(ATTRIBUTE_MACROS);
    injected.push_str("\n/* Generated semantic-convention helpers. */\n");
    injected.push_str("#include \"cotel_semconv.h\"\n");
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
        header.push_str(&format!(
            "static inline otel_attribute_t {}({}) {{\n    return otel_attr_{}({}, (value));\n}}\n\n",
            helper, parameter, builder, constant
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
