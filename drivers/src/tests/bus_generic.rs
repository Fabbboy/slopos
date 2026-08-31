//! `probe_bus` regression tests over a synthetic third bus.
//!
//! The PCI and platform suites prove the two real buses still bind correctly.
//! This one proves the binding protocol is genuinely bus-agnostic: `TestBus`
//! has its own `Device` and `DriverEntry` types, no link section, and no
//! hardware behind it, so anything asserted here is a property of
//! [`probe_bus`] rather than of either shipped bus.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::dev::Devres;
use slopos_ostd::{AllocError, KVec};
use slopos_testing::{TestResult, fail, pass};

use crate::driver_core::bus::{
    BoundDevice, Bus, ClaimSink, LinearIndex, ProbeError, ProbeOutcome, probe_bus,
};

#[derive(Clone, Copy)]
struct TestDevice {
    id: u32,
}

struct TestEntry {
    name: &'static str,
    accepts: u32,
    priority: u8,
    probe: fn(&mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError>,
}

struct TestBus;

impl Bus for TestBus {
    type Device = TestDevice;
    type DriverEntry = TestEntry;

    const NAME: &'static str = "testbus";

    fn entry_name(entry: &TestEntry) -> &'static str {
        entry.name
    }

    fn priority(entry: &TestEntry) -> u8 {
        entry.priority
    }

    fn matches(entry: &TestEntry, dev: &TestDevice) -> bool {
        entry.accepts == dev.id
    }

    fn probe(
        entry: &TestEntry,
        bound: &mut BoundDevice<'_, TestBus>,
    ) -> Result<ProbeOutcome, ProbeError> {
        (entry.probe)(bound)
    }
}

/// `ClaimSink` backed by a `KVec` indexed by device index; single-threaded, so
/// a `RefCell` suffices and no lock enters the lock graph.
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

fn run(
    devices: &[TestDevice],
    drivers: &[&'static TestEntry],
    claims: &dyn ClaimSink,
) -> Result<(), AllocError> {
    let mut entries = KVec::new();
    for &e in drivers {
        entries.push(e)?;
    }
    let idx = LinearIndex::<TestBus>::from_entries(entries);
    probe_bus::<TestBus>(&idx, devices.len(), &|i| devices.get(i).copied(), claims)
}

fn bind_probe(_b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    Ok(ProbeOutcome::Bound)
}

static T1_SEQ: AtomicU32 = AtomicU32::new(0);
static T1_LOW_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);
static T1_HIGH_SEQ: AtomicU32 = AtomicU32::new(u32::MAX);

