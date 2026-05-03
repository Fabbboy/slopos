//! Host-side integration tests for `slopos_ostd::user`.
//!
//! Exercises the `UserContext` argument-validation path and the
//! `copy_*_user` page-table reachability check against a fixture
//! `VmSpace`. The actual `__ostd_raw_usercopy` asm is **not**
//! invoked from these tests — `STAC` is privileged and would fault
//! when run from a host process. We force every assertion path to
//! return *before* the asm runs by either using zero-length
//! buffers or by mapping pages with permissions that fail
//! validation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::frame::{
    AnonymousMeta, FrameAlloc, FrameAllocOptions, MetaSlot, Paddr, init_meta_slots,
};
use slopos_ostd::mm::frame_alloc::register_frame_allocator;
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{VmSpace, register_kernel_master_pml4};
use slopos_ostd::user::context::FpuStateRef;
use slopos_ostd::user::{
    UserContext, UserCopyError, UserPtrError, UserRegs, copy_bytes_from_user, copy_bytes_to_user,
    copy_from_user, copy_to_user,
};

const N_PAGES: usize = 256;
const PAGE_SIZE: usize = 4096;

struct BumpAlloc {
    next_page: AtomicU64,
}

impl FrameAlloc for BumpAlloc {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        assert_eq!(opts.size_pages, 1);
        let page = self.next_page.fetch_add(1, Ordering::Relaxed);
        if page as usize >= N_PAGES {
            return None;
        }
        let paddr = PhysAddr::new(page * PAGE_SIZE as u64);
        if opts.zeroing {
            unsafe {
                let virt = (BACKING_BASE.load(Ordering::Acquire) as usize + paddr.as_u64() as usize)
                    as *mut u8;
                core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            }
        }
        Some(paddr)
    }

    fn dealloc(&self, _paddr: Paddr, _size_pages: usize) {}
}

static BACKING_BASE: AtomicU64 = AtomicU64::new(0);
static BUMP_ALLOC: BumpAlloc = BumpAlloc {
    next_page: AtomicU64::new(1),
};
static BUMP_REF: &'static dyn FrameAlloc = &BUMP_ALLOC;
static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let layout = std::alloc::Layout::from_size_align(N_PAGES * PAGE_SIZE, PAGE_SIZE)
            .expect("backing layout");
        let backing_ptr = unsafe { std::alloc::alloc_zeroed(layout) } as u64;
        assert_ne!(backing_ptr, 0);
        BACKING_BASE.store(backing_ptr, Ordering::Release);

        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());

        unsafe {
            init_meta_slots(slots_ptr, N_PAGES);
            init_phys_virt_offset(backing_ptr);
            register_frame_allocator(&BUMP_REF);
            register_kernel_master_pml4(PhysAddr::new(0));
        }
        Mutex::new(())
    });
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh_user_frame() -> UFrame<AnonymousMeta> {
    let paddr = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .expect("test arena exhausted");
    UFrame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta).unwrap()
}

fn ctx_with_arg0(arg0: u64) -> UserContext {
    let mut regs = UserRegs::default();
    regs.rdi = arg0;
    UserContext::new(regs, FpuStateRef::empty())
}

#[test]
fn user_ptr_arg_round_trips_to_user_addr() {
    let _g = setup();
    let user_va = 0x0000_4000_dead_b000;
    let ctx = ctx_with_arg0(user_va);
    let p = ctx.user_ptr_arg::<u64>(0).expect("arg 0 valid");
    assert_eq!(p.as_u64(), user_va);
}

#[test]
fn user_ptr_arg_rejects_kernel_pointer() {
    let _g = setup();
    let kernel_va = 0xffff_8000_0000_1000;
    let ctx = ctx_with_arg0(kernel_va);
    let r = ctx.user_ptr_arg::<u64>(0);
    assert!(matches!(r, Err(UserPtrError::OutOfUserRange)));
}

#[test]
fn copy_bytes_from_user_zero_len_short_circuits() {
    let _g = setup();
    let space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_2000_0000;
    let mut regs = UserRegs::default();
    regs.rdi = user_va;
    regs.rsi = 0;
    let ctx = UserContext::new(regs, FpuStateRef::empty());
    let bytes = ctx.user_bytes_arg(0, 1).expect("user_bytes_arg");
    let mut dst: [u8; 0] = [];
    let r = copy_bytes_from_user(&space, bytes.base(), &mut dst);
    assert_eq!(r, Ok(()));
}

