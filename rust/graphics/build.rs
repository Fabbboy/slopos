use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Generate C bindings from Rust code using cbindgen
    let result = cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_language(cbindgen::Language::C)
        .with_include_guard("RUST_GRAPHICS_H")
        .with_no_includes()
        .with_sys_include("stdint.h")
        .with_sys_include("stddef.h")
        .generate();

    match result {
        Ok(bindings) => {
            bindings.write_to_file(out_dir.join("graphics.h"));
        }
        Err(e) => {
            eprintln!("cbindgen error: {:?}", e);
            // During development, don't fail the build
            // Just write an empty header
            std::fs::write(out_dir.join("graphics.h"),
                "/* cbindgen failed - check build output */\n#ifndef RUST_GRAPHICS_H\n#define RUST_GRAPHICS_H\n#endif\n")
                .unwrap();
        }
    }

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/buffer.rs");
    println!("cargo:rerun-if-changed=src/canvas.rs");
}
