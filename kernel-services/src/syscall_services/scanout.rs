//! Singleton-resource arbiter for the display scanout.
//!
//! A single piece of hardware (the scanout / display plane) can be driven by at
//! most one provider at a time, yet several drivers may be willing and able to
//! drive it (the firmware framebuffer, virtio-gpu, Intel xe). This module owns
//! the arbitration: providers register as ordinary `pci_driver!`s and **claim**
//! the scanout by priority. The highest-priority claimant wins; the displaced
//! owner is evicted through its own `evict` callback.
//!
//! The arbiter lives here (not in `video` or `drivers`) because it must be
//! reachable from both — `drivers` depends on this crate, and `video` does too,
//! but `drivers` must never name a `video` symbol. The arbiter therefore stores
//! only plain data and function pointers; the actual install work lives in
//! `video` and is reached through [`register_scanout_installer`] /
//! [`run_scanout_install`].

use core::ffi::c_int;
use slopos_ostd::lock_class;

use slopos_abi::damage::DamageRect;
use slopos_abi::{DisplayInfo, FramebufferData};
use slopos_ostd::klog_info;
use slopos_ostd::sync::{LockClassKey, SpinLock, LOCK_LEVEL_RESOURCE};

/// Outcome of a [`SingletonResource::claim`] reservation attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    /// The caller out-ranked the current owner and any in-flight reservation; it
    /// may now perform its (destructive) device bring-up and then commit.
    Won,
    /// A higher-priority provider already owns or has reserved the resource. The
    /// caller must stay passive and touch no hardware.
    Lost,
    /// A provider at the same priority already owns or reserved the resource.
    /// First-come keeps it; the caller stays passive.
    LostTie,
}

struct ArbiterState<P: Copy + 'static> {
    /// The committed owner and the priority it won at.
    owner: Option<(P, i32)>,
    /// Priority of an in-flight winner that has reserved but not yet committed.
    reserved: Option<i32>,
}

/// Generic two-phase claim/commit arbiter for a resource only one provider may
/// own at a time.
///
/// Phase A ([`claim`](Self::claim)) is a pure-priority reservation that touches
/// no hardware and releases the lock before returning. Phase B
/// ([`commit_install`](Self::commit_install)) records the new owner and hands
/// the displaced provider back so the caller can evict it — the evict callback
/// runs **after** the lock is dropped, so the arbiter never re-enters itself.
pub struct SingletonResource<P: Copy + 'static> {
    state: SpinLock<ArbiterState<P>>,
    name: &'static str,
}

impl<P: Copy + 'static> SingletonResource<P> {
    /// The lock class comes from the caller for the same reason `name` does.
    /// Minted here it would merge every arbiter — including the scratch ones
    /// the tests declare — into one class, and a test's nesting would then be
    /// indistinguishable from the production resource's.
    pub const fn new(name: &'static str, class: &'static LockClassKey) -> Self {
        Self {
            state: SpinLock::new(
                ArbiterState {
                    owner: None,
                    reserved: None,
                },
                class,
            ),
            name,
        }
    }

    /// Phase A: reserve the resource by priority. Touches no hardware; the lock
    /// is released before returning. A claimant must strictly out-rank the
    /// higher of the current owner's priority and any in-flight reservation.
    pub fn claim(&'static self, priority: i32) -> ClaimOutcome {
        let mut st = self.state.lock();
        let owner_prio = st.owner.map(|(_, p)| p);
        let bar = match (owner_prio, st.reserved) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        match bar {
            None => {
                st.reserved = Some(priority);
                ClaimOutcome::Won
            }
            Some(b) if priority > b => {
                st.reserved = Some(priority);
                ClaimOutcome::Won
            }
            Some(b) if priority == b => ClaimOutcome::LostTie,
            Some(_) => ClaimOutcome::Lost,
        }
    }

    /// Phase B: record `new` as the owner and clear the reservation. The
    /// displaced owner (if any) is passed to `evict_with`, which runs after the
    /// arbiter lock is dropped.
    pub fn commit_install(
        &'static self,
        new: P,
        priority: i32,
        evict_with: impl FnOnce(Option<P>),
    ) {
        let displaced = {
            let mut st = self.state.lock();
            let old = st.owner.map(|(p, _)| p);
            st.owner = Some((new, priority));
            st.reserved = None;
            old
        };
        klog_info!("{}: scanout owner committed (prio {})", self.name, priority);
        evict_with(displaced);
    }

    /// Release a reservation taken by a winner whose device bring-up then failed,
    /// restoring the prior owner.
    pub fn abort_claim(&'static self) {
        self.state.lock().reserved = None;
    }

    /// The current committed owner, if any.
    pub fn current(&'static self) -> Option<P> {
        self.state.lock().owner.map(|(p, _)| p)
    }
}

