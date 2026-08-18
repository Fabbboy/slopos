#![feature(restricted_std)]

//! memfd page-ownership regression test.
//!
//! Invariant under test: a memfd is the single MetaSlot owner of its backing
//! pages (one owning ref per page at ftruncate, mappings layering extra refs on
//! top), so a page survives `munmap` for as long as the fd is open and is freed
//! exactly once. A second owner returns the page to the buddy under a live fd,
//! which the write→unmap→churn→remap pattern check detects as clobbered
//! contents.

use slopos_userland as _;

use slopos_abi::signal::SIGKILL;
use slopos_abi::syscall::posix::{MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_userland::syscall::{core as sys_core, fs, memory, process};

const PAGE: u64 = 4096;
/// Pages per memfd (16 KiB) — small, so many iterations churn the buddy.
const MEMFD_PAGES: usize = 4;
/// Anonymous reuse region (256 KiB) — wide enough that the buddy very likely
/// recycles any wrongly-released memfd frames.
const REUSE_PAGES: usize = 64;
const INTEGRITY_ITERS: usize = 200;
/// Concurrent churn workers (parent + this ≈ the 4 QEMU vCPUs).
const WORKERS: usize = 3;
const WORKER_ITERS: usize = 400;
/// Bounded reap so a wedged child fails the case instead of hanging.
const REAP_SPINS: usize = 400_000;

const EXIT_OK: i32 = 0;
const EXIT_MMAP_FAIL: i32 = 2;
const EXIT_MEMFD_FAIL: i32 = 3;
const EXIT_CORRUPT: i32 = 4;

fn mmap_failed(v: u64) -> bool {
    v == 0 || (v as i64) < 0
}

/// Per-(iter, page) 64-bit magic written to the head of each memfd page.
#[inline]
fn magic(iter: usize, page: usize) -> u64 {
    0x5105_0000_0000_0000 ^ ((iter as u64) << 20) ^ ((page as u64) << 4) ^ 0xABCD
}

fn write_pattern(base: u64, iter: usize, pages: usize) {
    for p in 0..pages {
        let head = (base + p as u64 * PAGE) as *mut u64;
        unsafe { head.write_volatile(magic(iter, p)) }
    }
}

fn check_pattern(base: u64, iter: usize, pages: usize) -> bool {
    for p in 0..pages {
        let head = (base + p as u64 * PAGE) as *const u64;
        if unsafe { head.read_volatile() } != magic(iter, p) {
            return false;
        }
    }
    true
}

/// Touch a byte in every page so the kernel demand-faults the frames in.
fn touch(addr: u64, pages: usize) {
    for i in 0..pages {
        let p = (addr + i as u64 * PAGE) as *mut u8;
        unsafe { p.write_volatile(0xAB) }
    }
}

fn reap_bounded(pid: u32) -> Option<i32> {
    for _ in 0..REAP_SPINS {
        if let Some(code) = process::waitpid_nohang(pid) {
            return Some(code);
        }
        sys_core::yield_now();
    }
    None
}

/// One map→write→unmap→reuse→remap→verify cycle on a fresh memfd. Returns
/// an exit code (EXIT_OK on success) so it can drive both the in-process
/// loop and the child workers.
fn memfd_cycle(iter: usize) -> i32 {
    let len = MEMFD_PAGES as u64 * PAGE;
    let reuse_len = REUSE_PAGES as u64 * PAGE;

    let fd = memory::memfd_create(0);
    if fd < 0 {
        return EXIT_MEMFD_FAIL;
    }
    if memory::ftruncate(fd, len) < 0 {
        let _ = fs::close_fd_raw(fd);
        return EXIT_MEMFD_FAIL;
    }

    // Map shared, stamp the pattern.
    let a = memory::mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd as i64, 0);
    if mmap_failed(a) {
        let _ = fs::close_fd_raw(fd);
        return EXIT_MMAP_FAIL;
    }
    write_pattern(a, iter, MEMFD_PAGES);

    // Unmap while the fd stays open. Under the bug this freed the backing
    // pages to the buddy even though the memfd is still alive.
    let _ = memory::munmap(a, len);

    // Reuse: churn a big anonymous region to recycle any freed frames and
    // clobber their contents.
    let b = memory::mmap(
        0,
        reuse_len,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        -1,
        0,
    );
    if mmap_failed(b) {
        let _ = fs::close_fd_raw(fd);
        return EXIT_MMAP_FAIL;
    }
    touch(b, REUSE_PAGES);
    let _ = memory::munmap(b, reuse_len);

    // Re-map the SAME memfd and verify the pattern survived. A mismatch
    // means the pages were freed-and-reused under the open fd.
    let c = memory::mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd as i64, 0);
    if mmap_failed(c) {
        let _ = fs::close_fd_raw(fd);
        return EXIT_MMAP_FAIL;
    }
    let intact = check_pattern(c, iter, MEMFD_PAGES);
    let _ = memory::munmap(c, len);
    let _ = fs::close_fd_raw(fd);

    if intact { EXIT_OK } else { EXIT_CORRUPT }
}

