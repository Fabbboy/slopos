use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../lib/klog.h");
    println!("cargo:rerun-if-changed=../../boot/kernel_panic.h");
    println!("cargo:rerun-if-changed=../../mm/kernel_heap.h");
    println!("cargo:rerun-if-changed=../../video/framebuffer.h");
    println!("cargo:rerun-if-changed=../../mm/phys_virt.h");
    println!("cargo:rerun-if-changed=../../boot/limine_protocol.h");

    let bindings = bindgen::Builder::default()
        // Kernel headers to parse
        .header("wrapper.h")
        // Target freestanding x86_64
        .use_core()
        .clang_arg("--target=x86_64-unknown-none")
        .clang_arg("-ffreestanding")
        .clang_arg("-nostdlib")
        .clang_arg("-fno-stack-protector")
        // Allow certain types
        .allowlist_function("klog_.*")
        .allowlist_function("kernel_panic")
        .allowlist_function("kmalloc")
        .allowlist_function("kfree")
        .allowlist_function("get_framebuffer_info")
        .allowlist_function("mm_phys_to_virt")
        .allowlist_function("is_hhdm_available")
        .allowlist_function("get_hhdm_offset")
        .allowlist_type("klog_level")
        .allowlist_type("framebuffer_info_t")
        // Disable layout tests (not supported in no_std)
        .layout_tests(false)
        // Rust 2024 edition requires unsafe extern blocks
        .generate_cstr(true)
        // Generate bindings
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
