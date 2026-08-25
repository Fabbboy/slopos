//! `exec` is an address-space boundary.
//!
//! Replacing the program image must sever every mapping the old image made.
//! The implementation this replaced unmapped exactly the code window, so the
//! heap, the mmap arena, shared memfd mappings and SlopRing mappings all
//! survived into a program that never mapped them.

use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use super::tests::resolve_pid;
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_get_vm_space, process_vm_mmap,
    process_vm_read_user_bytes, process_vm_reset_for_exec, process_vm_write_user_bytes,
};

const MAP_FLAGS: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PROT_RW: u64 = PROT_READ | PROT_WRITE;
const MARKER: &[u8] = b"pre-exec-image";

/// Return every frame the address spaces below borrowed, so the suite does not
/// carry a growing quarantine from here to whatever runs next.
fn settle_teardown() {
    // Teardown stages a frame through three holds before it is reusable: the
    // quiesce epoch has to close, then the quarantine rotates, then the
    // per-CPU cache drains.
    let _ = crate::mmu::quiesce::force_close_epoch_for_test();
    for _ in 0..4 {
        crate::page_alloc::quarantine_rotate();
        while crate::page_alloc::quarantine_has_releasable() {
            if crate::page_alloc::quarantine_release_some(u32::MAX) == 0 {
                break;
            }
        }
    }
    crate::page_alloc::pcp_drain_all();
}

/// A page mapped by the old image, carrying a known pattern, must not be
/// readable at the same address after exec.
pub fn test_exec_severs_previous_image_mappings() -> TestResult {
    let pid = create_process_vm();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return fail!("could not create an address space");
    }
    let process = resolve_pid(pid);

    // The stack is mapped eagerly at creation, so a write lands without a
    // fault-in; a lazy mmap has no frame yet and would prove nothing.
    let stack_top = crate::process_vm::process_vm_get_stack_top(process);
    if stack_top == 0 {
        destroy_process_vm(process);
        settle_teardown();
        return fail!("no stack for a fresh address space");
    }
    let addr = stack_top - 64;

    let wrote = {
        let Some(vm_space) = process_vm_get_vm_space(process) else {
            destroy_process_vm(process);
            settle_teardown();
            return fail!("no vm_space for a live process");
        };
        process_vm_write_user_bytes(&vm_space, addr, MARKER).is_ok()
    };
    if !wrote {
        destroy_process_vm(process);
        settle_teardown();
        return fail!("could not seed the marker");
    }

    // Prove the marker is there before exec, so a post-exec absence means the
    // reset removed it rather than the write never landing.
    let seen_before = {
        let Some(vm_space) = process_vm_get_vm_space(process) else {
            destroy_process_vm(process);
            settle_teardown();
            return fail!("no vm_space before exec");
        };
        let mut buf = [0u8; MARKER.len()];
        process_vm_read_user_bytes(&vm_space, addr, &mut buf).is_ok() && buf == MARKER
    };

    let rc = process_vm_reset_for_exec(process);

    let seen_after = {
        match process_vm_get_vm_space(process) {
            Some(vm_space) => {
                let mut buf = [0u8; MARKER.len()];
                process_vm_read_user_bytes(&vm_space, addr, &mut buf).is_ok() && buf == MARKER
            }
            None => false,
        }
    };

    destroy_process_vm(process);
    settle_teardown();

    assert_test!(seen_before, "the marker was not readable before exec");
    assert_test!(rc == 0, "process_vm_reset_for_exec failed: {}", rc);
    assert_test!(
        !seen_after,
        "an mmap made by the previous image survived exec at {:#x}",
        addr
    );
    pass!()
}

/// The reset must leave an address space a new program can be loaded into: the
/// three initial VMAs describing the fresh layout, and a heap wound back to
/// its start.
///
/// The stack's *pages* are deliberately not mapped here — `do_exec` calls
/// `process_vm_reset_stack` straight after the load, which unmaps and remaps
/// that whole extent, so mapping it twice would charge the caller 256 pages
/// for the window between them. The VMA is what the loader needs; the pages
/// are the next step's job.
pub fn test_exec_reset_reseeds_a_usable_layout() -> TestResult {
    let pid = create_process_vm();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return fail!("could not create an address space");
    }
    let process = resolve_pid(pid);

    let rc = process_vm_reset_for_exec(process);
    if rc != 0 {
        destroy_process_vm(process);
        settle_teardown();
        return fail!("process_vm_reset_for_exec failed: {}", rc);
    }

    let stack_top = crate::process_vm::process_vm_get_stack_top(process);

    // `reset_stack` is what `do_exec` runs next; after it the stack is backed
    // and writable, which is the property the new image actually depends on.
    let stack_rc = crate::process_vm::process_vm_reset_stack(process);
    let stack_ok = stack_top != 0 && stack_rc == 0 && {
        let Some(vm_space) = process_vm_get_vm_space(process) else {
            destroy_process_vm(process);
            settle_teardown();
            return fail!("no vm_space after reset");
        };
        let ok = process_vm_write_user_bytes(&vm_space, stack_top - 16, b"ok").is_ok();
        drop(vm_space);
        ok
    };

    let rc2 = process_vm_reset_for_exec(process);

    destroy_process_vm(process);
    settle_teardown();

    assert_test!(stack_rc == 0, "process_vm_reset_stack failed: {}", stack_rc);
    assert_test!(
        stack_ok,
        "the stack is not writable after reset + reset_stack"
    );
    assert_test!(rc2 == 0, "a second reset failed: {}", rc2);
    pass!()
}

/// The account row must still agree with the summed maps after a reset.
///
/// A phantom debit — a page unmapped but never refunded — is invisible in the
/// row alone, which is why this reconciles the two rather than comparing a
/// number against itself. Absolute counts cannot be asserted: the re-seed
/// re-randomises the layout, so region sizes legitimately differ across it.
pub fn test_exec_reset_leaves_the_page_ledger_consistent() -> TestResult {
    use slopos_ostd::process::quota::{LedgerFault, ledger_audit};

    let pid = create_process_vm();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return fail!("could not create an address space");
    }
    let process = resolve_pid(pid);

    let addr = process_vm_mmap(process, 0, 64 * PAGE_SIZE_4KB, PROT_RW, MAP_FLAGS, -1, 0);
    let rc = process_vm_reset_for_exec(process);

    let mut mismatches = 0usize;
    ledger_audit(|fault| {
        if matches!(fault, LedgerFault::PagesMismatch { .. }) {
            mismatches += 1;
        }
    });

    // The mapping itself must be gone from the map, which is the other half of
    // what the audit cannot see on its own.
    let still_mapped = crate::process_vm::process_vm_user_va_to_paddr(process, addr) != 0;

    destroy_process_vm(process);
    settle_teardown();

    assert_test!(addr != 0, "mmap failed");
    assert_test!(rc == 0, "reset failed: {}", rc);
    assert_test!(
        mismatches == 0,
        "exec left {} page-ledger mismatch(es)",
        mismatches
    );
    assert_test!(
        !still_mapped,
        "the previous image's mmap is still mapped at {:#x}",
        addr
    );
    pass!()
}

slopos_testing::stest!(
    name = test_exec_severs_previous_image_mappings,
    suite = exec_boundary
);
slopos_testing::stest!(
    name = test_exec_reset_reseeds_a_usable_layout,
    suite = exec_boundary
);
slopos_testing::stest!(
    name = test_exec_reset_leaves_the_page_ledger_consistent,
    suite = exec_boundary
);
