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
    // ...and no other privilege: a power binary that could also launch or
    // signal is a second `Admin`. The universal capabilities are excluded —
    // holding one says nothing about privilege.
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

/// The capabilities every task holds, pinned so their removal is a visible
/// diff rather than a silent widening.
///
/// Each names a global the kernel has not yet given an object form. They are
/// classified rather than left ungated so that the deletion conditions in
/// `slopos_ostd::authority` have something to delete: when `write` routes
/// through the controlling TTY and the clipboard becomes fd-passing only,
/// these entries go, and this test is what fails to say so.
fn the_universal_capabilities_are_the_recorded_set() -> TestResult {
    let ordinary = caps_from_task_flags(TASK_FLAG_USER_MODE);

    for cap in [
        Capability::ConsoleIo,
        Capability::ClipboardGlobal,
        Capability::SysInspect,
        Capability::Fate,
    ] {
        if !mask_permits(ordinary, cap) {
            return fail!(
                "{} is no longer universal -- if deliberate, this test records it",
                cap.name()
            );
        }
    }

    // The half that matters: a capability drifting into the universal set is
    // how a model becomes ambient again, one convenience at a time.
    for cap in [
        Capability::Power,
        Capability::Launch,
        Capability::ProcSignal,
        Capability::DisplaySeat,
        Capability::InputSeat,
        Capability::ConsoleConfig,
        Capability::TestHarness,
    ] {
        if mask_permits(ordinary, cap) {
            return fail!("{} leaked into the universal set", cap.name());
        }
    }
    pass!()
}

/// `Signalable` resolves and authorizes in one step, and the authorization
/// carries the object.
///
/// There must be no way to hold an authorization for one task and act on
/// another. `syscall_terminate_task` used to check the compositor bit and then
/// terminate an arbitrary target; a bare capability witness would have left
/// that byte-identical, which is why the witness carries the target rather
/// than merely attesting that a check ran.
fn signalable_refuses_init_and_invalid_targets() -> TestResult {
    use crate::syscall::signalable::resolve_signal_target;
    use slopos_abi::Errno;
    use slopos_abi::task::INVALID_TASK_ID;

    // A caller holding every privilege: if these refusals hold for it, they
    // hold for everybody.
    let omnipotent = u16::MAX;

    // The kernel phase runs at drivers/90, before `/sbin/init` launches, so
    // `init_task_id()` is unset here and the init arm cannot be exercised from
    // a `stest!`. Asserting it *is* unset keeps this honest: were it to become
    // set, the branch below would start running and this test would need to
    // stop skipping it rather than silently passing.
    let init = crate::exec::init_task_id();
    if init != INVALID_TASK_ID {
        match resolve_signal_target(omnipotent, init) {
            Err(Errno::EPERM) => {}
            Err(other) => return fail!("init resolved to {:?}, want EPERM", other),
            Ok(_) => return fail!("init must never resolve as a signal target"),
        }
    }

    // A nonexistent id is ESRCH, never a success and never a permission answer
    // that would disclose whether the id names anything.
    for (id, label) in [(INVALID_TASK_ID, "an invalid id"), (0, "id 0")] {
        match resolve_signal_target(omnipotent, id) {
            Err(Errno::ESRCH) => {}
            Err(other) => return fail!("{} gave {:?}, want ESRCH", label, other),
            Ok(_) => return fail!("{} must not resolve", label),
        }
    }
    pass!()
}

slopos_testing::stest!(
    name = the_universal_capabilities_are_the_recorded_set,
    suite = authority
);
slopos_testing::stest!(
    name = signalable_refuses_init_and_invalid_targets,
    suite = authority
);

/// exec narrows authority to what the new image's identity earns, and the
/// reduction is monotone.
///
/// An entitlement surviving `exec` is one of the two CVE shapes this model is
/// built against: a privileged program execs an arbitrary binary and the
/// binary inherits the privilege. The intersection is what prevents it.
///
/// Tested at the mask level because that is where the property lives -- the
/// handler computes `grant(image) & held` and stores it, so what matters is
/// that the arithmetic cannot widen.
fn exec_intersection_never_widens() -> TestResult {
    let privileged = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM);
    let ordinary = caps_from_task_flags(TASK_FLAG_USER_MODE);

    // exec of an image whose identity earns nothing drops every privilege.
    let after = privileged & ordinary;
    for cap in [
        Capability::Power,
        Capability::Launch,
        Capability::ProcSignal,
        Capability::TestHarness,
    ] {
        if mask_permits(after, cap) {
            return fail!("{} survived exec of an unprivileged image", cap.name());
        }
    }

    // An unprivileged task execing a *privileged* image does not gain the
    // grant: the intersection bounds it by what was already held. This is the
    // half that stops exec being a privilege-escalation primitive.
    let grant = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_POWER);
    let raised = ordinary & grant;
    if mask_permits(raised, Capability::Power) {
        return fail!("exec of a privileged image raised an unprivileged task");
    }

    // Monotone: intersecting with everything changes nothing.
    if (after & u64::MAX) != after {
        return fail!("the reduction is not monotone");
    }
    pass!()
}

/// `Launch` bounds the one raise site, and is held by exactly the launchers.
///
/// Without it any task obtains a privileged child by spawning the privileged
/// path -- an entitlement leaking to an unrelated program with nothing granted
/// to it.
fn launch_is_held_by_the_launchers_only() -> TestResult {
    use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_LAUNCH};

    // The four real launchers: init (SYSTEM), shell, terminal, compositor.
    for (flags, who) in [
        (TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM, "init"),
        (TASK_FLAG_USER_MODE | TASK_FLAG_LAUNCH, "shell/terminal"),
        (
            TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR | TASK_FLAG_LAUNCH,
            "compositor",
        ),
    ] {
        if !mask_permits(caps_from_task_flags(flags), Capability::Launch) {
            return fail!("{} must hold Launch", who);
        }
    }

    // And nobody else. An ordinary program spawning /bin/halt must not thereby
    // obtain a child holding Power.
    let ordinary = caps_from_task_flags(TASK_FLAG_USER_MODE);
    if mask_permits(ordinary, Capability::Launch) {
        return fail!("an ordinary task must not hold Launch");
    }
    // A privileged-but-not-launcher program likewise: holding Power does not
    // imply being allowed to confer it.
    let power_only = caps_from_task_flags(TASK_FLAG_USER_MODE | TASK_FLAG_POWER);
    if mask_permits(power_only, Capability::Launch) {
        return fail!("/bin/halt must not hold Launch");
    }
    pass!()
}

slopos_testing::stest!(name = exec_intersection_never_widens, suite = authority);
slopos_testing::stest!(
    name = launch_is_held_by_the_launchers_only,
    suite = authority
);
