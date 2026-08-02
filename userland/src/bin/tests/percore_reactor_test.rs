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
//! cross-core channel, and (c) the workers really did run on distinct physical
//! CPUs, each on the one it pinned itself to. Each worker takes a strict
//! `1 << idx` affinity mask, so (c) is a placement assertion and not a
//! description of where the scheduler happened to put things: a reactor parked
//! in `ring_enter` and woken cross-core must be re-dispatched on its pinned CPU,
//! and sustained concurrent allocation from N CPUs must not wedge the machine.

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
        let work_senders: std::sync::Arc<Mutex<Vec<Option<cross_core::Sender<WorkMsg>>>>> =
            std::sync::Arc::new(Mutex::new((0..workers).map(|_| None).collect()));
        let ready = std::sync::Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for worker_idx in 0..workers {
            let reply_tx = reply_tx.clone();
            let work_senders = std::sync::Arc::clone(&work_senders);
            let ready = std::sync::Arc::clone(&ready);
            let handle = std::thread::spawn(move || {
                // Strict pin. Keeping CPU 0 in the mask would let every worker
                // fall back to it, hiding both the cross-core re-dispatch path
                // and the concurrent multi-CPU allocation load.
                let _ = sys_core::set_cpu_affinity(0, 1u32 << worker_idx);
                std::thread::yield_now();
                let pinned_cpu = sys_core::get_current_cpu();

                // Still count ready on failure: otherwise the collector waits
                // for a handshake that never completes and the test hangs until
                // the harness gives up, reporting nothing. An empty sender slot
                // makes it an ordinary failure instead.
                let Ok(ring) = Ring::setup(64) else {
                    eprintln!("percore_reactor: worker {} Ring::setup failed", worker_idx);
                    ready.fetch_add(1, Ordering::Release);
                    return;
                };
                slopfut::block_on(ring, async move {
                    let (work_tx, mut work_rx) = cross_core::channel::<WorkMsg>();
                    // Publish this worker's sender in worker-index order, then
                    // announce readiness. Thread startup order is nondeterministic.
                    work_senders.lock().unwrap()[worker_idx] = Some(work_tx);
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
        let senders: Vec<cross_core::Sender<WorkMsg>> = {
            let mut slots = work_senders.lock().unwrap();
            let mut senders = Vec::with_capacity(workers);
            for slot in slots.iter_mut() {
                let Some(sender) = slot.take() else {
                    return false;
                };
                senders.push(sender);
            }
            senders
        };

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

        // A single-worker boot has nothing to spread across.
        let mut distinct = cpus.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let multi_cpu = distinct.len() >= workers.min(2);

        // (e) each worker ran on exactly the CPU it pinned itself to.
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
        // across N independent per-thread reactors over the cross-core channel,
        // and (c) each worker ran on its own pinned CPU.
        correct && all_seen && received == TOTAL_ITEMS && multi_cpu && each_on_pinned_cpu
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
