//! Focused AML namespace walk for **I²C-HID device enumeration**.
//!
//! Not a general AML interpreter (see `interp` for the deliberately narrow
//! scope). It answers one question from the DSDT: *which I²C controller +
//! slave address + HID descriptor register is the touchpad at?* — by running
//! the device's own `_STA`/`_INI` and reading back the resource template
//! they patch, with no per-machine constants.
//!
//! Robustness is a hard requirement: it runs at boot on whatever firmware is
//! present. Every step returns `Option` and never panics; a DSDT it can't
//! fully parse yields "no touchpad found" and boot proceeds.

pub mod interp;
pub mod object;
pub mod parse;
pub mod resource;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::{KBTreeMap, KVec, klog_info};

use crate::fadt::Fadt;
use crate::tables::{self, AcpiTables, SdtHeader};
use interp::{FieldLoc, Interp, Overlay};
use object::{AmlVal, bytes_from_slice, nameseg_key};
use parse::*;

/// Physical-memory backend for the interpreter's `SystemMemory` field
/// reads. Abstracted so the walker is testable against scripted memory.
pub trait AmlHost {
    /// Fill `out` with `SystemMemory` bytes starting at physical `phys`
    /// (zero-filled on failure).
    fn read_phys(&self, phys: u64, out: &mut [u8]);
}

/// Default host: reads physical memory through the HHDM mapping used for
/// all other ACPI table access.
pub struct HhdmHost;

impl AmlHost for HhdmHost {
    fn read_phys(&self, phys: u64, out: &mut [u8]) {
        // Default to zero; we never leave `out` partially garbage and we
        // never fault, regardless of whether the region is mappable.
        for b in out.iter_mut() {
            *b = 0;
        }
        if out.is_empty() {
            return;
        }
        let Some(off) = slopos_ostd::boot::hhdm::hhdm_offset() else {
            return;
        };
        let Some(virt) = phys.checked_add(off) else {
            return;
        };
        let Some(end) = virt.checked_add(out.len() as u64) else {
            return;
        };
        // The kernel HHDM covers usable RAM only; a SystemMemory
        // OperationRegion can point at a BIOS-NVS / reserved area that isn't
        // mapped. Map any missing page on demand (read-only) and bail to
        // zero if that fails — never dereference unmapped memory.
        let mut page = virt & !0xfff;
        while page < end {
            if slopos_mm::paging::is_mapped(VirtAddr::new(page)) == 0 {
                let _ = slopos_mm::paging::map_page_4kb(
                    VirtAddr::new(page),
                    PhysAddr::new(page.wrapping_sub(off)),
                    slopos_mm::paging_defs::PageFlags::KERNEL_RO.bits(),
                );
                if slopos_mm::paging::is_mapped(VirtAddr::new(page)) == 0 {
                    return;
                }
            }
            page = page.wrapping_add(0x1000);
        }
        if let Some(src) =
            slopos_ostd::boot::handoff::acpi_region_bytes(PhysAddr::new(phys), out.len())
        {
            let n = out.len().min(src.len());
            out[..n].copy_from_slice(&src[..n]);
        }
    }
}

/// The enumeration result for one I²C-HID device.
#[derive(Clone, Copy, Debug)]
pub struct AcpiI2cHid {
    /// I²C controller index parsed from the resource's controller path
    /// (`\_SB.PC00.I2Cn` → `n`).
    pub controller_index: u8,
    /// 7-bit slave address.
    pub slave_addr: u16,
    /// HID descriptor register (the `_DSM` fn-1 value, i.e. `HID2`).
    pub hid_desc_reg: u16,
    /// Bus speed in Hz.
    pub speed_hz: u32,
    /// GpioInt pin number from the device's `_CRS` (the controller-global
    /// GPIO line), if it declares one. `None` ⇒ no GPIO interrupt
    /// (driver falls back to polling).
    pub gpio_int_pin: Option<u16>,
    /// GpioInt trigger: `true` = edge, `false` = level.
    pub gpio_int_edge: bool,
    /// GpioInt polarity: `true` = active-low.
    pub gpio_int_active_low: bool,
}

