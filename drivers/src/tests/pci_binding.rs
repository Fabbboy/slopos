//! PCI declarative-match + binding/claim regression tests.
//!
//! Exercises the registry's matchmaker core (`MatchIndex` + `matchmake`) over
//! synthetic drivers and synthetic devices, with a heap-backed claim sink — so
//! nothing touches real hardware or the live `CLAIMED_BY` table. Covers index
//! correctness (priority-sorted, deduped candidates), one-driver-per-device
//! binding, dup-claim prevention, specific-beats-generic priority ordering, and
//! the deferred-probe substrate.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::{AllocError, KVec};
use slopos_testing::{TestResult, fail, pass};

use crate::pci::{
    ClaimSink, MatchIndex, PciDeviceInfo, PciDriverEntry, PciMatch, PciProbeError, ProbeOutcome,
    matchmake,
};

// ---------------------------------------------------------------------------
// Synthetic device + heap-backed claim sink.
// ---------------------------------------------------------------------------

/// A synthetic device with just the fields matching reads.
fn device(vendor: u16, dev: u16, class: u8, subclass: u8) -> PciDeviceInfo {
    let mut d = PciDeviceInfo::zeroed();
    d.vendor_id = vendor;
    d.device_id = dev;
    d.class_code = class;
    d.subclass = subclass;
    d
}

/// `ClaimSink` backed by a heap `KVec`, indexed by device index. Single-
/// threaded, so a `RefCell` suffices — no lock and no lock-graph interaction.
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

    fn record(&self, dev_idx: usize, name: &'static str) {
        if let Some(slot) = self.bound.borrow_mut().get_mut(dev_idx) {
            *slot = Some(name);
        }
    }
}

fn index_of(drivers: &[&'static PciDriverEntry]) -> Result<MatchIndex, AllocError> {
    let mut entries = KVec::new();
    for &e in drivers {
        entries.push(e)?;
    }
    MatchIndex::build_from(entries)
}

// ---------------------------------------------------------------------------
// Probe stubs (no hardware). Each uses static counters reset at test start.
// ---------------------------------------------------------------------------

fn decline_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    Ok(ProbeOutcome::Declined)
}

fn bind_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    Ok(ProbeOutcome::Bound)
}

fn always_true(_info: &PciDeviceInfo) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Test 1: index correctness — candidates priority-sorted and deduplicated.
// ---------------------------------------------------------------------------

static T1_VD: PciDriverEntry = PciDriverEntry {
    name: "t1-vd",
    match_table: &[PciMatch::VendorDevice {
        vendor: 0x1234,
        device: 0x5678,
    }],
    fallback: None,
    priority: 50,
    probe: decline_probe,
};
static T1_VC: PciDriverEntry = PciDriverEntry {
    name: "t1-vc",
    match_table: &[PciMatch::VendorClass {
        vendor: 0x1234,
        class: 0x03,
    }],
    fallback: None,
    priority: 20,
    probe: decline_probe,
};
static T1_CS: PciDriverEntry = PciDriverEntry {
    name: "t1-cs",
    match_table: &[PciMatch::ClassSubclass {
        class: 0x03,
        subclass: 0x80,
    }],
    fallback: None,
    priority: 10,
    probe: decline_probe,
};
static T1_CO: PciDriverEntry = PciDriverEntry {
    name: "t1-co",
    match_table: &[PciMatch::ClassOnly { class: 0x03 }],
    fallback: None,
    priority: 200,
    probe: decline_probe,
};
static T1_FB: PciDriverEntry = PciDriverEntry {
    name: "t1-fb",
    match_table: &[],
    fallback: Some(always_true),
    priority: 30,
    probe: decline_probe,
};
// Two indexable rules for the SAME driver: must still appear once.
static T1_DUP: PciDriverEntry = PciDriverEntry {
    name: "t1-dup",
    match_table: &[
        PciMatch::VendorDevice {
            vendor: 0x1234,
            device: 0x5678,
        },
        PciMatch::VendorClass {
            vendor: 0x1234,
            class: 0x03,
        },
    ],
    fallback: None,
    priority: 5,
    probe: decline_probe,
};

