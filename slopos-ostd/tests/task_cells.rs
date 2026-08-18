//! Coverage for the witness-gated task cells, from outside the crate.
//!
//! The aliasing proofs live in `slopos-ostd/src/task/cell.rs`, because they
//! must call the production `pub(crate) TaskOwnCell::get_ptr`. What this file
//! covers is the surface `sched`/`core` can reach: the witness-taking methods
//! on `TaskInner`, the only way a `#![forbid(unsafe_code)]` crate writes
//! register-adjacent state after publication.
//!
//! `SwitchWindow` is the witness a host test can mint; `CurrentTask::get()` is
//! always `None` here because the PCR read short-circuits on `GS_BASE_SET`.

use slopos_ostd::KArc;
use slopos_ostd::task::HostStack;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{CurrentTask, SwitchWindow};

type HostTask = TaskInner<(), ()>;

fn fresh() -> KArc<HostTask> {
    KArc::try_new(HostTask::invalid()).expect("task allocation")
}

/// # Safety
/// Single-threaded host test: this "CPU" performs the switch, holds the only
/// reference to `task`, and the window cannot be re-entered.
fn window(task: &HostTask) -> SwitchWindow<'_, (), ()> {
    unsafe { SwitchWindow::new(task) }
}

/// Why a host test mints a `SwitchWindow` rather than a `CurrentTask`, and why
/// the dispatcher needs the former: it covers the outgoing task, no longer the
/// CPU's current by the time its registers are saved.
#[test]
fn current_task_is_none_without_a_pcr() {
    assert!(CurrentTask::<HostStack, HostStack>::get().is_none());
}

#[test]
fn cwd_round_trips_through_a_witness() {
    let task = fresh();
    let w = window(&task);

    assert!(task.set_cwd(&w, b"/usr/local/share"));
    task.with_cwd(&w, |bytes| {
        assert_eq!(bytes, b"/usr/local/share\0");
    });

    // A shorter path must republish the length, not leave the old tail visible.
    assert!(task.set_cwd(&w, b"/tmp"));
    task.with_cwd(&w, |bytes| {
        assert_eq!(bytes, b"/tmp\0");
    });
}

#[test]
fn an_oversized_cwd_is_refused_and_leaves_the_old_value() {
    let task = fresh();
    let w = window(&task);

    assert!(task.set_cwd(&w, b"/keep"));
    let too_long = [b'x'; 256];
    assert!(
        !task.set_cwd(&w, &too_long),
        "256 bytes leaves no room for NUL"
    );
    task.with_cwd(&w, |bytes| assert_eq!(bytes, b"/keep\0"));
}

/// A shared buffer would make a forked child's working directory track its
/// parent's.
#[test]
fn cells_are_per_task() {
    let first = fresh();
    let second = fresh();
    let w1 = window(&first);
    let w2 = window(&second);

    assert!(first.set_cwd(&w1, b"/first"));
    assert!(second.set_cwd(&w2, b"/second"));

    first.with_cwd(&w1, |b| assert_eq!(b, b"/first\0"));
    second.with_cwd(&w2, |b| assert_eq!(b, b"/second\0"));
}

/// Two witnesses for one task coexist in the kernel — an interrupt handler
/// above a syscall on the same task — and both authorise the same storage.
#[test]
fn two_witnesses_for_one_task_agree() {
    let task = fresh();
    let outer = window(&task);
    let inner = window(&task);

    assert!(task.set_cwd(&outer, b"/via-outer"));
    task.with_cwd(&inner, |b| assert_eq!(b, b"/via-outer\0"));

    assert!(task.set_cwd(&inner, b"/via-inner"));
    task.with_cwd(&outer, |b| assert_eq!(b, b"/via-inner\0"));
}

/// A witness is a safety argument, so one naming the wrong task is unsound
/// rather than merely wrong: the owner check must be loud, not silent.
#[test]
#[should_panic(expected = "witness names a different task")]
fn a_witness_for_another_task_is_refused() {
    let first = fresh();
    let second = fresh();
    let w = window(&first);
    // The owner check is a debug assertion; in release it compiles out and the
    // type-level sealing of `TaskExclusive` is what remains.
    second.set_cwd(&w, b"/nope");
}