/// Identifies the scanout providers participating in arbitration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanoutId {
    /// The passive firmware framebuffer (direct CPU writes, no flush callback).
    FirmwareFb,
    /// virtio-gpu 2D scanout.
    VirtioGpu,
    /// Intel xe display plane.
    IntelXe,
}

/// Hardware-cursor + runtime mode-set entry points a GPU provider exposes to the
/// compositor (mirrors the legacy `register_gpu_control` four-tuple).
#[derive(Clone, Copy)]
pub struct GpuControlFns {
    pub available: fn() -> bool,
    pub set_image: fn(*const u8, usize, u32, u32) -> bool,
    pub move_cursor: fn(u32, u32) -> bool,
    pub set_mode: fn(u32, u32) -> Option<FramebufferData>,
}

/// Everything the `video`-side installer needs to adopt a scanout. Passed **by
/// reference** so the (small) value never bloats a probe stack frame.
pub struct InstallCtx {
    pub fb: FramebufferData,
    /// Present hook, or `None` for a direct-write (firmware) backing.
    pub flush: Option<fn(*const DamageRect, u32) -> c_int>,
    pub gpu_control: Option<GpuControlFns>,
}

/// A registered scanout provider. Copy-cheap: an id, the priority it won at, and
/// its eviction hook.
#[derive(Clone, Copy)]
pub struct ScanoutProvider {
    pub id: ScanoutId,
    pub priority: i32,
    /// Run when this provider is displaced by a higher-priority one.
    pub evict: fn(),
}

/// The one display-scanout arbiter.
pub static SCANOUT: SingletonResource<ScanoutProvider> =
    SingletonResource::new("scanout", lock_class!("SCANOUT.state", LOCK_LEVEL_RESOURCE));

// Single source of truth for the claim priority ladder. Lower providers are
// always losable; a cmdline hint folds into priority rather than gating a probe.
pub const PRIO_FIRMWARE_FB: i32 = 0;
pub const PRIO_VIRTIO_GPU: i32 = 30;
pub const PRIO_INTEL_XE: i32 = 50;
/// Added to the firmware priority when the cmdline forces the passive backend,
/// lifting it above every GPU so their claims lose without a gate in `matches`.
pub const PRIO_CMDLINE_HINT_BUMP: i32 = 100;

// The `video`-side install logic, reached by `drivers` through a fn-pointer so
// no `drivers -> video` dependency edge is needed.
static SCANOUT_INSTALLER: SpinLock<Option<fn(&InstallCtx) -> bool>> =
    SpinLock::new(None, lock_class!("SCANOUT_INSTALLER", LOCK_LEVEL_RESOURCE));

/// Register the install callback. Called once by `video::init`.
pub fn register_scanout_installer(installer: fn(&InstallCtx) -> bool) {
    *SCANOUT_INSTALLER.lock() = Some(installer);
}

/// Invoke the registered installer. The fn-pointer is copied out and the lock
/// dropped before the (potentially blocking) installer runs.
pub fn run_scanout_install(ctx: &InstallCtx) -> bool {
    let installer = *SCANOUT_INSTALLER.lock();
    match installer {
        Some(installer) => installer(ctx),
        None => false,
    }
}

// The framebuffer the next provider seeds from (dimensions for mode choice, plus
// the backing address it copies the existing image across from). Stored as an
// integer address + `DisplayInfo` so the static stays `Send`/`Sync` without
// holding a raw pointer.
static CURRENT_FB: SpinLock<Option<(u64, DisplayInfo)>> =
    SpinLock::new(None, lock_class!("CURRENT_FB", LOCK_LEVEL_RESOURCE));

/// Record the framebuffer a subsequent provider should seed from.
pub fn set_current_framebuffer(fb: FramebufferData) {
    *CURRENT_FB.lock() = Some((fb.address as u64, fb.info));
}

/// The framebuffer a provider taking over should seed from, if one is live.
pub fn current_framebuffer() -> Option<FramebufferData> {
    CURRENT_FB.lock().map(|(address, info)| FramebufferData {
        address: address as *mut u8,
        info,
    })
}
