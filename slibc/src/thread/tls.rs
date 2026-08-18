//! TLS initialization — heap-allocate and install TCB via FS_BASE.
//!
//! x86_64 variant-II layout: the thread pointer (`fs_base`) points at the
//! [`Tcb`], the program's TLS image (`.tdata`/`.tbss`) sits *below* it at
//! `[tp - tls_size, tp)`, and `#[thread_local]` statics are addressed at
//! negative offsets from `fs_base`.
//!
//! libc owns all TLS: every thread, main included, builds its own block from
//! the program's `PT_TLS` template discovered via `AT_PHDR`, copying `.tdata`
//! and zeroing `.tbss`. The kernel never constructs a TLS image.

use core::cell::SyncUnsafeCell;
use core::mem;
use core::ptr;

use crate::mem::malloc;
use crate::pal::{Pal, Sys};

use super::tcb::Tcb;

static mut TLS_READY: bool = false;

/// Program TLS template (the `PT_TLS` segment), captured once at startup.
/// All-integer so `SyncUnsafeCell<TlsTemplate>` is `Sync` without an unsafe
/// impl. `image_addr` addresses the pristine `.tdata` image (`p_vaddr`).
#[derive(Clone, Copy)]
struct TlsTemplate {
    image_addr: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
}

static TLS_TEMPLATE: SyncUnsafeCell<TlsTemplate> = SyncUnsafeCell::new(TlsTemplate {
    image_addr: 0,
    filesz: 0,
    memsz: 0,
    align: 8,
});

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

/// Capture the program's `PT_TLS` template by walking the auxv to `AT_PHDR`
/// and scanning the program headers. Idempotent. With no TLS segment or no
/// usable `AT_PHDR` the template stays empty and threads get a TCB-only block.
///
/// # Safety
/// `stack_base` must point at the kernel-prepared entry stack (`argc` at
/// `[stack_base]`, then `argv`, `envp`, and the auxv).
pub unsafe extern "C" fn capture_tls_template_from_stack(stack_base: *const usize) {
    if stack_base.is_null() {
        return;
    }
    let argc = *stack_base;
    if argc > 4096 {
        return; // implausible argc → bail rather than walk garbage
    }
    // envp begins after argc + argv[0..argc] + the argv NULL terminator.
    let mut p = stack_base.add(1 + argc + 1);
    while *p != 0 {
        p = p.add(1);
    }
    p = p.add(1); // step past the envp NULL → first auxv entry

    let (mut phdr, mut phnum, mut phent) = (0usize, 0usize, 0usize);
    loop {
        let a_type = *p as u64;
        let a_val = *p.add(1);
        if a_type == slopos_abi::auxv::AT_NULL {
            break;
        }
        match a_type {
            slopos_abi::auxv::AT_PHDR => phdr = a_val,
            slopos_abi::auxv::AT_PHNUM => phnum = a_val,
            slopos_abi::auxv::AT_PHENT => phent = a_val,
            _ => {}
        }
        p = p.add(2);
    }
    if phdr == 0 || phnum == 0 || phent == 0 {
        return;
    }

    // Load bias is `AT_PHDR - PT_PHDR.p_vaddr`, so 0 for a non-relocated
    // executable. The `.tdata` image at `bias + p_vaddr` lies in the mapped
    // data segment and stays pristine for every thread to copy from.
    // Elf64_Phdr: p_type@0 (u32), p_vaddr@16, p_filesz@32, p_memsz@40, p_align@48.
    const PT_PHDR: u32 = 6;
    const PT_TLS: u32 = 7;
    let mut bias: usize = 0;
    let mut tls: Option<(usize, usize, usize, usize)> = None;
    for i in 0..phnum {
        let ph = (phdr + i * phent) as *const u8;
        let p_type = ptr::read_unaligned(ph as *const u32);
        let p_vaddr = ptr::read_unaligned(ph.add(16) as *const u64) as usize;
        if p_type == PT_PHDR {
            bias = phdr.wrapping_sub(p_vaddr);
        } else if p_type == PT_TLS {
            let p_filesz = ptr::read_unaligned(ph.add(32) as *const u64) as usize;
            let p_memsz = ptr::read_unaligned(ph.add(40) as *const u64) as usize;
            let p_align = ptr::read_unaligned(ph.add(48) as *const u64) as usize;
            tls = Some((
                p_vaddr,
                p_filesz,
                p_memsz,
                if p_align == 0 { 8 } else { p_align },
            ));
        }
    }
    if let Some((p_vaddr, filesz, memsz, align)) = tls {
        *TLS_TEMPLATE.get() = TlsTemplate {
            image_addr: bias.wrapping_add(p_vaddr),
            filesz,
            memsz,
            align,
        };
    }
}

