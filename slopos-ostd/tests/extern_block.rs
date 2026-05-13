//! Host-side tests for `slopos_ostd::ffi::extern_block!`.
//!
//! The macro wraps `unsafe extern "C" { … }` declarations and emits
//! safe `<name>_addr() -> *const <ty>` accessors for each `static`
//! item. `fn` items are consolidated inside the extern block but get
//! no safe wrapper (callers retain call-site `unsafe { … }`).
//!
//! Each test pairs a backing `#[unsafe(no_mangle)]` static or fn at
//! the test-file scope with an `extern_block!` invocation that imports
//! it by name. The Rust linker resolves the extern-side declaration
//! against the test-file-side definition.

// Backing definitions: each test gets its own `BACKING_*` symbol.

#[unsafe(no_mangle)]
static EXTERN_BLOCK_TEST_BYTE: u8 = 0xAB;

#[unsafe(no_mangle)]
pub extern "C" fn extern_block_test_inc(x: u32) -> u32 {
    x + 1
}

#[unsafe(no_mangle)]
static EXTERN_BLOCK_TEST_MIXED_STATIC: u64 = 0xCAFEBABE_DEADBEEF;

#[unsafe(no_mangle)]
pub extern "C" fn extern_block_test_mixed_fn() -> u32 {
    42
}

#[unsafe(no_mangle)]
static MANGLED_NAME_SYMBOL: u32 = 7;

#[unsafe(no_mangle)]
static EXTERN_BLOCK_TEST_DUP_A: u8 = 1;

#[unsafe(no_mangle)]
static EXTERN_BLOCK_TEST_DUP_B: u8 = 2;

// =============================================================================
// Test 1: static-only form emits accessor that resolves to the right value.
// =============================================================================

slopos_ostd::extern_block! {
    mod static_only {
        static EXTERN_BLOCK_TEST_BYTE: u8;
    }
}

#[test]
fn static_symbol_form_compiles_and_accessor_returns_non_null() {
    let addr = static_only::EXTERN_BLOCK_TEST_BYTE_addr();
    assert!(!addr.is_null(), "accessor must return non-null");
    // SAFETY: the backing static is defined in this file and has
    // static-lifetime address; reading its byte through the macro-
    // emitted accessor is sound.
    let val = unsafe { *addr };
    assert_eq!(val, 0xAB, "accessor must point at the backing static");
}

// =============================================================================
// Test 2: fn-only form consolidates the declaration; caller uses unsafe.
// =============================================================================

slopos_ostd::extern_block! {
    mod fn_only {
        fn extern_block_test_inc(x: u32) -> u32;
    }
}

#[test]
fn function_form_compiles_no_safe_wrapper() {
    // The macro emits no safe wrapper for `fn` items — caller wraps
    // the call site in `unsafe { ... }`. The `unsafe extern` syntax
    // itself lives only inside the macro expansion (interior to OSTD).
    // SAFETY: backing fn is a pure-arith integer add defined at the
    // top of this file.
    let result = unsafe { fn_only::extern_block_test_inc(41) };
    assert_eq!(result, 42);
}

// =============================================================================
// Test 3: mixed block with both statics and fns.
// =============================================================================

slopos_ostd::extern_block! {
    mod mixed {
        static EXTERN_BLOCK_TEST_MIXED_STATIC: u64;
        fn extern_block_test_mixed_fn() -> u32;
    }
}

#[test]
fn mixed_block_with_statics_and_fns() {
    let addr = mixed::EXTERN_BLOCK_TEST_MIXED_STATIC_addr();
    // SAFETY: backing static defined above.
    let val = unsafe { *addr };
    assert_eq!(val, 0xCAFEBABE_DEADBEEF);

    // SAFETY: backing fn returns a constant.
    let result = unsafe { mixed::extern_block_test_mixed_fn() };
    assert_eq!(result, 42);
}

// =============================================================================
// Test 4: `#[link_name = "…"]` attribute preserved on static.
// =============================================================================

slopos_ostd::extern_block! {
    mod link_name_alias {
        #[link_name = "MANGLED_NAME_SYMBOL"]
        static LOCAL_ALIAS: u32;
    }
}

#[test]
fn link_name_attr_preserved() {
    let addr = link_name_alias::LOCAL_ALIAS_addr();
    // SAFETY: backing static `MANGLED_NAME_SYMBOL` defined above; the
    // local alias resolves to it via the preserved `#[link_name]`.
    let val = unsafe { *addr };
    assert_eq!(val, 7);
}

// =============================================================================
// Test 5: outer `#[allow(...)]` on the mod is accepted (compile smoke).
// =============================================================================

slopos_ostd::extern_block! {
    #[allow(non_camel_case_types)]
    mod with_outer_attr {
        static EXTERN_BLOCK_TEST_BYTE: u8;
    }
}

#[test]
fn outer_attr_on_mod_survives() {
    // We can't easily verify the attribute is actively *applied* — only
    // that the macro accepts it and compiles cleanly. If the macro
    // ate the attribute or rejected it, this file would not compile.
    let addr = with_outer_attr::EXTERN_BLOCK_TEST_BYTE_addr();
    assert!(!addr.is_null());
}

// =============================================================================
// Test 6: multiple invocations in the same scope don't collide.
// =============================================================================

slopos_ostd::extern_block! {
    mod scope_a {
        static EXTERN_BLOCK_TEST_DUP_A: u8;
    }
}

slopos_ostd::extern_block! {
    mod scope_b {
        static EXTERN_BLOCK_TEST_DUP_B: u8;
    }
}

#[test]
fn multiple_extern_blocks_in_same_scope_dont_collide() {
    let a = scope_a::EXTERN_BLOCK_TEST_DUP_A_addr();
    let b = scope_b::EXTERN_BLOCK_TEST_DUP_B_addr();
    // SAFETY: both backing statics defined above.
    assert_eq!(unsafe { *a }, 1);
    assert_eq!(unsafe { *b }, 2);
}
