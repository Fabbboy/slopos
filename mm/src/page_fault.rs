use slopos_abi::task::TaskFaultReason;
use slopos_ostd::handle::HandleError;
use slopos_ostd::{klog_info, klog_warn};

use crate::error::MmError;
use crate::{cow, demand, process_vm};

/// What became of a user page fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultOutcome {
    /// Serviced; the faulting instruction can be retried.
    Resolved,
    /// Exclusive access was unavailable; nothing changed and the instruction re-faults.
    Retry,
    /// Not serviceable; the task dies with this reason, which `waitpid` needs
    /// to tell an out-of-memory kill from a wild dereference.
    Fatal(TaskFaultReason),
}

pub(crate) const RETRY_WARN_MS: u64 = 50;

#[derive(Clone, Copy)]
pub(crate) struct RetryEpisode {
    task_id: u32,
    fault_addr: u64,
    since_ms: u64,
    warned: bool,
}

impl RetryEpisode {
    pub(crate) const IDLE: Self = Self {
        task_id: 0,
        fault_addr: 0,
        since_ms: 0,
        warned: false,
    };
}

slopos_ostd::cpu_local! {
    static RETRY_EPISODE: RetryEpisode = RetryEpisode::IDLE;
}

/// Keyed on the task, not the address: alternating addresses would reset the episode.
pub(crate) fn note_retry(
    ep: &mut RetryEpisode,
    task_id: u32,
    fault_addr: u64,
    now_ms: u64,
) -> bool {
    if ep.task_id != task_id {
        *ep = RetryEpisode {
            task_id,
            fault_addr,
            since_ms: now_ms,
            warned: false,
        };
        return false;
    }
    if ep.warned || now_ms.wrapping_sub(ep.since_ms) < RETRY_WARN_MS {
        return false;
    }
    ep.fault_addr = fault_addr;
    ep.warned = true;
    true
}

/// No escalation: any threshold here is user-reachable, and a stuck retry is a defect.
fn retry(task_id: u32, fault_addr: u64) -> FaultOutcome {
    let warn = {
        let mut episode = RETRY_EPISODE.get_mut();
        // The clock read is uncached MMIO taken with `IF` clear; skip it once warned.
        if episode.task_id == task_id && episode.warned {
            false
        } else {
            let now = slopos_kernel_services::clock::uptime_ms();
            note_retry(&mut episode, task_id, fault_addr, now)
        }
    };
    if warn {
        klog_warn!(
            "PF: task {} has been retrying at cr2=0x{:x} for {} ms — an address-space \
             reader is not draining",
            task_id,
            fault_addr,
            RETRY_WARN_MS
        );
    }
    FaultOutcome::Retry
}

/// Try to service a user page fault in the address space named by
/// `process_vm_handle`, returning whether it was resolved.
///
/// Keyed by handle rather than by process id because ids recycle: a handle
/// names the address space the faulting task was built against and fails to
/// resolve once that slot is rebound, where a recycled id would service the
/// fault inside a stranger's page tables.
pub fn try_resolve_user_fault(
    fault_addr: u64,
    error_code: u64,
    process_vm_handle: u64,
    task_id: u32,
) -> FaultOutcome {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return FaultOutcome::Fatal(TaskFaultReason::UserPage);
    };

    // One hold of the per-process lock, not two: a sibling can resolve the page between.
    let cow = process_vm::process_vm_with_vm_space_by_handle(handle, |vs| {
        if !cow::is_cow_fault(error_code, vs, fault_addr) {
            return None;
        }
        Some(cow::handle_cow_fault(vs, fault_addr))
    });

    match cow {
        Ok(Some(Ok(()))) => return FaultOutcome::Resolved,
        Ok(Some(Err(MmError::Retry))) => return retry(task_id, fault_addr),
        Ok(Some(Err(MmError::NoMemory))) => {
            klog_info!(
                "PF: COW copy for task {} at cr2=0x{:x} found no memory",
                task_id,
                fault_addr
            );
            return FaultOutcome::Fatal(TaskFaultReason::UserOom);
        }
        Ok(Some(Err(_))) => {
            klog_info!(
                "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
                task_id,
                fault_addr
            );
        }
        Ok(None) => {}
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            return FaultOutcome::Fatal(TaskFaultReason::UserPage);
        }
    }

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
        Ok(Some(Err(MmError::Retry))) => retry(task_id, fault_addr),
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

/// `HandleError::NoEntry` is the ordinary race a dying task loses; only
/// `Stale` is worth naming — a fault arriving for a task whose slot now
/// belongs to another process.
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
