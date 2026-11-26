use std::env;
use std::fs;
use std::path::PathBuf;

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

fn main() {
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=src");

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
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
    header.insert_str(guard_end, ATTRIBUTE_MACROS);

    // Expose the header to dependents via OUT_DIR, and keep the checked-in
    // copy used by C consumers current.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    fs::write(out_dir.join("cotel.h"), &header).expect("should write OUT_DIR/cotel.h");

    let checked_in = root.join("include").join("cotel.h");
    fs::create_dir_all(checked_in.parent().unwrap()).expect("should create include/");
    fs::write(&checked_in, &header).expect("should write include/cotel.h");
}