/// EISA-packed id for `"PNP0C50"` (the generic I²C-HID `_CID`).
const EISAID_PNP0C50: u64 = 0x500C_D041;

const HEADER_LEN: usize = core::mem::size_of::<SdtHeader>();

/// Scan the ACPI namespace for the first present I²C-HID device and
/// resolve its bus location. Returns `None` if the tables are missing or no
/// such device is found. `debug` emits step-by-step `klog` diagnostics.
pub fn scan_i2c_hid(tables: &AcpiTables, host: &dyn AmlHost, debug: bool) -> Option<AcpiI2cHid> {
    let facp = tables.find_table(b"FACP")?;
    let fadt = Fadt::parse(facp.raw())?;
    let dsdt = tables::table_bytes_at(fadt.dsdt_phys)?;
    let dsdt_aml = dsdt.get(HEADER_LEN..)?;

    // Collect the SSDT AML bodies (for method arg-counts / fields).
    let mut ssdts: KVec<&[u8]> = KVec::new();
    tables.find_map_raw(b"SSDT", |bytes| {
        if let Some(aml) = bytes.get(HEADER_LEN..) {
            let _ = ssdts.push(aml);
        }
        None::<()>
    });

    scan_blobs(dsdt_aml, ssdts.as_slice(), host, debug)
}

/// Core of [`scan_i2c_hid`] operating on raw AML bodies (DSDT + SSDTs, each
/// already stripped of its 36-byte SDT header). Exposed so it can be driven
/// by a captured-DSDT fixture in the test harness.
pub fn scan_blobs(
    dsdt_aml: &[u8],
    ssdts: &[&[u8]],
    host: &dyn AmlHost,
    debug: bool,
) -> Option<AcpiI2cHid> {
    // Build the global symbol index (method arg-counts + SystemMemory
    // fields) across the DSDT and every SSDT.
    let mut idx = Index::new();
    index_blob(dsdt_aml, &mut idx);
    for ssdt in ssdts {
        index_blob(ssdt, &mut idx);
    }
    idx.resolve_fields();

    if debug {
        klog_info!(
            "aml: idx methods={} bodies={} names={} | GPCL={} GNUM={} GINF={} GNMB={} GGRP={}",
            idx.methods.len(),
            idx.method_bodies.len(),
            idx.names.len(),
            idx.names.get(&nameseg_key(b"GPCL")).is_some(),
            idx.method_bodies.get(&nameseg_key(b"GNUM")).is_some(),
            idx.method_bodies.get(&nameseg_key(b"GINF")).is_some(),
            idx.method_bodies.get(&nameseg_key(b"GNMB")).is_some(),
            idx.method_bodies.get(&nameseg_key(b"GGRP")).is_some(),
        );
    }

    // Find candidate devices in the DSDT and process the first I²C-HID one.
    let devices = collect_devices(dsdt_aml);
    let mut candidates = 0usize;
    for dev in devices.iter() {
        let members = collect_members(dsdt_aml, *dev);
        if !members.is_i2c_hid {
            continue;
        }
        candidates += 1;
        if let Some(found) = process_device(dsdt_aml, &members, &idx, host, debug) {
            return Some(found);
        }
    }
    if debug {
        klog_info!(
            "aml: walked {} devices, {} methods, {} fields, {} PNP0C50 candidates (none usable)",
            devices.len(),
            idx.methods.len(),
            idx.fields.len(),
            candidates
        );
    }
    None
}

// ---------------------------------------------------------------------------
// Global symbol index
// ---------------------------------------------------------------------------

struct Index {
    methods: KBTreeMap<u32, u8>,
    /// Method name → body term-list range (for evaluating calls).
    method_bodies: KBTreeMap<u32, Range>,
    /// Global `Name` → the position of its data object (for resolving
    /// `Package` tables like a GPIO pad-info array).
    names: KBTreeMap<u32, usize>,
    regions: KBTreeMap<u32, (u8, u64)>,
    fields: KBTreeMap<u32, FieldLoc>,
    pending: KVec<(u32, u32, u32, u32)>, // (region_seg, field_seg, bit_off, bit_width)
}