/// Allocate and initialize a per-thread TLS block in variant-II layout.
/// Returns `(alloc_base, tp)`: the raw allocation to free later, and the
/// thread pointer (the [`Tcb`] address) to load into `fs_base`. Null on OOM.
///
/// # Safety
/// Reads `filesz` bytes from the captured template image.
pub unsafe fn alloc_thread_tls() -> (*mut u8, *mut Tcb) {
    let t = *TLS_TEMPLATE.get();
    // The linker computes each thread-local's negative `%fs` offset against
    // `tls_size`, so every thread must size its block identically or the
    // thread-locals land at the wrong address.
    let align = t.align.max(8);
    let tls_size = align_up(t.memsz, align);
    let block_size = tls_size + mem::size_of::<Tcb>();

    let base = malloc::memalign(align, block_size);
    if base.is_null() {
        return (ptr::null_mut(), ptr::null_mut());
    }
    // Zeroing the whole image region is what initialises `.tbss`.
    ptr::write_bytes(base, 0, tls_size);
    if t.filesz > 0 && t.image_addr != 0 {
        ptr::copy_nonoverlapping(t.image_addr as *const u8, base, t.filesz);
    }
    // `base` is `align`-aligned and `tls_size` a multiple of `align`, so `tp`
    // is `align`-aligned too.
    let tp = base.add(tls_size) as *mut Tcb;
    ptr::write_bytes(tp as *mut u8, 0, mem::size_of::<Tcb>());
    (base, tp)
}

#[inline]
pub fn tls_is_initialized() -> bool {
    unsafe { TLS_READY }
}

/// Set up the main thread's TLS. Must run after
/// [`capture_tls_template_from_stack`]. Until `TLS_READY` flips, `errno` uses
/// its static fallback, so the work below is safe with `fs_base == 0`.
///
/// # Safety
/// Must be called exactly once from the main thread during CRT startup.
pub unsafe fn tls_init_main_thread() {
    // Adopt an already-installed valid TCB rather than building a second one.
    if let Ok(fs_base) = Sys::arch_prctl_get_fs() {
        if fs_base != 0 {
            let tcb_ptr = fs_base as *mut Tcb;
            if !tcb_ptr.is_null() && (*tcb_ptr).self_ptr == tcb_ptr {
                (*tcb_ptr).tid = Sys::getpid();
                TLS_READY = true;
                return;
            }
        }
    }

    let (_base, tcb_ptr) = alloc_thread_tls();
    if tcb_ptr.is_null() {
        return;
    }
    (*tcb_ptr).self_ptr = tcb_ptr;
    (*tcb_ptr).tid = Sys::getpid();

    if Sys::arch_prctl_set_fs(tcb_ptr as u64).is_err() {
        return;
    }

    TLS_READY = true;
}

/// # Safety
/// `tcb` must be a valid TCB pointer passed as TLS arg to `clone()`.
pub unsafe fn tls_init_new_thread(tcb: *mut Tcb) {
    debug_assert_eq!((*tcb).self_ptr, tcb);
}