pub fn test_candidates_priority_sorted_and_deduped() -> TestResult {
    // Link order = build order: VD=0, VC=1, CS=2, CO=3, FB=4, DUP=5.
    let idx = match index_of(&[&T1_VD, &T1_VC, &T1_CS, &T1_CO, &T1_FB, &T1_DUP]) {
        Ok(i) => i,
        Err(_) => return fail!("index build out of memory"),
    };
    let dev = device(0x1234, 0x5678, 0x03, 0x80);
    let mut out = KVec::new();
    if idx.candidates_for(&dev, &mut out).is_err() {
        return fail!("candidates_for out of memory");
    }
    // Sorted by (priority, link-index): DUP(5,prio5), CS(2,prio10), VC(1,prio20),
    // FB(4,prio30), VD(0,prio50), CO(3,prio200). DUP appears exactly once.
    let expected: &[u16] = &[5, 2, 1, 4, 0, 3];
    if out.as_slice() != expected {
        return fail!(
            "candidate order wrong: got {:?}, expected {:?}",
            out.as_slice(),
            expected
        );
    }
    pass!()
}

// ---------------------------------------------------------------------------
// Test 2: one matching driver binds the device.
// ---------------------------------------------------------------------------

static T2_DRV: PciDriverEntry = PciDriverEntry {
    name: "t2-blk",
    match_table: &[PciMatch::VendorDevice {
        vendor: 0x1af4,
        device: 0x1042,
    }],
    fallback: None,
    priority: 128,
    probe: bind_probe,
};

pub fn test_binding_records_claim() -> TestResult {
    let idx = match index_of(&[&T2_DRV]) {
        Ok(i) => i,
        Err(_) => return fail!("index build out of memory"),
    };
    let devices = [device(0x1af4, 0x1042, 0x01, 0x00)];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink out of memory"),
    };
    if matchmake(&idx, devices.len(), &|i| devices.get(i).copied(), &claims).is_err() {
        return fail!("matchmake out of memory");
    }
    match claims.owner(0) {
        Some("t2-blk") => pass!(),
        other => fail!("device should be bound by t2-blk, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test 3: a claimed device is not offered to a lower-priority driver.
// ---------------------------------------------------------------------------

static T3_B_PROBES: AtomicU32 = AtomicU32::new(0);

fn t3_b_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    T3_B_PROBES.fetch_add(1, Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}

static T3_A: PciDriverEntry = PciDriverEntry {
    name: "t3-a",
    match_table: &[PciMatch::VendorDevice {
        vendor: 0x9000,
        device: 0x0001,
    }],
    fallback: None,
    priority: 10,
    probe: bind_probe,
};
static T3_B: PciDriverEntry = PciDriverEntry {
    name: "t3-b",
    match_table: &[PciMatch::ClassOnly { class: 0x40 }],
    fallback: None,
    priority: 20,
    probe: t3_b_probe,
};

pub fn test_dup_claim_prevention() -> TestResult {
    T3_B_PROBES.store(0, Ordering::Relaxed);
    let idx = match index_of(&[&T3_A, &T3_B]) {
        Ok(i) => i,
        Err(_) => return fail!("index build out of memory"),
    };
    // Device matches BOTH A (vendor/device) and B (class).
    let devices = [device(0x9000, 0x0001, 0x40, 0x00)];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink out of memory"),
    };
    if matchmake(&idx, devices.len(), &|i| devices.get(i).copied(), &claims).is_err() {
        return fail!("matchmake out of memory");
    }
    if claims.owner(0) != Some("t3-a") {
        return fail!("higher-priority t3-a should own the device");
    }
    match T3_B_PROBES.load(Ordering::Relaxed) {
        0 => pass!(),
        n => fail!("t3-b was probed {} times after device was claimed", n),
    }
}

// ---------------------------------------------------------------------------
// Test 4: specific (lower priority) is offered before generic.
// ---------------------------------------------------------------------------

static T4_SEQ: AtomicU32 = AtomicU32::new(0);
static T4_SPECIFIC_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);
static T4_GENERIC_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);

