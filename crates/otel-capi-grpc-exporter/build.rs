use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let workspace = root
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&root)
        .to_path_buf();
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let config = cbindgen::Config::from_file(root.join("cbindgen.toml"))
        .expect("cbindgen.toml should be valid");
    let mut bytes = Vec::new();
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&root)
        .generate()
        .expect("cbindgen should generate the exporter header")
        .write(&mut bytes);
    let header = annotate_exports(&String::from_utf8(bytes).expect("header should be UTF-8"));
    let header = with_handle_init(
        header,
        concat!(
            "\n/* Exporter handle and options initializers. */\n",
            "#define OTEL_GRPC_EXPORTER_T_INIT { .exporter = OTEL_EXPORTER_INIT, .inner = NULL }\n",
            "#define OTEL_GRPC_EXPORTER_OPTIONS_INIT { \\\n",
            "    .size = sizeof(otel_grpc_exporter_options_t), \\\n",
            "    .worker_count = 1 \\\n",
            "}\n"
        ),
    );

    let include = workspace.join("include");
    fs::create_dir_all(&include).expect("should create include/");
    fs::write(include.join("otel_capi_grpc_exporter.h"), header)
        .expect("should write otel_capi_grpc_exporter.h");
    remove_stale_header(&include.join("otel_capi_grpc_transport.h"));
}

fn remove_stale_header(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("should remove {}: {err}", path.display()),
    }
}

fn annotate_exports(header: &str) -> String {
    let mut result = String::with_capacity(header.len());
    for line in header.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let exported = ["otel_result_t otel_", "uint32_t otel_", "void otel_"]
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

fn with_handle_init(header: String, declaration: &str) -> String {
    let Some(marker) = header.rfind("#endif") else {
        return format!("{header}\n{declaration}");
    };
    format!(
        "{}\n{}\n{}",
        &header[..marker],
        declaration,
        &header[marker..]
    )
}