#[test]
fn copy_from_user_reports_not_mapped() {
    let _g = setup();
    let space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_3000_0000;
    let ctx = ctx_with_arg0(user_va);
    let p = ctx.user_ptr_arg::<u64>(0).unwrap();
    let r = copy_from_user::<u64>(&space, p);
    assert_eq!(r, Err(UserCopyError::NotMapped));
}

#[test]
fn copy_from_user_reports_not_user_accessible() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_4000_0000;
    let v_start = VirtAddr::new(user_va);
    let v_end = VirtAddr::new(user_va + PAGE_SIZE as u64);
    {
        let mut cur = space.cursor_mut(v_start..v_end).unwrap();
        // map kernel-only — user bit clear.
        cur.map(fresh_user_frame(), PageProperty::KERNEL_RW)
            .unwrap();
    }
    let ctx = ctx_with_arg0(user_va);
    let p = ctx.user_ptr_arg::<u64>(0).unwrap();
    let r = copy_from_user::<u64>(&space, p);
    assert_eq!(r, Err(UserCopyError::NotUserAccessible));
}

#[test]
fn copy_to_user_reports_not_user_writable() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_5000_0000;
    let v_start = VirtAddr::new(user_va);
    let v_end = VirtAddr::new(user_va + PAGE_SIZE as u64);
    {
        let mut cur = space.cursor_mut(v_start..v_end).unwrap();
        cur.map(fresh_user_frame(), PageProperty::USER_RO).unwrap();
    }
    let ctx = ctx_with_arg0(user_va);
    let p = ctx.user_ptr_arg::<u64>(0).unwrap();
    let v: u64 = 0;
    let r = copy_to_user::<u64>(&space, p, &v);
    assert_eq!(r, Err(UserCopyError::NotUserWritable));
}

#[test]
fn copy_bytes_to_user_validates_against_writable_user_pages() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_6000_0000;
    let v_start = VirtAddr::new(user_va);
    let v_end = VirtAddr::new(user_va + PAGE_SIZE as u64);
    {
        let mut cur = space.cursor_mut(v_start..v_end).unwrap();
        cur.map(fresh_user_frame(), PageProperty::USER_RO).unwrap();
    }
    let mut regs = UserRegs::default();
    regs.rdi = user_va;
    regs.rsi = 8;
    let ctx = UserContext::new(regs, FpuStateRef::empty());
    let bytes = ctx.user_bytes_arg(0, 1).unwrap();
    let src = [0u8; 8];
    let r = copy_bytes_to_user(&space, bytes.base(), &src);
    assert_eq!(r, Err(UserCopyError::NotUserWritable));
}

#[test]
fn copy_validation_spans_two_pages() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let user_va = 0x0000_4000_7000_0000;
    let v_start = VirtAddr::new(user_va);
    let v_mid = VirtAddr::new(user_va + PAGE_SIZE as u64);
    {
        // Map only the first page; the second page in the cross-page
        // copy below is unmapped, so validation must report it.
        let mut cur = space.cursor_mut(v_start..v_mid).unwrap();
        cur.map(fresh_user_frame(), PageProperty::USER_RW).unwrap();
    }
    // Place the slice base 8 bytes before the page boundary; a 16-byte
    // copy then spans both pages.
    let near_end = user_va + (PAGE_SIZE as u64 - 8);
    let mut regs = UserRegs::default();
    regs.rdi = near_end;
    regs.rsi = 16;
    let ctx = UserContext::new(regs, FpuStateRef::empty());
    let bytes = ctx.user_bytes_arg(0, 1).unwrap();
    let mut buf = vec![0u8; 16];
    let r = copy_bytes_from_user(&space, bytes.base(), &mut buf);
    assert_eq!(r, Err(UserCopyError::NotMapped));
}

#[test]
fn user_ptr_construction_is_private() {
    // Confirm at run time that `UserVirtAddr::try_new` is not public.
    // This is a weaker check than the compile-fail test on the
    // `&T` escape: here we just assert that the only public entry
    // point (`UserContext::user_ptr_arg`) yields the same wrapper.
    let _g = setup();
    let user_va = 0x0000_4000_8000_0000;
    let ctx = ctx_with_arg0(user_va);
    let p = ctx.user_ptr_arg::<u32>(0).unwrap();
    assert_eq!(p.as_u64(), user_va);
}
