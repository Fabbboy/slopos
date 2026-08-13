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

/// `flags` bit: panic from this test should be reported as Pass with the
/// `EXPECTED_PANIC` suffix. Used by the bootstrap panic-isolation canary.
pub const FLAG_EXPECTED_PANIC: u32 = 0x1;

impl slopos_ostd::ffi::registry::RegistryEntry for TestDesc {
    const REGISTRIES: &'static [slopos_ostd::ffi::registry::RegistryId] =
        &[slopos_ostd::ffi::registry::RegistryId::Tests];
}

/// Walk every entry in `.test_registry`.
pub fn registry_iter() -> impl Iterator<Item = &'static TestDesc> {
    slopos_ostd::ffi::registry::registry_slice::<TestDesc>(
        slopos_ostd::ffi::registry::RegistryId::Tests,
    )
    .iter()
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

/// One bottom-up merge pass: merge each adjacent pair of `width`-sized sorted
/// runs from `src` into `dst`. Stable — ties resolve to the left run.
fn merge_pass(src: &[&'static TestDesc], dst: &mut [&'static TestDesc], width: usize) {
    let len = src.len();
    let mut start = 0usize;
    while start < len {
        let mid = core::cmp::min(start + width, len);
        let end = core::cmp::min(start + 2 * width, len);
        let (mut i, mut j) = (start, mid);
        let mut k = start;
        while k < end {
            let take_left = if i >= mid {
                false
            } else if j >= end {
                true
            } else {
                cmp_desc(&src[j], &src[i]) != Ordering::Less
            };
            if take_left {
                dst[k] = src[i];
                i += 1;
            } else {
                dst[k] = src[j];
                j += 1;
            }
            k += 1;
        }
        start = end;
    }
}

/// Collect every registry entry into a heap-backed vector and sort by
/// `(module, name)` byte order. Bottom-up merge sort over a second buffer of
/// pointers: the registry holds thousands of entries and an O(n^2) sort here
/// costs more wall-clock than the test run it is ordering.
pub fn registry_sorted() -> Result<KVec<&'static TestDesc>, AllocError> {
    let mut src: KVec<&'static TestDesc> = KVec::new();
    for desc in registry_iter() {
        src.push(desc)?;
    }
    let len = src.len();
    if len < 2 {
        return Ok(src);
    }

    let mut dst: KVec<&'static TestDesc> = KVec::with_capacity(len)?;
    for &desc in src.iter() {
        dst.push(desc)?;
    }

    let mut width = 1usize;
    let mut sorted_in_src = true;
    while width < len {
        if sorted_in_src {
            merge_pass(&src, &mut dst, width);
        } else {
            merge_pass(&dst, &mut src, width);
        }
        sorted_in_src = !sorted_in_src;
        width *= 2;
    }

    Ok(if sorted_in_src { src } else { dst })
}
