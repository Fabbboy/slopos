use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SLOPOS_KSYMS_RS");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let dst = out_dir.join("kallsyms.rs");

    if let Some(src) = env::var_os("SLOPOS_KSYMS_RS") {
        // Track the symbol table by content: rerun (and recompile) only when
        // it changes, so a rebuild with unchanged symbols is a cache hit
        // rather than a forced recompile of the whole kernel every phase.
        println!("cargo:rerun-if-changed={}", PathBuf::from(&src).display());
        fs::copy(src, &dst).expect("copy generated SlopOS symbol table");
    } else {
        fs::write(
            &dst,
            "pub static KERNEL_SYMBOLS: &[crate::ksym::KernelSymbol] = &[];\n",
        )
        .expect("write empty SlopOS symbol table");
    }
}
