use core::mem;
use core::sync::atomic::AtomicI32;

use super::condvar::*;
use super::mutex::*;
use super::rwlock::*;
use super::tcb::PTHREAD_KEYS_MAX;
use super::tcb::*;
use super::*;

pub fn run_thread_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    check!("PTHREAD_CREATE_JOINABLE", PTHREAD_CREATE_JOINABLE == 0);
    check!("PTHREAD_CREATE_DETACHED", PTHREAD_CREATE_DETACHED == 1);
    check!("DEFAULT_STACK_SIZE", DEFAULT_STACK_SIZE == 2 * 1024 * 1024);
    check!("PTHREAD_STACK_MIN", PTHREAD_STACK_MIN == 16384);

    check!("PTHREAD_MUTEX_NORMAL", PTHREAD_MUTEX_NORMAL == 0);
    check!("PTHREAD_MUTEX_RECURSIVE", PTHREAD_MUTEX_RECURSIVE == 1);
    check!("PTHREAD_MUTEX_ERRORCHECK", PTHREAD_MUTEX_ERRORCHECK == 2);

    check!("PTHREAD_KEYS_MAX", PTHREAD_KEYS_MAX == 64);

    check!(
        "tcb self_ptr at offset 0",
        mem::offset_of!(Tcb, self_ptr) == 0
    );
    check!(
        "tcb errno_val after self_ptr",
        mem::offset_of!(Tcb, errno_val) == 8
    );
    check!("tcb tid after errno_val", mem::offset_of!(Tcb, tid) == 12);

    let tcb = Tcb::zeroed();
    check!("tcb zeroed self_ptr null", tcb.self_ptr.is_null());
    check!("tcb zeroed errno 0", tcb.errno_val == 0);
    check!("tcb zeroed tid 0", tcb.tid == 0);
    check!("tcb zeroed detached false", !tcb.detached);
    check!("tcb zeroed child_tid 0", tcb.child_tid == 0);

    let init = PTHREAD_MUTEX_INITIALIZER;
    check!(
        "mutex init state 0",
        init.state.load(core::sync::atomic::Ordering::Relaxed) == 0
    );
    check!("mutex init owner 0", init.owner_tid == 0);
    check!("mutex init kind normal", init.kind == PTHREAD_MUTEX_NORMAL);

    let cond_init = PTHREAD_COND_INITIALIZER;
    check!(
        "cond init seq 0",
        cond_init.seq.load(core::sync::atomic::Ordering::Relaxed) == 0
    );
    check!("cond init mutex null", cond_init.mutex.is_null());

    let rw_init = PTHREAD_RWLOCK_INITIALIZER;
    check!(
        "rwlock init state 0",
        rw_init.state.load(core::sync::atomic::Ordering::Relaxed) == 0
    );
    check!(
        "rwlock init writer_waiting 0",
        rw_init
            .writer_waiting
            .load(core::sync::atomic::Ordering::Relaxed)
            == 0
    );

    check!("pthread_t is u64", mem::size_of::<pthread_t>() == 8);
    check!(
        "AtomicI32 same size as i32",
        mem::size_of::<AtomicI32>() == mem::size_of::<i32>()
    );

    check!("pthread_equal same", unsafe {
        super::join::pthread_equal(42, 42) != 0
    });
    check!("pthread_equal diff", unsafe {
        super::join::pthread_equal(1, 2) == 0
    });

    (pass, fail)
}