impl Index {
    fn new() -> Self {
        Self {
            methods: KBTreeMap::new(),
            method_bodies: KBTreeMap::new(),
            names: KBTreeMap::new(),
            regions: KBTreeMap::new(),
            fields: KBTreeMap::new(),
            pending: KVec::new(),
        }
    }

    fn resolve_fields(&mut self) {
        for &(region, field, off, width) in self.pending.iter() {
            if let Some(&(space, base)) = self.regions.get(&region) {
                self.fields.insert(
                    field,
                    FieldLoc {
                        region_base: base,
                        region_space: space,
                        bit_offset: off,
                        bit_width: width,
                    },
                );
            }
        }
    }
}

struct IndexVisitor<'a>(&'a mut Index);

impl Visitor for IndexVisitor<'_> {
    fn method(&mut self, seg: [u8; 4], argc: u8, body: Range) {
        let key = nameseg_key(&seg);
        self.0.methods.insert(key, argc);
        self.0.method_bodies.insert(key, body);
    }
    fn external_method(&mut self, seg: [u8; 4], argc: u8) {
        self.0.methods.entry(nameseg_key(&seg)).or_insert(argc);
    }
    fn name(&mut self, seg: [u8; 4], value: Range) {
        // First occurrence wins; we only resolve uniquely-named global
        // tables (e.g. a GPIO pad-info `Package`).
        self.0.names.entry(nameseg_key(&seg)).or_insert(value.start);
    }
    fn op_region(&mut self, seg: [u8; 4], space: u8, base: u64, _len: u64) {
        self.0.regions.insert(nameseg_key(&seg), (space, base));
    }
    fn field(&mut self, region: [u8; 4], elem: &FieldElem) {
        let _ = self.0.pending.push((
            nameseg_key(&region),
            nameseg_key(&elem.seg),
            elem.bit_offset,
            elem.bit_width,
        ));
    }
}

fn index_blob(aml: &[u8], idx: &mut Index) {
    let mut v = IndexVisitor(idx);
    walk_terms(aml, 0, aml.len(), &mut v);
}

// ---------------------------------------------------------------------------
// Device discovery + member collection
// ---------------------------------------------------------------------------

struct DeviceCollector {
    devices: KVec<Range>,
}

impl Visitor for DeviceCollector {
    fn enter_device(&mut self, _seg: [u8; 4], body: Range) -> bool {
        let _ = self.devices.push(body);
        true // descend to find nested devices too
    }
}

fn collect_devices(aml: &[u8]) -> KVec<Range> {
    let mut c = DeviceCollector {
        devices: KVec::new(),
    };
    walk_terms(aml, 0, aml.len(), &mut c);
    c.devices
}

struct Members<'a> {
    aml: &'a [u8],
    is_i2c_hid: bool,
    sta: Option<Range>,
    ini: Option<Range>,
    names: KVec<(u32, Range)>,
    overlays: KVec<(u32, Overlay)>,
}

impl Visitor for Members<'_> {
    fn name(&mut self, seg: [u8; 4], value: Range) {
        let key = nameseg_key(&seg);
        if key == nameseg_key(b"_CID") || key == nameseg_key(b"_HID") {
            if value_is_pnp0c50(self.aml, value.start) {
                self.is_i2c_hid = true;
            }
        }
        let _ = self.names.push((key, value));
    }
    fn method(&mut self, seg: [u8; 4], _argc: u8, body: Range) {
        let key = nameseg_key(&seg);
        if key == nameseg_key(b"_STA") {
            self.sta = Some(body);
        } else if key == nameseg_key(b"_INI") {
            self.ini = Some(body);
        }
    }
    fn create_field(&mut self, source: [u8; 4], byte_index: u64, width_bytes: u8, name: [u8; 4]) {
        let _ = self.overlays.push((
            nameseg_key(&name),
            Overlay {
                source: nameseg_key(&source),
                byte_index,
                width: width_bytes,
            },
        ));
    }
    fn enter_device(&mut self, _seg: [u8; 4], _body: Range) -> bool {
        false // direct members only
    }
}

