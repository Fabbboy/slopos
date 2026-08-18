//! HID report-descriptor parser for a Precision-Touchpad digitizer collection.
//! Multitouch fingers appear as repeated X/Y/Tip occurrences in descriptor
//! order, so the engine indexes them by occurrence.

use slopos_ostd::KVec;

pub const PAGE_GENERIC_DESKTOP: u16 = 0x01;
pub const PAGE_BUTTON: u16 = 0x09;
pub const PAGE_DIGITIZER: u16 = 0x0d;

pub const USAGE_X: u16 = 0x30; // Generic Desktop
pub const USAGE_Y: u16 = 0x31; // Generic Desktop
pub const USAGE_TIP_SWITCH: u16 = 0x42; // Digitizer
pub const USAGE_CONTACT_ID: u16 = 0x51; // Digitizer
pub const USAGE_INPUT_MODE: u16 = 0x52; // Digitizer (device configuration)
pub const USAGE_CONTACT_COUNT: u16 = 0x54; // Digitizer
pub const USAGE_BUTTON_1: u16 = 0x01; // Button page

#[derive(Clone, Copy, Debug)]
pub struct HidField {
    pub report_id: u8,
    pub usage_page: u16,
    pub usage: u16,
    /// Offset within the report *data*, past the report-ID byte when IDs are used.
    pub bit_offset: u32,
    pub bit_size: u32,
    pub logical_min: i32,
    pub logical_max: i32,
}

pub struct ReportFormat {
    pub fields: KVec<HidField>,
    /// The descriptor declared report IDs, so reports carry a leading ID byte.
    pub uses_report_ids: bool,
    /// Report ID of the "Input Mode" feature. Writing `0x03` to it switches the
    /// device from mouse-compatibility to multitouch mode.
    pub input_mode_report_id: Option<u8>,
}

impl ReportFormat {
    /// In descriptor order, so the Nth match is finger N.
    pub fn matches(&self, page: u16, usage: u16) -> impl Iterator<Item = &HidField> {
        self.fields
            .iter()
            .filter(move |f| f.usage_page == page && f.usage == usage)
    }
}

#[derive(Clone, Copy)]
struct GlobalState {
    usage_page: u16,
    report_size: u32,
    report_count: u32,
    report_id: u8,
    logical_min: i32,
    logical_max: i32,
}

impl GlobalState {
    const fn new() -> Self {
        Self {
            usage_page: 0,
            report_size: 0,
            report_count: 0,
            report_id: 0,
            logical_min: 0,
            logical_max: 0,
        }
    }
}

