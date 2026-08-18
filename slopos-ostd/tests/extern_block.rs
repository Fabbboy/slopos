//! Host-side tests for `slopos_ostd::ffi::extern_block!`.
//!
//! The macro emits a safe `<name>_addr() -> *const <ty>` accessor for each
//! `static` item; `fn` items get no safe wrapper. Each test pairs a backing
//! `#[unsafe(no_mangle)]` definition with an invocation importing it by name.

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

slopos_ostd::extern_block! {
    mod static_only {
        static EXTERN_BLOCK_TEST_BYTE: u8;
    }
}

// Miri's interpreter does not resolve `unsafe extern static` symbols.
#[cfg_attr(miri, ignore)]
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

slopos_ostd::extern_block! {
    mod fn_only {
        fn extern_block_test_inc(x: u32) -> u32;
    }
}

#[test]
fn function_form_compiles_no_safe_wrapper() {
    // SAFETY: backing fn is a pure-arith integer add defined at the
    // top of this file.
    let result = unsafe { fn_only::extern_block_test_inc(41) };
    assert_eq!(result, 42);
}

slopos_ostd::extern_block! {
    mod mixed {
        static EXTERN_BLOCK_TEST_MIXED_STATIC: u64;
        fn extern_block_test_mixed_fn() -> u32;
    }
}

#[cfg_attr(miri, ignore)]
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

slopos_ostd::extern_block! {
    mod link_name_alias {
        #[link_name = "MANGLED_NAME_SYMBOL"]
        static LOCAL_ALIAS: u32;
    }
}

// Miri keys its symbol table on Rust names, so `#[link_name]` does not
// resolve there.
#[cfg_attr(miri, ignore)]
#[test]
fn link_name_attr_preserved() {
    let addr = link_name_alias::LOCAL_ALIAS_addr();
    // SAFETY: backing static `MANGLED_NAME_SYMBOL` defined above; the
    // local alias resolves to it via the preserved `#[link_name]`.
    let val = unsafe { *addr };
    assert_eq!(val, 7);
}

slopos_ostd::extern_block! {
    #[allow(non_camel_case_types)]
    mod with_outer_attr {
        static EXTERN_BLOCK_TEST_BYTE: u8;
    }
}

#[cfg_attr(miri, ignore)]
#[test]
fn outer_attr_on_mod_survives() {
    // Compile smoke: a macro that ate or rejected the attribute would not
    // compile this file. Whether the attribute is applied is not checked.
    let addr = with_outer_attr::EXTERN_BLOCK_TEST_BYTE_addr();
    assert!(!addr.is_null());
}

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

#[cfg_attr(miri, ignore)]
#[test]
fn multiple_extern_blocks_in_same_scope_dont_collide() {
    let a = scope_a::EXTERN_BLOCK_TEST_DUP_A_addr();
    let b = scope_b::EXTERN_BLOCK_TEST_DUP_B_addr();
    // SAFETY: both backing statics defined above.
    assert_eq!(unsafe { *a }, 1);
    assert_eq!(unsafe { *b }, 2);
}
