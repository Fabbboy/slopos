//! Platform (ACPI) bus matchmaker regression tests.
//!
//! Exercises [`crate::platform_bus::matchmake`] over synthetic devices +
//! drivers with a heap-backed claim sink — no ACPI namespace, no real hardware,
//! no live claim table. Mirrors `pci_binding.rs`: one-driver-per-device binding,
//! priority ordering (specific before generic), dup-claim prevention, and
//! probe-failure leaving the device unbound.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::dev::Devres;
use slopos_ostd::{AllocError, KVec};
use slopos_testing::{TestResult, fail, pass};

use crate::driver_core::platform_bound::BoundPlatformDevice;
use crate::platform_bus::{
    MAX_PLATFORM_IO, PlatformClaimSink, PlatformDeviceInfo, PlatformDriverEntry, PlatformIoWindow,
    PlatformMatch, PlatformProbeError, ProbeOutcome, matchmake,
};

/// A synthetic device that matched a `'static` id.
fn device(id: &'static [u8]) -> PlatformDeviceInfo {
    PlatformDeviceInfo {
        matched_id: id,
        io: [PlatformIoWindow::default(); MAX_PLATFORM_IO],
        io_count: 0,
        irq: None,
        present: None,
    }
}

/// `PlatformClaimSink` backed by a heap `KVec`, indexed by device index.
struct TestClaims {
    bound: RefCell<KVec<Option<&'static str>>>,
}

impl TestClaims {
    fn new(n: usize) -> Result<Self, AllocError> {
        let mut v = KVec::new();
        for _ in 0..n {
            v.push(None)?;
        }
        Ok(Self {
            bound: RefCell::new(v),
        })
    }

    fn owner(&self, dev_idx: usize) -> Option<&'static str> {
        self.bound.borrow().get(dev_idx).copied().flatten()
    }
}

impl PlatformClaimSink for TestClaims {
    fn is_claimed(&self, dev_idx: usize) -> bool {
        self.bound
            .borrow()
            .get(dev_idx)
            .copied()
            .flatten()
            .is_some()
    }
    fn record(&self, dev_idx: usize, name: &'static str, _devres: Devres) {
        if let Some(slot) = self.bound.borrow_mut().get_mut(dev_idx) {
            *slot = Some(name);
        }
    }
}

fn bind_probe(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    Ok(ProbeOutcome::Bound)
}

fn fail_probe(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    Err(PlatformProbeError::DeviceFault)
}

// --- Test 1: a matching driver binds the device. --------------------------

static T1_DRV: PlatformDriverEntry = PlatformDriverEntry {
    name: "t1-kbd",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 128,
    fallback: None,
    probe: bind_probe,
};

pub fn test_platform_binding_records_claim() -> TestResult {
    let devices = [device(b"PNP0303")];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    let drivers: [&'static PlatformDriverEntry; 1] = [&T1_DRV];
    if matchmake(&devices, &drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    match claims.owner(0) {
        Some("t1-kbd") => pass!(),
        other => fail!("expected t1-kbd to bind, got {:?}", other),
    }
}

// --- Test 2: specific (lower priority) offered before generic. -------------

static T2_SEQ: AtomicU32 = AtomicU32::new(0);
static T2_SPECIFIC_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);
static T2_GENERIC_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);

fn t2_specific(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    T2_SPECIFIC_SEQ.store(T2_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}
fn t2_generic(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    T2_GENERIC_SEQ.store(T2_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Bound)
}

// Registered generic-first to prove ordering comes from priority, not order.
static T2_GENERIC_DRV: PlatformDriverEntry = PlatformDriverEntry {
    name: "t2-generic",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 200,
    fallback: None,
    probe: t2_generic,
};
static T2_SPECIFIC_DRV: PlatformDriverEntry = PlatformDriverEntry {
    name: "t2-specific",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 10,
    fallback: None,
    probe: t2_specific,
};

pub fn test_platform_specific_beats_generic() -> TestResult {
    T2_SEQ.store(0, Ordering::Relaxed);
    T2_SPECIFIC_SEQ.store(u32::MAX, Ordering::Relaxed);
    T2_GENERIC_SEQ.store(u32::MAX, Ordering::Relaxed);
    let devices = [device(b"PNP0303")];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    let drivers: [&'static PlatformDriverEntry; 2] = [&T2_GENERIC_DRV, &T2_SPECIFIC_DRV];
    if matchmake(&devices, &drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    let spec = T2_SPECIFIC_SEQ.load(Ordering::Relaxed);
    let generic = T2_GENERIC_SEQ.load(Ordering::Relaxed);
    if spec == u32::MAX {
        return fail!("specific driver was never offered the device");
    }
    if spec >= generic {
        return fail!(
            "specific (seq {}) must precede generic (seq {})",
            spec,
            generic
        );
    }
    match claims.owner(0) {
        Some("t2-generic") => pass!(),
        other => fail!(
            "generic should bind after specific declined, got {:?}",
            other
        ),
    }
}

// --- Test 3: a claimed device is not offered to another matching driver. ---

static T3_B_PROBES: AtomicU32 = AtomicU32::new(0);

fn t3_b_probe(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    T3_B_PROBES.fetch_add(1, Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}

static T3_A: PlatformDriverEntry = PlatformDriverEntry {
    name: "t3-a",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 10,
    fallback: None,
    probe: bind_probe,
};
static T3_B: PlatformDriverEntry = PlatformDriverEntry {
    name: "t3-b",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 20,
    fallback: None,
    probe: t3_b_probe,
};

pub fn test_platform_dup_claim_prevention() -> TestResult {
    T3_B_PROBES.store(0, Ordering::Relaxed);
    let devices = [device(b"PNP0303")];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    let drivers: [&'static PlatformDriverEntry; 2] = [&T3_A, &T3_B];
    if matchmake(&devices, &drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    if claims.owner(0) != Some("t3-a") {
        return fail!("higher-priority t3-a should own the device");
    }
    match T3_B_PROBES.load(Ordering::Relaxed) {
        0 => pass!(),
        n => fail!(
            "t3-b should not run after the device bound; ran {} times",
            n
        ),
    }
}

// --- Test 4: probe Err leaves the device unbound. --------------------------

static T4_DRV: PlatformDriverEntry = PlatformDriverEntry {
    name: "t4-fail",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 128,
    fallback: None,
    probe: fail_probe,
};

pub fn test_platform_probe_failure_unbinds() -> TestResult {
    let devices = [device(b"PNP0303")];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    let drivers: [&'static PlatformDriverEntry; 1] = [&T4_DRV];
    if matchmake(&devices, &drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    if claims.owner(0).is_some() {
        return fail!("device must stay unbound after probe Err");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_platform_binding_records_claim,
    suite = platform_binding
);
slopos_testing::stest!(
    name = test_platform_specific_beats_generic,
    suite = platform_binding
);
slopos_testing::stest!(
    name = test_platform_dup_claim_prevention,
    suite = platform_binding
);
slopos_testing::stest!(
    name = test_platform_probe_failure_unbinds,
    suite = platform_binding
);