pub fn parse_report_descriptor(desc: &[u8]) -> ReportFormat {
    let mut fields: KVec<HidField> = KVec::new();
    let mut g = GlobalState::new();
    let mut gstack: KVec<GlobalState> = KVec::new();
    let mut usages: KVec<(u16, u16)> = KVec::new();
    let mut usage_min: Option<u16> = None;
    let mut usage_max: Option<u16> = None;
    let mut uses_report_ids = false;
    let mut input_mode_report_id: Option<u8> = None;
    let mut bit_off = [0u32; 256];

    let mut p = 0usize;
    while p < desc.len() {
        let prefix = desc[p];
        if prefix == 0xfe {
            // Long item: 0xFE, dataSize, tag, data...
            let size = *desc.get(p + 1).unwrap_or(&0) as usize;
            p += 3 + size;
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let btype = (prefix >> 2) & 0x03;
        let tag = (prefix >> 4) & 0x0f;
        let data_u = read_le(desc, p + 1, size);
        let data_i = read_signed(desc, p + 1, size);
        let next = p + 1 + size;

        match btype {
            0 => {
                // Main item.
                match tag {
                    0x8 => {
                        // Input.
                        let constant = data_u & 0x01 != 0;
                        emit_input(
                            &mut fields,
                            &g,
                            &usages,
                            usage_min,
                            usage_max,
                            constant,
                            &mut bit_off,
                        );
                        usages.clear();
                        usage_min = None;
                        usage_max = None;
                    }
                    0x9 | 0xb => {
                        // Output / Feature. A Feature carrying the Digitizer
                        // "Input Mode" usage is the multitouch mode selector.
                        if tag == 0xb
                            && usages
                                .iter()
                                .any(|&(p, u)| p == PAGE_DIGITIZER && u == USAGE_INPUT_MODE)
                        {
                            input_mode_report_id = Some(g.report_id);
                        }
                        usages.clear();
                        usage_min = None;
                        usage_max = None;
                    }
                    0xa => {
                        // Collection.
                        usages.clear();
                        usage_min = None;
                        usage_max = None;
                    }
                    0xc => { /* End Collection */ }
                    _ => {}
                }
            }
            1 => {
                // Global item.
                match tag {
                    0x0 => g.usage_page = data_u as u16,
                    0x1 => g.logical_min = data_i,
                    0x2 => g.logical_max = data_i,
                    0x7 => g.report_size = data_u,
                    0x8 => {
                        g.report_id = data_u as u8;
                        uses_report_ids = true;
                    }
                    0x9 => g.report_count = data_u,
                    0xa => {
                        let _ = gstack.push(g);
                    }
                    0xb => {
                        if let Some(prev) = gstack.pop() {
                            g = prev;
                        }
                    }
                    _ => {}
                }
            }
            2 => {
                // Local item.
                match tag {
                    0x0 => {
                        // Usage. 4-byte form carries the page in the high word.
                        let (page, usage) = if size == 4 {
                            ((data_u >> 16) as u16, data_u as u16)
                        } else {
                            (g.usage_page, data_u as u16)
                        };
                        let _ = usages.push((page, usage));
                    }
                    0x1 => usage_min = Some(data_u as u16),
                    0x2 => usage_max = Some(data_u as u16),
                    _ => {}
                }
            }
            _ => {}
        }
        p = next;
    }

    ReportFormat {
        fields,
        uses_report_ids,
        input_mode_report_id,
    }
}

fn emit_input(
    fields: &mut KVec<HidField>,
    g: &GlobalState,
    usages: &KVec<(u16, u16)>,
    usage_min: Option<u16>,
    usage_max: Option<u16>,
    constant: bool,
    bit_off: &mut [u32; 256],
) {
    let rid = g.report_id as usize;
    let total = g.report_size.saturating_mul(g.report_count);
    if constant || g.report_size == 0 {
        bit_off[rid] = bit_off[rid].wrapping_add(total);
        return;
    }
    for i in 0..g.report_count {
        let (page, usage) = pick_usage(g, usages, usage_min, usage_max, i as usize);
        let _ = fields.push(HidField {
            report_id: g.report_id,
            usage_page: page,
            usage,
            bit_offset: bit_off[rid],
            bit_size: g.report_size,
            logical_min: g.logical_min,
            logical_max: g.logical_max,
        });
        bit_off[rid] = bit_off[rid].wrapping_add(g.report_size);
    }
}

fn pick_usage(
    g: &GlobalState,
    usages: &KVec<(u16, u16)>,
    usage_min: Option<u16>,
    usage_max: Option<u16>,
    i: usize,
) -> (u16, u16) {
    if let Some(&(page, usage)) = usages.get(i) {
        return (page, usage);
    }
    if let (Some(min), Some(max)) = (usage_min, usage_max) {
        let u = (min as usize + i).min(max as usize) as u16;
        return (g.usage_page, u);
    }
    // Repeat the last explicit usage (common for arrays of identical fields).
    if let Some(&(page, usage)) = usages.last() {
        return (page, usage);
    }
    (g.usage_page, 0)
}

fn read_le(desc: &[u8], p: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        if let Some(&b) = desc.get(p + i) {
            v |= (b as u32) << (8 * i);
        }
    }
    v
}

fn read_signed(desc: &[u8], p: usize, n: usize) -> i32 {
    let raw = read_le(desc, p, n);
    match n {
        1 => (raw as u8 as i8) as i32,
        2 => (raw as u16 as i16) as i32,
        _ => raw as i32,
    }
}
