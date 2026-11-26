use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=src");

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let config = cbindgen::Config::from_file(root.join("cbindgen.toml"))
        .expect("cbindgen.toml should be valid");

    let mut header = Vec::new();
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&root)
        .generate()
        .expect("cbindgen should generate the C header")
        .write(&mut header);
    let header = String::from_utf8(header).expect("generated header should be UTF-8");

    // Expose the header to dependents via OUT_DIR, and keep the checked-in
    // copy used by C consumers current.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    fs::write(out_dir.join("cotel.h"), &header).expect("should write OUT_DIR/cotel.h");

    let checked_in = root.join("include").join("cotel.h");
    fs::create_dir_all(checked_in.parent().unwrap()).expect("should create include/");
    fs::write(&checked_in, &header).expect("should write include/cotel.h");
}