fn test_memfd_survives_unmap_while_open() -> bool {
    for iter in 0..INTEGRITY_ITERS {
        match memfd_cycle(iter) {
            EXIT_OK => {}
            EXIT_CORRUPT => {
                eprintln!("mm_stress: iter {iter}: memfd pattern clobbered after unmap+reuse");
                return false;
            }
            EXIT_MMAP_FAIL => {
                eprintln!("mm_stress: iter {iter}: mmap failed (PathCorrupt rollback?)");
                return false;
            }
            EXIT_MEMFD_FAIL => {
                eprintln!("mm_stress: iter {iter}: memfd_create/ftruncate failed");
                return false;
            }
            other => {
                eprintln!("mm_stress: iter {iter}: unexpected code {other}");
                return false;
            }
        }
    }
    true
}

fn worker_loop() -> ! {
    for iter in 0..WORKER_ITERS {
        let code = memfd_cycle(iter);
        if code != EXIT_OK {
            std::process::exit(code);
        }
    }
    std::process::exit(EXIT_OK)
}

fn test_concurrent_memfd_churn() -> bool {
    let mut workers = [0u32; WORKERS];
    let mut spawned = 0usize;
    for slot in workers.iter_mut() {
        let pid = process::fork();
        if pid == 0 {
            worker_loop();
        }
        if pid < 0 {
            eprintln!("mm_stress: failed to fork worker {spawned}");
            break;
        }
        *slot = pid as u32;
        spawned += 1;
    }

    // Drive a cycle stream in the parent too, concurrently with the workers.
    let mut ok = true;
    for iter in 0..WORKER_ITERS {
        if memfd_cycle(iter) != EXIT_OK {
            eprintln!("mm_stress: parent worker failed at iter {iter}");
            ok = false;
            break;
        }
    }

    for &pid in workers.iter().take(spawned) {
        match reap_bounded(pid) {
            Some(EXIT_OK) => {}
            Some(EXIT_CORRUPT) => {
                eprintln!("mm_stress: worker pid {pid} reported memfd corruption");
                ok = false;
            }
            Some(code) => {
                eprintln!("mm_stress: worker pid {pid} exited {code} (mmap/fault)");
                ok = false;
            }
            None => {
                eprintln!("mm_stress: worker pid {pid} never exited");
                let _ = process::kill(pid, SIGKILL);
                let _ = reap_bounded(pid);
                ok = false;
            }
        }
    }

    if spawned < WORKERS {
        return false;
    }
    ok
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "memfd_survives_unmap_while_open",
        test_memfd_survives_unmap_while_open,
    ),
    ("concurrent_memfd_churn", test_concurrent_memfd_churn),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
