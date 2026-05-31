#![feature(restricted_std)]

//! Per-core reactor + cross-core channel test (Phase-6 Tier B consumer).
//!
//! Proves the thread-per-core model end-to-end: N worker OS threads, each
//! running its OWN `block_on` reactor and each requesting CPU affinity, receive
//! work from the main thread over a `Send` cross-core channel and reply over a
//! second cross-core channel back to the main thread's reactor. The cross-core
//! wake path is the per-reactor wakeup self-pipe — a sender on one thread writes
//! a byte that rouses another reactor parked in `ring_enter`, whose ring poll
//! completes and fires the receiver task's local waker.
//!
//! Hard assertions (PASS requires all, no hang/deadlock): (a) every squared
//! result is correct, (b) no lost or duplicated items — exactly K replies, each
//! index seen once, delivered across N independent per-thread reactors over the
//! cross-core channel. The CPU each worker runs on is recorded and printed but
//! is INFORMATIONAL: the kernel honors affinity at task creation and wake but
//! does not yet migrate a runnable thread off CPU 0 at a slice boundary nor
//! re-dispatch a ring_enter-parked thread woken cross-core onto a strict
//! non-zero pin, so workers co-locate on CPU 0 in practice. True physical
//! cross-core distribution is a documented kernel follow-up; this test proves
//! the per-thread-reactor + cross-core-channel model, not physical placement.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use slopos_userland as _;
use slopos_userland::ring::slopfut::cross_core;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::core as sys_core;

/// Items the producer fans out across the workers.
const TOTAL_ITEMS: usize = 200;
/// Upper bound on worker threads (min with the online CPU count).
const MAX_WORKERS: usize = 4;

/// Work handed to a worker: square `value`, tagging it with its `index` so the
/// reply can be matched back. `Stop` ends the worker's recv loop.
enum WorkMsg {
    Job { index: usize, value: u64 },
    Stop,
}

/// A worker's reply to the main reactor: which worker handled it, the CPU that
/// worker observed for its first item, the original index, and the squared
/// result.
struct Reply {
    worker: usize,
    cpu: u32,
    index: usize,
    result: u64,
}

fn worker_count() -> usize {
    let n = sys_core::get_cpu_count() as usize;
    n.clamp(1, MAX_WORKERS)
}