fn t4_specific_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    T4_SPECIFIC_SEQ.store(T4_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}

fn t4_generic_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    T4_GENERIC_SEQ.store(T4_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Bound)
}

// Registered generic-first to prove ordering comes from priority, not link order.
static T4_GENERIC: PciDriverEntry = PciDriverEntry {
    name: "t4-generic",
    match_table: &[PciMatch::ClassOnly { class: 0x0c }],
    fallback: None,
    priority: 200,
    probe: t4_generic_probe,
};
static T4_SPECIFIC: PciDriverEntry = PciDriverEntry {
    name: "t4-specific",
    match_table: &[PciMatch::VendorDevice {
        vendor: 0x8086,
        device: 0xabcd,
    }],
    fallback: None,
    priority: 10,
    probe: t4_specific_probe,
};

pub fn test_specific_beats_generic() -> TestResult {
    T4_SEQ.store(0, Ordering::Relaxed);
    T4_SPECIFIC_SEQ.store(u32::MAX, Ordering::Relaxed);
    T4_GENERIC_SEQ.store(u32::MAX, Ordering::Relaxed);
    let idx = match index_of(&[&T4_GENERIC, &T4_SPECIFIC]) {
        Ok(i) => i,
        Err(_) => return fail!("index build out of memory"),
    };
    let devices = [device(0x8086, 0xabcd, 0x0c, 0x80)];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink out of memory"),
    };
    if matchmake(&idx, devices.len(), &|i| devices.get(i).copied(), &claims).is_err() {
        return fail!("matchmake out of memory");
    }
    let spec = T4_SPECIFIC_SEQ.load(Ordering::Relaxed);
    let generic = T4_GENERIC_SEQ.load(Ordering::Relaxed);
    if spec == u32::MAX {
        return fail!("specific driver was never offered the device");
    }
    if spec >= generic {
        return fail!(
            "specific (seq {}) must be offered before generic (seq {})",
            spec,
            generic
        );
    }
    match claims.owner(0) {
        Some("t4-generic") => pass!(),
        other => fail!(
            "generic should bind after specific declined, got {:?}",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Test 5: deferred substrate — Deferred on round 1, Bound on the retry pass.
// ---------------------------------------------------------------------------

static T5_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

fn t5_defer_probe(_info: &PciDeviceInfo) -> Result<ProbeOutcome, PciProbeError> {
    let n = T5_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        Err(PciProbeError::Deferred)
    } else {
        Ok(ProbeOutcome::Bound)
    }
}

static T5_DEFER: PciDriverEntry = PciDriverEntry {
    name: "t5-defer",
    match_table: &[PciMatch::VendorDevice {
        vendor: 0x7777,
        device: 0x0042,
    }],
    fallback: None,
    priority: 128,
    probe: t5_defer_probe,
};

pub fn test_deferred_then_bound() -> TestResult {
    T5_ATTEMPTS.store(0, Ordering::Relaxed);
    let idx = match index_of(&[&T5_DEFER]) {
        Ok(i) => i,
        Err(_) => return fail!("index build out of memory"),
    };
    let devices = [device(0x7777, 0x0042, 0x02, 0x00)];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink out of memory"),
    };
    if matchmake(&idx, devices.len(), &|i| devices.get(i).copied(), &claims).is_err() {
        return fail!("matchmake out of memory");
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
    name = test_candidates_priority_sorted_and_deduped,
    suite = pci_binding
);
slopos_testing::stest!(name = test_binding_records_claim, suite = pci_binding);
slopos_testing::stest!(name = test_dup_claim_prevention, suite = pci_binding);
slopos_testing::stest!(name = test_specific_beats_generic, suite = pci_binding);
slopos_testing::stest!(name = test_deferred_then_bound, suite = pci_binding);
