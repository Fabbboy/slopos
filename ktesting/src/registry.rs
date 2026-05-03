//! Per-test registry walked by the harness.
//!
//! Every `stest!` invocation emits a static `TestDesc` into the
//! `.test_registry` linker section. The harness reads the section between the
//! linker-provided symbols `__start_test_registry` and `__stop_test_registry`,
//! sorts by `(module_path, name)`, then runs each entry.

use core::cmp::Ordering;

use slopos_ostd::{AllocError, KVec};

use crate::result::TestResult;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestKind {
    Kernel = 0,
    Userland = 1,
}

/// Compile-time descriptor for one test entry.
///
/// Stored in the `.test_registry` linker section. The userland-test fields
/// (`bin`, `argv`) are populated by Phase 3's `utest!` macro and ignored by
/// kernel tests.
#[repr(C)]
pub struct TestDesc {
    pub name: &'static str,
    pub module: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub run: fn() -> TestResult,
    pub kind: TestKind,
    pub flags: u32,
    pub bin: Option<&'static str>,
    pub argv: &'static [&'static str],
}

// SAFETY: All fields are immutable `'static` references and function
// pointers; safe for read-only access from any CPU.
unsafe impl Sync for TestDesc {}

/// `flags` bit: panic from this test should be reported as Pass with the
/// `EXPECTED_PANIC` suffix. Used by the bootstrap panic-isolation canary.
pub const FLAG_EXPECTED_PANIC: u32 = 0x1;

#[allow(improper_ctypes)]
unsafe extern "C" {
    static __start_test_registry: TestDesc;
    static __stop_test_registry: TestDesc;
}

/// Walk every entry in `.test_registry`.
pub fn registry_iter() -> impl Iterator<Item = &'static TestDesc> {
    let start: *const TestDesc = unsafe { &__start_test_registry };
    let end: *const TestDesc = unsafe { &__stop_test_registry };
    let count = if end >= start {
        // SAFETY: section contains a contiguous run of `TestDesc` entries.
        unsafe { end.offset_from(start) as usize }
    } else {
        0
    };
    (0..count).map(move |i| {
        // SAFETY: `i < count` and the section is well-formed.
        unsafe { &*start.add(i) }
    })
}

fn is_bootstrap(desc: &TestDesc) -> bool {
    desc.name.starts_with("bootstrap_")
}

fn cmp_desc(a: &&TestDesc, b: &&TestDesc) -> Ordering {
    // Bootstrap framework self-tests run first regardless of module path,
    // so a `bootstrap_*` failure aborts the run before any subsystem test
    // wastes time on broken plumbing.
    let a_boot = is_bootstrap(a);
    let b_boot = is_bootstrap(b);
    if a_boot != b_boot {
        return if a_boot {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    match a.module.as_bytes().cmp(b.module.as_bytes()) {
        Ordering::Equal => a.name.as_bytes().cmp(b.name.as_bytes()),
        other => other,
    }
}

/// Collect every registry entry into a heap-backed vector and sort by
/// `(module, name)` byte order. Insertion sort to keep the implementation
/// alloc-light and avoid pulling generic slice-sort into the kernel.
pub fn registry_sorted() -> Result<KVec<&'static TestDesc>, AllocError> {
    let mut out: KVec<&'static TestDesc> = KVec::new();
    for desc in registry_iter() {
        out.push(desc)?;
    }
    let slice: &mut [&'static TestDesc] = &mut out;
    let len = slice.len();
    let mut i = 1usize;
    while i < len {
        let mut j = i;
        while j > 0 && cmp_desc(&slice[j], &slice[j - 1]) == Ordering::Less {
            slice.swap(j, j - 1);
            j -= 1;
        }
        i += 1;
    }
    Ok(out)
}