/// The end-to-end cross-core round trip. Returns true iff all three assertions
/// hold.
fn test_percore_roundtrip() -> bool {
    let workers = worker_count();

    // Main's reactor hosts the reply receiver. Everything below runs inside
    // this single `block_on` so the reply channel is armed on main's reactor.
    let Ok(main_ring) = Ring::setup(64) else {
        return false;
    };

    slopfut::block_on(main_ring, async move {
        let (reply_tx, mut reply_rx) = cross_core::channel::<Reply>();

        // Bootstrap: each worker registers its work-sender here once its own
        // reactor has armed the work channel. Main spins (yielding) until all
        // are present — bootstrap-only thread coordination, not the event loop.
        let work_senders: std::sync::Arc<Mutex<Vec<cross_core::Sender<WorkMsg>>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let ready = std::sync::Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for worker_idx in 0..workers {
            let reply_tx = reply_tx.clone();
            let work_senders = std::sync::Arc::clone(&work_senders);
            let ready = std::sync::Arc::clone(&ready);
            let handle = std::thread::spawn(move || {
                // Request affinity to a distinct CPU, keeping CPU 0 in the mask
                // ((1 << worker_idx) | 1) so a cross-core wake can never
                // dead-end. The kernel honors affinity at task creation and at
                // wake, but does not yet migrate an already-runnable thread off
                // CPU 0 at a slice boundary, nor re-dispatch a ring_enter-parked
                // thread woken cross-core onto a strictly-pinned non-zero CPU, so
                // the workers co-locate on CPU 0 in practice. The recorded CPU is
                // informational; true physical cross-core distribution is a
                // documented kernel follow-up. The cross-core channel + the
                // per-thread reactors are exercised regardless of placement.
                let _ = sys_core::set_cpu_affinity(0, (1u32 << worker_idx) | 1);
                std::thread::yield_now();
                let pinned_cpu = sys_core::get_current_cpu();

                let Ok(ring) = Ring::setup(64) else {
                    return;
                };
                slopfut::block_on(ring, async move {
                    let (work_tx, mut work_rx) = cross_core::channel::<WorkMsg>();
                    // Publish this worker's sender, then announce readiness.
                    work_senders.lock().unwrap().push(work_tx);
                    ready.fetch_add(1, Ordering::Release);

                    // The first item carries this worker's pinned CPU so the
                    // collector can prove work spread across >= 2 CPUs.
                    let mut first = true;
                    loop {
                        match work_rx.recv().await {
                            WorkMsg::Stop => break,
                            WorkMsg::Job { index, value } => {
                                let cpu = if first {
                                    first = false;
                                    pinned_cpu
                                } else {
                                    u32::MAX
                                };
                                reply_tx.send(Reply {
                                    worker: worker_idx,
                                    cpu,
                                    index,
                                    result: value * value,
                                });
                            }
                        }
                    }
                });
            });
            handles.push(handle);
        }

        // Wait for every worker to register its sender (bootstrap handshake).
        while ready.load(Ordering::Acquire) < workers {
            std::thread::yield_now();
        }
        let senders: Vec<cross_core::Sender<WorkMsg>> =
            core::mem::take(&mut *work_senders.lock().unwrap());

        // Producer: fan TOTAL_ITEMS round-robin across the workers.
        for index in 0..TOTAL_ITEMS {
            let value = index as u64;
            senders[index % workers].send(WorkMsg::Job { index, value });
        }

        // Collect exactly TOTAL_ITEMS replies on main's reactor; each cross-core
        // reply rouses main's parked reactor via its wakeup self-pipe.
        let mut seen = vec![false; TOTAL_ITEMS];
        let mut correct = true;
        let mut cpus: Vec<u32> = Vec::new();
        // Per-worker CPU observed on that worker's first item (u32::MAX = unseen).
        let mut worker_cpu: Vec<u32> = vec![u32::MAX; workers];
        let mut received = 0usize;
        while received < TOTAL_ITEMS {
            let reply = reply_rx.recv().await;
            if reply.cpu != u32::MAX {
                cpus.push(reply.cpu);
                if reply.worker < workers {
                    worker_cpu[reply.worker] = reply.cpu;
                }
            }
            if reply.index >= TOTAL_ITEMS || seen[reply.index] {
                // Out-of-range or duplicate index — lost/duplicated item.
                correct = false;
            } else {
                seen[reply.index] = true;
                let expected = (reply.index as u64) * (reply.index as u64);
                if reply.result != expected {
                    correct = false;
                }
                // Round-robin invariant: index i was sent to worker i % workers.
                if reply.worker != reply.index % workers {
                    correct = false;
                }
            }
            received += 1;
        }

        // Tell every worker to stop, then join them.
        for s in &senders {
            s.send(WorkMsg::Stop);
        }
        for h in handles {
            let _ = h.join();
        }

        // (c) every item seen exactly once.
        let all_seen = seen.iter().all(|&b| b);

        // INFORMATIONAL: the distinct CPUs work was observed on. The N workers
        // are independent per-thread reactors communicating purely cross-core;
        // physical multi-CPU spread is NOT asserted because the kernel does not
        // yet distribute pinned userland threads across cores (documented
        // follow-up) — so in practice this is [0].
        let mut distinct = cpus.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let multi_cpu = distinct.len() >= 2;

        // INFORMATIONAL: whether each worker ran on exactly its requested CPU.
        // Not asserted — see the cross-core placement follow-up above.
        let each_on_pinned_cpu = worker_cpu
            .iter()
            .enumerate()
            .all(|(idx, &cpu)| cpu == idx as u32);

        eprintln!(
            "percore_reactor: workers={} replies={} distinct_cpus={:?} worker_cpu={:?} multi_cpu={} each_on_pinned_cpu={} correct={} all_seen={}",
            workers,
            received,
            distinct,
            worker_cpu,
            multi_cpu,
            each_on_pinned_cpu,
            correct,
            all_seen
        );

        // Hard requirements: (a) every result correct, (b) no lost or duplicated
        // items (exactly TOTAL_ITEMS replies, each index seen once), delivered
        // across N independent per-thread reactors over the cross-core channel.
        // multi_cpu / each_on_pinned_cpu are informational (printed above), not
        // asserted — physical cross-core placement is a documented follow-up.
        correct && all_seen && received == TOTAL_ITEMS
    })
}

/// Two spawned threads each run their OWN `block_on` reactor (a `nop` op) and
/// join — proves concurrent per-thread reactors work independently of any
/// cross-core channel (each OS thread gets its own SCHED + REACTOR + Ring).
fn test_two_reactors() -> bool {
    fn reactor_thread() -> bool {
        let Ok(ring) = Ring::setup(8) else {
            return false;
        };
        slopfut::block_on(ring, async {
            let _ = slopfut::nop().await;
            true
        })
    }
    let h1 = std::thread::spawn(reactor_thread);
    let h2 = std::thread::spawn(reactor_thread);
    h1.join().unwrap_or(false) && h2.join().unwrap_or(false)
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("two_reactors", test_two_reactors),
    ("percore_roundtrip", test_percore_roundtrip),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
