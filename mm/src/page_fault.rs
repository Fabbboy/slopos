use slopos_abi::task::TaskFaultReason;
use slopos_ostd::handle::HandleError;
use slopos_ostd::klog_info;

use crate::error::MmError;
use crate::{cow, demand, process_vm};

/// What became of a user page fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultOutcome {
    /// Serviced; the faulting instruction can be retried.
    Resolved,
    /// Not serviceable. The task dies, and this is the reason it died — which
    /// `waitpid` needs, because "the machine is out of memory" and "the
    /// program dereferenced a wild pointer" are the same observation without
    /// it.
    Fatal(TaskFaultReason),
}

/// Try to service a user page fault in the address space named by
/// `process_vm_handle`, returning whether it was resolved.
///
/// Keyed by handle rather than by process id because ids recycle. An id
/// says which process holds it *now*; a handle says which address space
/// the faulting task was built against, and fails to resolve once that
/// slot has been rebound. Servicing a COW fault against a recycled id
/// would copy a page inside a stranger's page tables — a `false` here
/// costs the faulting task its life, which is the correct trade.
pub fn try_resolve_user_fault(
    fault_addr: u64,
    error_code: u64,
    process_vm_handle: u64,
    task_id: u32,
) -> FaultOutcome {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return FaultOutcome::Fatal(TaskFaultReason::UserPage);
    };

    // Quick path: probe COW status WITHOUT taking the per-process lock
    // for write — the read-only `&KArc<VmSpace>` deref + cursor query
    // does the job. If the address isn't COW we fall through to the
    // demand-fault check.
    let is_cow = process_vm::process_vm_with_vm_space_by_handle(handle, |vs| {
        cow::is_cow_fault(error_code, vs, fault_addr)
    });

    match is_cow {
        Ok(true) => {
            let result = process_vm::process_vm_with_vm_space_by_handle(handle, |vs| {
                cow::handle_cow_fault(vs, fault_addr)
            });
            match result {
                Ok(Ok(())) => return FaultOutcome::Resolved,
                Ok(Err(MmError::NoMemory)) => {
                    klog_info!(
                        "PF: COW copy for task {} at cr2=0x{:x} found no memory",
                        task_id,
                        fault_addr
                    );
                    return FaultOutcome::Fatal(TaskFaultReason::UserOom);
                }
                Ok(Err(_)) | Err(_) => {
                    klog_info!(
                        "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
                        task_id,
                        fault_addr
                    );
                }
            }
        }
        Ok(false) => {}
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            return FaultOutcome::Fatal(TaskFaultReason::UserPage);
        }
    }

    // The demand path decides and acts under one lock acquisition: the
    // region that says whether this is a demand fault and the address
    // space that services it are read together.
    let demanded = process_vm::process_vm_with_vm_space_and_region_by_handle(
        handle,
        fault_addr,
        |vs, region| {
            if !demand::is_demand_fault_in_region(error_code, &region) {
                return None;
            }
            Some(demand::handle_demand_fault(
                vs, fault_addr, error_code, &region,
            ))
        },
    );

    match demanded {
        Ok(Some(Ok(()))) => FaultOutcome::Resolved,
        Ok(Some(Err(MmError::NoMemory))) => {
            klog_info!(
                "PF: demand fault for task {} at cr2=0x{:x} found no memory after reclaim",
                task_id,
                fault_addr
            );
            FaultOutcome::Fatal(TaskFaultReason::UserOom)
        }
        Ok(_) => FaultOutcome::Fatal(TaskFaultReason::UserPage),
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            FaultOutcome::Fatal(TaskFaultReason::UserPage)
        }
    }
}

/// `HandleError::NoEntry` is the ordinary "the address space is already
/// gone" race a dying task loses; `Stale` is the one worth naming,
/// because it is a fault arriving for a task whose slot now belongs to
/// another process.
fn report_unresolvable_address_space(err: HandleError, task_id: u32, fault_addr: u64) {
    if err == HandleError::Stale {
        klog_info!(
            "PF: task {} faulted at cr2=0x{:x} against an address space that has \
             been rebound to another process",
            task_id,
            fault_addr
        );
    }
}