fn collect_members<'a>(aml: &'a [u8], dev: Range) -> Members<'a> {
    let mut m = Members {
        aml,
        is_i2c_hid: false,
        sta: None,
        ini: None,
        names: KVec::new(),
        overlays: KVec::new(),
    };
    walk_terms(aml, dev.start, dev.end, &mut m);
    m
}

fn process_device(
    aml: &[u8],
    members: &Members<'_>,
    idx: &Index,
    host: &dyn AmlHost,
    debug: bool,
) -> Option<AcpiI2cHid> {
    let mut interp = Interp::new(
        aml,
        &idx.fields,
        &idx.methods,
        &idx.method_bodies,
        &idx.names,
        host,
    );
    if debug {
        klog_info!(
            "aml: I2C-HID candidate (names={}, overlays={}, _STA={}, _INI={})",
            members.names.len(),
            members.overlays.len(),
            members.sta.is_some(),
            members.ini.is_some()
        );
    }

    // Seed device-local objects and Create*Field overlays.
    for &(seg, range) in members.names.iter() {
        if let Some(val) = parse_value(aml, range.start) {
            interp.locals.insert(seg, val);
        }
    }
    for &(seg, ov) in members.overlays.iter() {
        interp.overlays.insert(seg, ov);
    }

    // Presence: run _STA if present; absent _STA ⇒ present.
    if let Some(sta) = members.sta {
        let present = interp
            .run(sta.start, sta.end)
            .map(|v| v.as_int() & 0x01 != 0)
            .unwrap_or(true);
        if !present {
            if debug {
                klog_info!("aml: candidate _STA reports absent");
            }
            return None;
        }
    }

    // Configure: run _INI (patches the slave address into the template).
    if let Some(ini) = members.ini {
        interp.run(ini.start, ini.end);
    }

    if debug {
        let gpcl = interp.resolve_pkg_len_for_test(b"GPCL");
        let ginf = interp.invoke_for_test(b"GINF", &[8, 8]);
        let gnum = interp.invoke_for_test(b"GNUM", &[0x0908_0011]);
        klog_info!(
            "aml: GPCL_pkg_len={:?} GINF(8,8)={:?} GNUM(0x09080011)={:?} (expect 18 / 160 / 177)",
            gpcl,
            ginf,
            gnum
        );
    }

    // Find the buffer holding an I²C serial-bus descriptor (patched by `_INI`).
    let mut found_i2c: Option<resource::I2cResource> = None;
    for (_seg, val) in interp.locals.iter() {
        if let AmlVal::Buf(b) = val {
            if let Some(r) = resource::parse_i2c(b.as_slice()) {
                found_i2c = Some(r);
                break;
            }
        }
    }
    let Some(i2c) = found_i2c else {
        if debug {
            klog_info!("aml: candidate has no I2cSerialBus in its resource templates");
        }
        return None;
    };
    let Some(controller_index) = controller_index_from_path(i2c.controller.as_slice()) else {
        if debug {
            klog_info!("aml: could not parse I2C controller index from resource path");
        }
        return None;
    };

    // HID descriptor register = HID2 (the _DSM fn-1 value); default 0x0001.
    let hid_desc_reg = interp
        .locals
        .get(&nameseg_key(b"HID2"))
        .map(|v| v.as_int() as u16)
        .filter(|&v| v != 0)
        .unwrap_or(0x0001);

    // GpioInt connection (interrupt pin + polarity), if the device declares
    // one in the same resource templates. Drives the interrupt-driven path;
    // its absence makes the touchpad fall back to polling.
    let mut gpio: Option<resource::GpioIntResource> = None;
    for (_seg, val) in interp.locals.iter() {
        if let AmlVal::Buf(b) = val {
            if let Some(g) = resource::parse_gpio_int(b.as_slice()) {
                gpio = Some(g);
                break;
            }
        }
    }

    Some(AcpiI2cHid {
        controller_index,
        slave_addr: i2c.slave_addr,
        hid_desc_reg,
        speed_hz: i2c.speed_hz,
        gpio_int_pin: gpio.as_ref().map(|g| g.pin),
        gpio_int_edge: gpio.as_ref().map(|g| g.edge).unwrap_or(false),
        gpio_int_active_low: gpio.as_ref().map(|g| g.active_low).unwrap_or(true),
    })
}

