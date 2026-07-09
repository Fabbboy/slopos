use std::sync::{Arc, Barrier};
use std::thread;

use slopos_ostd::{KArc, KWeak};

#[test]
fn upgrade_racing_last_drop_never_resurrects_dead_data() {
    #[cfg(not(miri))]
    const ITERATIONS: usize = 10_000;
    // Miri explores each atomic interleaving and is intentionally much slower
    // than native execution; this still exercises the real two-thread race.
    #[cfg(miri)]
    const ITERATIONS: usize = 32;

    for value in 0..ITERATIONS {
        let strong = KArc::try_new(value).expect("KArc allocation");
        let weak = KArc::downgrade(&strong);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            weak.upgrade().map(|upgraded| *upgraded)
        });

        barrier.wait();
        drop(strong);
        if let Some(observed) = worker.join().expect("upgrade worker panicked") {
            assert_eq!(observed, value);
        }
    }
}

#[test]
fn cyclic_construction_is_fallible_and_publishes_after_initialization() {
    struct Node {
        value: usize,
        myself: KWeak<Node>,
    }

    let node = KArc::try_new_cyclic(|weak| {
        assert!(weak.upgrade().is_none());
        Node {
            value: 42,
            myself: weak.clone(),
        }
    })
    .expect("KArc cyclic allocation");

    let upgraded = node.myself.upgrade().expect("published cyclic weak");
    assert_eq!(upgraded.value, 42);
    assert!(KArc::ptr_eq(&node, &upgraded));
}

#[test]
fn unsized_coercion_preserves_allocation_identity_and_drop() {
    trait Value: Send + Sync {
        fn value(&self) -> usize;
    }

    struct Concrete(usize);
    impl Value for Concrete {
        fn value(&self) -> usize {
            self.0
        }
    }

    let concrete = KArc::try_new(Concrete(7)).expect("KArc allocation");
    let concrete_weak = KArc::downgrade(&concrete);
    let erased_weak: KWeak<dyn Value> = concrete_weak;
    let erased: KArc<dyn Value> = concrete;
    assert_eq!(erased.value(), 7);
    assert_eq!(erased_weak.upgrade().map(|value| value.value()), Some(7));
    drop(erased);
    assert!(erased_weak.upgrade().is_none());
}
