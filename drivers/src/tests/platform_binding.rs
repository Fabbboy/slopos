//! Platform (ACPI) bus matchmaker regression tests.
//!
//! Exercises the generic [`probe_bus`] over synthetic platform devices +
//! drivers with a heap-backed claim sink — no ACPI namespace, no real hardware,
//! no live claim table.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::dev::Devres;
use slopos_ostd::{AllocError, KVec};
use slopos_testing::{TestResult, fail, pass};

use crate::driver_core::bus::{ClaimSink, LinearIndex, probe_bus};
use crate::platform_bus::{
    BoundPlatformDevice, MAX_PLATFORM_IO, PlatformBus, PlatformDeviceInfo, PlatformDriverEntry,
    PlatformIoWindow, PlatformMatch, PlatformProbeError, ProbeOutcome,
};

/// Build the linear driver index the platform bus uses in production over an
/// explicit synthetic driver set.
fn index_of(
    drivers: &[&'static PlatformDriverEntry],
) -> Result<LinearIndex<PlatformBus>, AllocError> {
    let mut entries = KVec::new();
    for &e in drivers {
        entries.push(e)?;
    }
    Ok(LinearIndex::from_entries(entries))
}

/// Run the generic matchmaker over `devices` with `drivers` as the registry.
fn run(
    devices: &[PlatformDeviceInfo],
    drivers: &[&'static PlatformDriverEntry],
    claims: &dyn ClaimSink,
) -> Result<(), AllocError> {
    let idx = index_of(drivers)?;
    probe_bus::<PlatformBus>(&idx, devices.len(), &|i| devices.get(i).copied(), claims)
}

fn device(id: &'static [u8]) -> PlatformDeviceInfo {
    PlatformDeviceInfo {
        matched_id: id,
        io: [PlatformIoWindow::default(); MAX_PLATFORM_IO],
        io_count: 0,
        irq: None,
        present: None,
        i2c: None,
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

impl ClaimSink for TestClaims {
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
    let drivers: &[&'static PlatformDriverEntry] = &[&T1_DRV];
    if run(&devices, drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    match claims.owner(0) {
        Some("t1-kbd") => pass!(),
        other => fail!("expected t1-kbd to bind, got {:?}", other),
    }
}

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
    let drivers: &[&'static PlatformDriverEntry] = &[&T2_GENERIC_DRV, &T2_SPECIFIC_DRV];
    if run(&devices, drivers, &claims).is_err() {
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
    let drivers: &[&'static PlatformDriverEntry] = &[&T3_A, &T3_B];
    if run(&devices, drivers, &claims).is_err() {
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
    let drivers: &[&'static PlatformDriverEntry] = &[&T4_DRV];
    if run(&devices, drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    if claims.owner(0).is_some() {
        return fail!("device must stay unbound after probe Err");
    }
    pass!()
}

static T5_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

fn t5_defer_probe(_b: &mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError> {
    if T5_ATTEMPTS.fetch_add(1, Ordering::Relaxed) == 0 {
        Err(PlatformProbeError::Deferred)
    } else {
        Ok(ProbeOutcome::Bound)
    }
}

static T5_DEFER: PlatformDriverEntry = PlatformDriverEntry {
    name: "t5-defer",
    match_table: &[PlatformMatch::HidCid(b"PNP0303")],
    priority: 128,
    fallback: None,
    probe: t5_defer_probe,
};

/// The platform bus inherited the deferred-retry pass from the generic
/// matchmaker. No in-tree platform driver defers today — this covers the
/// mechanism, not a consumer.
pub fn test_platform_deferred_then_bound() -> TestResult {
    T5_ATTEMPTS.store(0, Ordering::Relaxed);
    let devices = [device(b"PNP0303")];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    let drivers: &[&'static PlatformDriverEntry] = &[&T5_DEFER];
    if run(&devices, drivers, &claims).is_err() {
        return fail!("matchmake OOM");
    }
    if claims.owner(0) != Some("t5-defer") {
        return fail!("deferred driver should bind on the retry pass");
    }
    match T5_ATTEMPTS.load(Ordering::Relaxed) {
        2 => pass!(),
        n => fail!("expected 2 probe attempts (defer then bind), got {}", n),
    }
}

slopos_testing::stest!(
    name = test_platform_binding_records_claim,
    suite = platform_binding
);
slopos_testing::stest!(
    name = test_platform_deferred_then_bound,
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