/// Test-only: index `aml`, then invoke `method(args)` and return the integer
/// it returns. Exercises the method-call / arithmetic / package machinery.
#[doc(hidden)]
pub fn eval_method_u64_for_test(
    aml: &[u8],
    host: &dyn AmlHost,
    method: &[u8; 4],
    args: &[u64],
) -> Option<u64> {
    let mut idx = Index::new();
    index_blob(aml, &mut idx);
    idx.resolve_fields();
    let mut interp = Interp::new(
        aml,
        &idx.fields,
        &idx.methods,
        &idx.method_bodies,
        &idx.names,
        host,
    );
    interp.invoke_for_test(method, args)
}

// ---------------------------------------------------------------------------
// Value parsing helpers
// ---------------------------------------------------------------------------

/// Parse a Name's data object at `pos` into an [`AmlVal`].
fn parse_value(aml: &[u8], pos: usize) -> Option<AmlVal> {
    match *aml.get(pos)? {
        OP_ZERO => Some(AmlVal::Int(0)),
        OP_ONE => Some(AmlVal::Int(1)),
        OP_ONES => Some(AmlVal::Int(u64::MAX)),
        OP_BYTE_PREFIX | OP_WORD_PREFIX | OP_DWORD_PREFIX | OP_QWORD_PREFIX => {
            const_integer(aml, pos).map(|(v, _)| AmlVal::Int(v))
        }
        OP_STRING_PREFIX => {
            let mut q = pos + 1;
            let mut s = KVec::new();
            while *aml.get(q)? != 0 {
                let _ = s.push(aml[q]);
                q += 1;
            }
            Some(AmlVal::Str(s))
        }
        OP_BUFFER => {
            let (total, after_len) = pkg_length(aml, pos + 1)?;
            let buf_end = (pos + 1) + total;
            let data_start = skip_term_arg(aml, after_len)?; // skip BufferSize
            let bytes = aml.get(data_start..buf_end)?;
            Some(AmlVal::Buf(bytes_from_slice(bytes)))
        }
        _ => None,
    }
}

