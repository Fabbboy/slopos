#![feature(restricted_std)]

//! `slopfut` production-runtime test: real wakers + multi-task scheduler.
//!
//! Exercises the Phase-2 surfaces — `spawn`/`JoinHandle`, `join2`, `timeout`,
//! the async sync primitives (`Notify`/`oneshot`/`mpsc`), `Child::wait` (via
//! pidfd), and async signal delivery (via signalfd) — all driven by one
//! `block_on` over a ring, proving the executor wakes tasks on completion
//! rather than re-polling everything.

use slopos_abi::signal::{SIGCHLD, sig_bit};
use slopos_userland as _;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::{core as sys_core, process};

/// spawn a task doing a ring op, await its JoinHandle for the value.
fn test_spawn_join() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let got = slopfut::block_on(ring, async {
        let h = slopfut::spawn(async {
            // A real ring op so the task actually suspends and is woken.
            let _ = slopfut::nop().await;
            7i32
        });
        h.await
    });
    got == 7
}

/// join2 runs two futures concurrently to completion.
fn test_join2() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    slopfut::block_on(ring, async {
        let (a, b) = slopfut::join2(slopfut::nop(), slopfut::timeout(1_000_000)).await;
        a == 0 && b < 0 // nop -> 0; timeout -> -ETIME
    })
}

/// timeout(short, long-sleep) elapses; timeout(long, nop) completes.
fn test_timeout() -> bool {
    let Ok(r1) = Ring::setup(8) else {
        return false;
    };
    let elapsed = slopfut::block_on(
        r1,
        slopfut::time::timeout(10, slopfut::time::sleep_ms(5000)),
    )
    .is_err();
    let Ok(r2) = Ring::setup(8) else {
        return false;
    };
    let completed = slopfut::block_on(r2, slopfut::time::timeout(5000, slopfut::nop())).is_ok();
    elapsed && completed
}

/// Notify: a spawned task wakes the awaiting root.
fn test_notify() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    slopfut::block_on(ring, async {
        let n = slopfut::sync::Notify::new();
        let n2 = n.clone();
        slopfut::spawn(async move {
            // Suspend once on a ring op so ordering exercises the waker path.
            let _ = slopfut::nop().await;
            n2.notify_one();
        });
        n.notified().await;
        true
    })
}

/// oneshot channel carries a value from a spawned task.
fn test_oneshot() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    slopfut::block_on(ring, async {
        let (tx, rx) = slopfut::sync::oneshot::<i32>();
        slopfut::spawn(async move {
            let _ = slopfut::nop().await;
            tx.send(42);
        });
        rx.await == Some(42)
    })
}

/// unbounded mpsc carries multiple values then closes.
fn test_mpsc() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    slopfut::block_on(ring, async {
        let (tx, mut rx) = slopfut::sync::unbounded::<i32>();
        slopfut::spawn(async move {
            for i in 0..3 {
                tx.send(i);
            }
            // tx dropped here → channel closes after draining.
        });
        let mut sum = 0;
        while let Some(v) = rx.recv().await {
            sum += v;
        }
        sum == 0 + 1 + 2
    })
}

/// yield_now lets a spawned sibling run before the root resumes.
fn test_yield() -> bool {
    use std::cell::RefCell;
    use std::rc::Rc;
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    slopfut::block_on(ring, async {
        let order: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
        let o2 = order.clone();
        slopfut::spawn(async move {
            o2.borrow_mut().push(2);
        });
        order.borrow_mut().push(1);
        slopfut::yield_now().await; // sibling runs here
        order.borrow_mut().push(3);
        *order.borrow() == [1, 2, 3]
    })
}

/// Child::wait awaits a child's exit via pidfd and reaps the code.
fn test_child_wait() -> bool {
    let pid = process::fork();
    if pid == 0 {
        sys_core::exit_with_code(7);
    }
    if pid < 0 {
        return false;
    }
    let child = pid as u32;
    let Ok(ring) = Ring::setup(8) else {
        let _ = process::waitpid(child);
        return false;
    };
    let code = slopfut::block_on(ring, async move {
        slopfut::process::Child::from_pid(child).wait().await
    });
    code == 7
}

/// Async signal delivery: a child's SIGCHLD arrives via a signalfd-backed
/// SignalListener as an awaited event.
fn test_signal_recv() -> bool {
    let Some(listener) = slopfut::signal::SignalListener::new(sig_bit(SIGCHLD)) else {
        return false;
    };
    let pid = process::fork();
    if pid == 0 {
        sys_core::exit_with_code(0);
    }
    if pid < 0 {
        return false;
    }
    let child = pid as u32;
    let Ok(ring) = Ring::setup(8) else {
        let _ = process::waitpid(child);
        return false;
    };
    let signo = slopfut::block_on(ring, async { listener.recv().await });
    let _ = process::waitpid(child);
    signo == SIGCHLD as u32
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("spawn_join", test_spawn_join),
    ("join2", test_join2),
    ("timeout", test_timeout),
    ("notify", test_notify),
    ("oneshot", test_oneshot),
    ("mpsc", test_mpsc),
    ("yield_now", test_yield),
    ("child_wait", test_child_wait),
    ("signal_recv", test_signal_recv),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
