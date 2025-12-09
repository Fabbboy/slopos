use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Generate C bindings from Rust code using cbindgen
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_language(cbindgen::Language::C)
        .with_include_guard("RUST_GRAPHICS_H")
        .with_no_includes()
        .with_sys_include("stdint.h")
        .with_sys_include("stddef.h")
        .generate()
        .expect("Unable to generate C bindings")
        .write_to_file(out_dir.join("graphics.h"));

    println!("cargo:rerun-if-changed=src/lib.rs");
}