/// True if the data object at `pos` is the `"PNP0C50"` id (as a string or
/// an EISA-packed integer).
fn value_is_pnp0c50(aml: &[u8], pos: usize) -> bool {
    match parse_value(aml, pos) {
        Some(AmlVal::Str(s)) => s.as_slice() == b"PNP0C50",
        Some(AmlVal::Int(v)) => v == EISAID_PNP0C50,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Generic ACPI platform-device discovery (by _HID/_CID), for the platform bus.
// ---------------------------------------------------------------------------

/// A discovered ACPI platform device's presence + decoded `_CRS` resources.
///
/// Returned by [`find_device_by_ids`] for non-PCI devices the platform bus
/// binds (e.g. the i8042 keyboard at `PNP0303`). `present` is `None` when the
/// device has no `_STA` or its `_STA` could not be evaluated (commonly because
/// it gates on an Embedded-Controller field the narrow interpreter cannot read,
/// as the Lenovo keyboard's `P2MK` does) — callers must not treat that as
/// "absent".
pub struct AcpiPlatformDevice {
    /// `_STA` bit 0: `Some(true/false)` if evaluated, `None` if unknown.
    pub present: Option<bool>,
    /// I/O-port windows from `_CRS`, in order.
    pub io_ports: KVec<resource::IoPortResource>,
    /// IRQ descriptors from `_CRS`, in order.
    pub irqs: KVec<resource::IrqResource>,
}

/// Convenience over [`find_device_by_ids`] that extracts the DSDT + SSDT AML
/// bodies from a validated [`AcpiTables`] (mirrors [`scan_i2c_hid`]). Used by
/// the platform bus to discover non-PCI devices at boot.
pub fn find_acpi_platform_device(
    tables: &AcpiTables,
    host: &dyn AmlHost,
    ids: &[&[u8]],
    debug: bool,
) -> Option<AcpiPlatformDevice> {
    let facp = tables.find_table(b"FACP")?;
    let fadt = Fadt::parse(facp.raw())?;
    let dsdt = tables::table_bytes_at(fadt.dsdt_phys)?;
    let dsdt_aml = dsdt.get(HEADER_LEN..)?;

    let mut ssdts: KVec<&[u8]> = KVec::new();
    tables.find_map_raw(b"SSDT", |bytes| {
        if let Some(aml) = bytes.get(HEADER_LEN..) {
            let _ = ssdts.push(aml);
        }
        None::<()>
    });

    find_device_by_ids(dsdt_aml, ssdts.as_slice(), host, ids, debug)
}

/// Find the first DSDT device whose `_HID` or `_CID` matches any id in `ids`
/// (each a 7-byte EISA id like `b"PNP0303"`), and decode its presence + `_CRS`
/// I/O/IRQ resources. Returns `None` if no matching device exists.
///
/// Like [`scan_blobs`] this never panics; a DSDT it cannot fully parse yields
/// `None` (or empty resource lists) and the caller proceeds.
pub fn find_device_by_ids(
    dsdt_aml: &[u8],
    ssdts: &[&[u8]],
    host: &dyn AmlHost,
    ids: &[&[u8]],
    debug: bool,
) -> Option<AcpiPlatformDevice> {
    let mut idx = Index::new();
    index_blob(dsdt_aml, &mut idx);
    for ssdt in ssdts {
        index_blob(ssdt, &mut idx);
    }
    idx.resolve_fields();

    let devices = collect_devices(dsdt_aml);
    for dev in devices.iter() {
        let members = collect_platform_members(dsdt_aml, *dev, ids);
        if !members.matched {
            continue;
        }
        if debug {
            klog_info!(
                "aml: matched platform device (_STA={}, _CRS_name={}, _CRS_method={})",
                members.sta.is_some(),
                members.crs_name.is_some(),
                members.crs_method.is_some(),
            );
        }
        return Some(process_platform_device(dsdt_aml, &members, &idx, host));
    }
    None
}

struct PlatformMembers<'a> {
    aml: &'a [u8],
    ids: &'a [&'a [u8]],
    matched: bool,
    sta: Option<Range>,
    ini: Option<Range>,
    crs_name: Option<Range>,
    crs_method: Option<Range>,
    names: KVec<(u32, Range)>,
    overlays: KVec<(u32, Overlay)>,
}

impl Visitor for PlatformMembers<'_> {
    fn name(&mut self, seg: [u8; 4], value: Range) {
        let key = nameseg_key(&seg);
        if (key == nameseg_key(b"_CID") || key == nameseg_key(b"_HID"))
            && value_matches_any_id(self.aml, value.start, self.ids)
        {
            self.matched = true;
        }
        if key == nameseg_key(b"_CRS") {
            self.crs_name = Some(value);
        }
        let _ = self.names.push((key, value));
    }
    fn method(&mut self, seg: [u8; 4], _argc: u8, body: Range) {
        let key = nameseg_key(&seg);
        if key == nameseg_key(b"_STA") {
            self.sta = Some(body);
        } else if key == nameseg_key(b"_INI") {
            self.ini = Some(body);
        } else if key == nameseg_key(b"_CRS") {
            self.crs_method = Some(body);
        }
    }
    fn create_field(&mut self, source: [u8; 4], byte_index: u64, width_bytes: u8, name: [u8; 4]) {
        let _ = self.overlays.push((
            nameseg_key(&name),
            Overlay {
                source: nameseg_key(&source),
                byte_index,
                width: width_bytes,
            },
        ));
    }
    fn enter_device(&mut self, _seg: [u8; 4], _body: Range) -> bool {
        false // direct members only
    }
}

fn collect_platform_members<'a>(
    aml: &'a [u8],
    dev: Range,
    ids: &'a [&'a [u8]],
) -> PlatformMembers<'a> {
    let mut m = PlatformMembers {
        aml,
        ids,
        matched: false,
        sta: None,
        ini: None,
        crs_name: None,
        crs_method: None,
        names: KVec::new(),
        overlays: KVec::new(),
    };
    walk_terms(aml, dev.start, dev.end, &mut m);
    m
}

