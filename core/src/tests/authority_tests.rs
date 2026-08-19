//! The authority model's load-bearing claims, checked in the kernel.
//!
//! These test the *decision*, not the effect: a test that actually called
//! `halt` would power the machine off mid-run. What matters is that the mask
//! derivation and the table classification say the right things, because the
//! dispatcher's behaviour is a total function of those two.

use slopos_abi::syscall::{
    SYSCALL_HALT, SYSCALL_REBOOT, SYSCALL_ROULETTE_RESULT, SYSCALL_TABLE_SIZE,
};
use slopos_abi::task::{
    TASK_FLAG_COMPOSITOR, TASK_FLAG_POWER, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE,
};
use slopos_ostd::authority::{
    AuthorityDecision, CAP_NONE, Capability, caps_from_task_flags, decide, mask_permits,
};
use slopos_testing::{TestResult, fail, pass};

use crate::syscall::handlers::{cap_counts, syscall_lookup};

/// The finding this phase closes: `halt` and `reboot` had no authorization
/// check at all, so one instruction from any task powered off the machine.
fn power_is_denied_to_an_ordinary_task() -> TestResult {
    let ordinary = caps_from_task_flags(TASK_FLAG_USER_MODE);
    if mask_permits(ordinary, Capability::Power) {
        return fail!("an ordinary user task must not hold Power");
    }
    if decide(ordinary, Capability::Power) != AuthorityDecision::Deny {
        return fail!("Power must be denied under the default enforce mode");
    }

    // The compositor holds display authority and no more: a task privileged
    // for one thing must not be privileged for everything, which is the
    // failure mode a single `Admin` capability produces.
    let compositor = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR);
    if mask_permits(compositor, Capability::Power) {
        return fail!("the compositor must not hold Power");
    }
    if !mask_permits(compositor, Capability::DisplaySeat) {
        return fail!("the compositor must hold DisplaySeat");
    }
    pass!()
}

/// `/bin/halt` is the one program conferred `Power`, and init keeps it so the
/// machine can still be brought down by the system itself.
fn power_is_granted_by_program_identity() -> TestResult {
    let halt_binary = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_POWER);
    if !mask_permits(halt_binary, Capability::Power) {
        return fail!("/bin/halt's grant must confer Power");
    }
    // ...and no *other privilege*. A power binary that could also launch,
    // signal or reconfigure the console would be a second `Admin` by another
    // name, which is the failure this whole model exists to avoid.
    //
    // The capabilities every task holds (`ConsoleIo`, `ClipboardGlobal`,
    // `SysInspect`, `Fate`) are deliberately excluded: they name globals with
    // no object form yet, so holding one says nothing about privilege. Each
    // carries a deletion condition, and this list shrinks as they land.
    for other in [
        Capability::Launch,
        Capability::ProcSignal,
        Capability::ConsoleConfig,
        Capability::DisplaySeat,
        Capability::InputSeat,
        Capability::TestHarness,
    ] {
        if mask_permits(halt_binary, other) {
            return fail!("/bin/halt's grant leaked {}", other.name());
        }
    }

    let init = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM);
    if !mask_permits(init, Capability::Power) {
        return fail!("init must retain Power");
    }
    pass!()
}

/// The classification reaches the dispatch table through the handler, so the
/// slot a caller invokes and the capability the dispatcher tests are one
/// artifact. If these drift, every other claim in the model is void.
fn the_table_classifies_power() -> TestResult {
    for (sysno, name) in [(SYSCALL_HALT, "halt"), (SYSCALL_REBOOT, "reboot")] {
        let Some(entry) = syscall_lookup(sysno) else {
            return fail!("{} is not registered", name);
        };
        if entry.cap != Capability::Power {
            return fail!("{} is classified {}, want Power", name, entry.cap.name());
        }
    }

    // The reachability case: `roulette_result` reaches the reboot primitive on
    // its loss arm, so a slot-level gate alone would leave it green. It is
    // classified `Fate` *and* two-keyed on a boot flag.
    let Some(entry) = syscall_lookup(SYSCALL_ROULETTE_RESULT) else {
        return fail!("roulette_result is not registered");
    };
    if entry.cap != Capability::Fate {
        return fail!(
            "roulette_result is classified {}, want Fate",
            entry.cap.name()
        );
    }
    pass!()
}

/// Totality, checked at runtime as well as by the const assert: every slot
/// carries a classification, and a registered slot is never `Unimplemented`.
fn every_slot_is_classified() -> TestResult {
    let mut registered = 0usize;
    for sysno in 0..SYSCALL_TABLE_SIZE as u64 {
        if let Some(entry) = syscall_lookup(sysno) {
            registered += 1;
            if entry.cap == Capability::Unimplemented {
                return fail!("syscall {} is registered but unclassified", sysno);
            }
        }
    }
    if registered == 0 {
        return fail!("no syscall resolved -- the lookup is broken, not the table");
    }

    // The histogram sums to the table size; a drift here means the const
    // assert and the live table disagree, which should be impossible.
    let total: usize = cap_counts().iter().map(|(_, n)| *n).sum();
    if total != SYSCALL_TABLE_SIZE {
        return fail!(
            "the recorded distribution sums to {}, want {}",
            total,
            SYSCALL_TABLE_SIZE
        );
    }
    pass!()
}

/// An ungated operation is permitted by the empty mask. Getting this backwards
/// denies every unprivileged syscall in the system, so it is worth an explicit
/// test rather than resting on the enum's shape.
fn ungated_operations_need_nothing() -> TestResult {
    for cap in [
        Capability::NoneSelf,
        Capability::NoneFd,
        Capability::NoneRelation,
    ] {
        if !mask_permits(CAP_NONE, cap) {
            return fail!("{} must be permitted with no capabilities", cap.name());
        }
        if decide(CAP_NONE, cap) != AuthorityDecision::Allow {
            return fail!("{} must be allowed with no capabilities", cap.name());
        }
    }
    pass!()
}

slopos_testing::stest!(
    name = power_is_denied_to_an_ordinary_task,
    suite = authority
);
slopos_testing::stest!(
    name = power_is_granted_by_program_identity,
    suite = authority
);
slopos_testing::stest!(name = the_table_classifies_power, suite = authority);
slopos_testing::stest!(name = every_slot_is_classified, suite = authority);
slopos_testing::stest!(name = ungated_operations_need_nothing, suite = authority);