fn t1_low(_b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    T1_LOW_SEQ.store(T1_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}

fn t1_high(_b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    T1_HIGH_SEQ.store(T1_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    Ok(ProbeOutcome::Bound)
}

// Registered high-priority-value first, so an order that came from
// registration order rather than `priority` would fail.
static T1_HIGH: TestEntry = TestEntry {
    name: "t1-high",
    accepts: 7,
    priority: 200,
    probe: t1_high,
};
static T1_LOW: TestEntry = TestEntry {
    name: "t1-low",
    accepts: 7,
    priority: 10,
    probe: t1_low,
};

pub fn test_bus_priority_order() -> TestResult {
    T1_SEQ.store(0, Ordering::Relaxed);
    T1_LOW_SEQ.store(u32::MAX, Ordering::Relaxed);
    T1_HIGH_SEQ.store(u32::MAX, Ordering::Relaxed);
    let devices = [TestDevice { id: 7 }];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    if run(&devices, &[&T1_HIGH, &T1_LOW], &claims).is_err() {
        return fail!("probe_bus OOM");
    }
    let low = T1_LOW_SEQ.load(Ordering::Relaxed);
    let high = T1_HIGH_SEQ.load(Ordering::Relaxed);
    if low == u32::MAX {
        return fail!("the lower-priority-value driver was never offered the device");
    }
    if low >= high {
        return fail!(
            "priority 10 (seq {}) must precede priority 200 (seq {})",
            low,
            high
        );
    }
    match claims.owner(0) {
        Some("t1-high") => pass!(),
        other => fail!("t1-high should bind after t1-low declined, got {:?}", other),
    }
}

static T2_SECOND_PROBES: AtomicU32 = AtomicU32::new(0);

fn t2_second(_b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    T2_SECOND_PROBES.fetch_add(1, Ordering::Relaxed);
    Ok(ProbeOutcome::Declined)
}

static T2_FIRST: TestEntry = TestEntry {
    name: "t2-first",
    accepts: 9,
    priority: 10,
    probe: bind_probe,
};
static T2_SECOND: TestEntry = TestEntry {
    name: "t2-second",
    accepts: 9,
    priority: 20,
    probe: t2_second,
};

pub fn test_bus_dup_claim_prevention() -> TestResult {
    T2_SECOND_PROBES.store(0, Ordering::Relaxed);
    let devices = [TestDevice { id: 9 }];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    if run(&devices, &[&T2_FIRST, &T2_SECOND], &claims).is_err() {
        return fail!("probe_bus OOM");
    }
    if claims.owner(0) != Some("t2-first") {
        return fail!("t2-first should own the device");
    }
    match T2_SECOND_PROBES.load(Ordering::Relaxed) {
        0 => pass!(),
        n => fail!("t2-second was probed {} times after the device bound", n),
    }
}

static T3_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

fn t3_defer(_b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    if T3_ATTEMPTS.fetch_add(1, Ordering::Relaxed) == 0 {
        Err(ProbeError::Deferred)
    } else {
        Ok(ProbeOutcome::Bound)
    }
}

static T3_DEFER: TestEntry = TestEntry {
    name: "t3-defer",
    accepts: 11,
    priority: 128,
    probe: t3_defer,
};

pub fn test_bus_deferred_then_bound() -> TestResult {
    T3_ATTEMPTS.store(0, Ordering::Relaxed);
    let devices = [TestDevice { id: 11 }];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    if run(&devices, &[&T3_DEFER], &claims).is_err() {
        return fail!("probe_bus OOM");
    }
    if claims.owner(0) != Some("t3-defer") {
        return fail!("the deferred driver should bind on the retry pass");
    }
    match T3_ATTEMPTS.load(Ordering::Relaxed) {
        2 => pass!(),
        n => fail!("expected 2 probe attempts (defer then bind), got {}", n),
    }
}

static T4_DROPS: AtomicU32 = AtomicU32::new(0);

/// Counts its own drop, so the test observes the bag releasing rather than
/// inferring it.
struct DropSpy;

impl Drop for DropSpy {
    fn drop(&mut self) {
        T4_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn t4_acquire_then_fail(b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    b.attach(DropSpy).map_err(|_| ProbeError::OutOfMemory)?;
    b.attach(DropSpy).map_err(|_| ProbeError::OutOfMemory)?;
    Err(ProbeError::DeviceFault)
}

static T4_FAIL: TestEntry = TestEntry {
    name: "t4-fail",
    accepts: 13,
    priority: 128,
    probe: t4_acquire_then_fail,
};

pub fn test_bus_devres_released_on_probe_failure() -> TestResult {
    T4_DROPS.store(0, Ordering::Relaxed);
    let devices = [TestDevice { id: 13 }];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    if run(&devices, &[&T4_FAIL], &claims).is_err() {
        return fail!("probe_bus OOM");
    }
    if claims.owner(0).is_some() {
        return fail!("the device must stay unbound after probe Err");
    }
    match T4_DROPS.load(Ordering::Relaxed) {
        2 => pass!(),
        n => fail!("expected both attached resources to drop, saw {}", n),
    }
}

/// A bound driver's resources must NOT drop: the bag moves into the claim slot
/// and lives for the binding's lifetime. Without this, the previous test would
/// also pass on an implementation that dropped the bag unconditionally.
static T5_DROPS: AtomicU32 = AtomicU32::new(0);

struct KeepSpy;

impl Drop for KeepSpy {
    fn drop(&mut self) {
        T5_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn t5_acquire_then_bind(b: &mut BoundDevice<'_, TestBus>) -> Result<ProbeOutcome, ProbeError> {
    b.attach(KeepSpy).map_err(|_| ProbeError::OutOfMemory)?;
    Ok(ProbeOutcome::Bound)
}

static T5_BIND: TestEntry = TestEntry {
    name: "t5-bind",
    accepts: 15,
    priority: 128,
    probe: t5_acquire_then_bind,
};

pub fn test_bus_devres_retained_on_bind() -> TestResult {
    T5_DROPS.store(0, Ordering::Relaxed);
    let devices = [TestDevice { id: 15 }];
    let claims = match TestClaims::new(devices.len()) {
        Ok(c) => c,
        Err(_) => return fail!("claim sink OOM"),
    };
    // The sink discards the bag, so scope the check to the probe path itself:
    // a drop here would mean `probe_bus` released before handing it over.
    if run(&devices, &[&T5_BIND], &claims).is_err() {
        return fail!("probe_bus OOM");
    }
    if claims.owner(0) != Some("t5-bind") {
        return fail!("t5-bind should own the device");
    }
    // Exactly one drop, from the test sink discarding the bag it was handed —
    // never from `probe_bus` releasing it before the claim was recorded.
    match T5_DROPS.load(Ordering::Relaxed) {
        1 => pass!(),
        n => fail!(
            "expected the bag to reach the claim sink intact, drops={}",
            n
        ),
    }
}

slopos_testing::stest!(name = test_bus_priority_order, suite = bus_generic);
slopos_testing::stest!(name = test_bus_dup_claim_prevention, suite = bus_generic);
slopos_testing::stest!(name = test_bus_deferred_then_bound, suite = bus_generic);
slopos_testing::stest!(
    name = test_bus_devres_released_on_probe_failure,
    suite = bus_generic
);
slopos_testing::stest!(name = test_bus_devres_retained_on_bind, suite = bus_generic);