fn process_platform_device(
    aml: &[u8],
    members: &PlatformMembers<'_>,
    idx: &Index,
    host: &dyn AmlHost,
) -> AcpiPlatformDevice {
    let mut interp = Interp::new(
        aml,
        &idx.fields,
        &idx.methods,
        &idx.method_bodies,
        &idx.names,
        host,
    );
    for &(seg, range) in members.names.iter() {
        if let Some(val) = parse_value(aml, range.start) {
            interp.locals.insert(seg, val);
        }
    }
    for &(seg, ov) in members.overlays.iter() {
        interp.overlays.insert(seg, ov);
    }

    // Presence: evaluate _STA if present. Unevaluable (EC-gated) ⇒ None.
    let present = match members.sta {
        None => None,
        Some(body) => interp
            .run(body.start, body.end)
            .map(|v| v.as_int() & 0x01 != 0),
    };

    // Configure: run _INI (may patch the resource template).
    if let Some(ini) = members.ini {
        interp.run(ini.start, ini.end);
    }

    // Resolve the _CRS resource template: a Name's Buffer, or a method's return.
    let crs_val = if let Some(crs) = members.crs_name {
        parse_value(aml, crs.start)
    } else if let Some(crs) = members.crs_method {
        interp.run(crs.start, crs.end)
    } else {
        None
    };

    let (io_ports, irqs) = match crs_val {
        Some(AmlVal::Buf(b)) => (
            resource::parse_io_ports(b.as_slice()),
            resource::parse_irqs(b.as_slice()),
        ),
        _ => (KVec::new(), KVec::new()),
    };

    AcpiPlatformDevice {
        present,
        io_ports,
        irqs,
    }
}

/// True if the data object at `pos` matches any 7-byte EISA id in `ids`,
/// whether stored as a string or an EISA-packed integer.
fn value_matches_any_id(aml: &[u8], pos: usize, ids: &[&[u8]]) -> bool {
    match parse_value(aml, pos) {
        Some(AmlVal::Str(s)) => ids.iter().any(|id| s.as_slice() == *id),
        Some(AmlVal::Int(v)) => ids
            .iter()
            .any(|id| eisa_pack(id).map(|p| p as u64) == Some(v)),
        _ => false,
    }
}

/// Pack a 7-character EISA/PNP id (`b"PNP0303"`) into its compressed DWORD form
/// (matching the value an `EisaId(...)` term encodes in AML). Returns `None`
/// for a malformed id.
#[doc(hidden)]
pub fn eisa_pack(id: &[u8]) -> Option<u32> {
    if id.len() != 7 {
        return None;
    }
    let letter = |b: u8| -> Option<u8> {
        if b.is_ascii_uppercase() {
            Some(b - 0x40) // 'A' => 1
        } else {
            None
        }
    };
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'A'..=b'F' => Some(b - b'A' + 10),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    };
    let c1 = letter(id[0])?;
    let c2 = letter(id[1])?;
    let c3 = letter(id[2])?;
    let d1 = hex(id[3])?;
    let d2 = hex(id[4])?;
    let d3 = hex(id[5])?;
    let d4 = hex(id[6])?;
    let b0 = (c1 << 2) | (c2 >> 3);
    let b1 = ((c2 & 0x07) << 5) | c3;
    let b2 = (d1 << 4) | d2;
    let b3 = (d3 << 4) | d4;
    Some((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24))
}

/// Parse `\_SB.PC00.I2Cn` (or similar) → `n`. Looks for the `I2C` token
/// followed by a single decimal digit.
fn controller_index_from_path(path: &[u8]) -> Option<u8> {
    let n = path.len();
    let mut i = 0;
    while i + 3 < n {
        if &path[i..i + 3] == b"I2C" {
            let d = path[i + 3];
            if d.is_ascii_digit() {
                return Some(d - b'0');
            }
        }
        i += 1;
    }
    None
}
