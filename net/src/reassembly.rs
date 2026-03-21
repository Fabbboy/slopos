use core::sync::atomic::{AtomicU32, Ordering};

use slopos_sync::IrqMutex;
use slopos_utils::{klog_debug, klog_warn};

use super::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};
use super::types::Ipv4Addr;

const MAX_REASSEMBLY_GROUPS: usize = 32;
const MAX_FRAGMENTS_PER_GROUP: usize = 16;
const MAX_FRAGMENT_DATA: usize = 1500;
const MAX_REASSEMBLED_DATA: usize = MAX_FRAGMENTS_PER_GROUP * MAX_FRAGMENT_DATA;

const TICKS_PER_SEC: u64 = 100;
const REASSEMBLY_TIMEOUT_TICKS: u64 = 60 * TICKS_PER_SEC;

static NEXT_GROUP_ID: AtomicU32 = AtomicU32::new(1);

fn alloc_group_id() -> u32 {
    NEXT_GROUP_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReassemblyKey {
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    identification: u16,
    protocol: u8,
}

#[derive(Clone, Copy)]
struct Fragment {
    offset: u16,
    len: u16,
    data: [u8; MAX_FRAGMENT_DATA],
    more_fragments: bool,
}

struct ReassemblyGroup {
    active: bool,
    key: ReassemblyKey,
    fragments: [Option<Fragment>; MAX_FRAGMENTS_PER_GROUP],
    fragment_count: u8,
    total_len: Option<u16>,
    timer_token: Option<TimerToken>,
    group_id: u32,
}

pub struct ReassembledPacket {
    pub protocol: u8,
    pub len: u16,
    pub data: [u8; MAX_REASSEMBLED_DATA],
}

pub struct ReassemblyTable {
    groups: [ReassemblyGroup; MAX_REASSEMBLY_GROUPS],
}

pub static REASSEMBLY_TABLE: IrqMutex<ReassemblyTable> = IrqMutex::new(ReassemblyTable::new());

impl ReassemblyGroup {
    const fn empty() -> Self {
        Self {
            active: false,
            key: ReassemblyKey {
                src_ip: Ipv4Addr([0, 0, 0, 0]),
                dst_ip: Ipv4Addr([0, 0, 0, 0]),
                identification: 0,
                protocol: 0,
            },
            fragments: [None; MAX_FRAGMENTS_PER_GROUP],
            fragment_count: 0,
            total_len: None,
            timer_token: None,
            group_id: 0,
        }
    }
}

impl ReassembledPacket {
    const fn new(protocol: u8, len: u16) -> Self {
        Self {
            protocol,
            len,
            data: [0; MAX_REASSEMBLED_DATA],
        }
    }
}

impl ReassemblyTable {
    pub const fn new() -> Self {
        Self {
            groups: [const { ReassemblyGroup::empty() }; MAX_REASSEMBLY_GROUPS],
        }
    }

    pub fn reset(&mut self) {
        for i in 0..MAX_REASSEMBLY_GROUPS {
            self.clear_group(i);
        }
    }

    pub fn insert(
        &mut self,
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        identification: u16,
        protocol: u8,
        frag_offset: u16,
        more_fragments: bool,
        data: &[u8],
    ) -> Option<ReassembledPacket> {
        if data.len() > MAX_FRAGMENT_DATA {
            klog_warn!(
                "reassembly: fragment too large (len={}, max={})",
                data.len(),
                MAX_FRAGMENT_DATA
            );
            return None;
        }

        let key = ReassemblyKey {
            src_ip,
            dst_ip,
            identification,
            protocol,
        };

        let group_idx = match self.find_group_index(&key) {
            Some(idx) => idx,
            None => self.alloc_group_slot(key),
        };

        let group = &mut self.groups[group_idx];

        let mut frag = Fragment {
            offset: frag_offset,
            len: data.len() as u16,
            data: [0; MAX_FRAGMENT_DATA],
            more_fragments,
        };
        frag.data[..data.len()].copy_from_slice(data);

        if let Some(existing_idx) = group
            .fragments
            .iter()
            .position(|slot| matches!(slot, Some(existing) if existing.offset == frag_offset))
        {
            group.fragments[existing_idx] = Some(frag);
        } else if let Some(empty_idx) = group.fragments.iter().position(Option::is_none) {
            group.fragments[empty_idx] = Some(frag);
            group.fragment_count = group.fragment_count.saturating_add(1);
        } else {
            klog_warn!(
                "reassembly: group id {} out of fragment slots",
                group.group_id
            );
            return None;
        }

        if !more_fragments {
            let Some(total_len) = frag_offset.checked_add(data.len() as u16) else {
                klog_warn!("reassembly: invalid total length overflow");
                self.clear_group(group_idx);
                return None;
            };
            group.total_len = Some(total_len);
        }

        let total_len = match group.total_len {
            Some(v) => v,
            None => return None,
        };

        if !Self::is_complete(group, total_len) {
            return None;
        }

        let mut out = ReassembledPacket::new(protocol, total_len);
        let mut expected_offset = 0u16;
        while expected_offset < total_len {
            let Some(fragment) = Self::find_fragment_by_offset(group, expected_offset) else {
                return None;
            };

            let frag_len = fragment.len as usize;
            let out_start = expected_offset as usize;
            let out_end = out_start + frag_len;
            out.data[out_start..out_end].copy_from_slice(&fragment.data[..frag_len]);
            expected_offset = expected_offset.wrapping_add(fragment.len);
        }

        self.clear_group(group_idx);
        Some(out)
    }

    pub fn on_timeout(&mut self, group_id: u32) {
        let Some(group_idx) = self
            .groups
            .iter()
            .position(|group| group.active && group.group_id == group_id)
        else {
            return;
        };

        klog_debug!("reassembly: timeout dropped group id {}", group_id);
        self.clear_group(group_idx);
    }

    fn find_group_index(&self, key: &ReassemblyKey) -> Option<usize> {
        self.groups
            .iter()
            .position(|group| group.active && group.key == *key)
    }

    fn alloc_group_slot(&mut self, key: ReassemblyKey) -> usize {
        if let Some(idx) = self.groups.iter().position(|group| !group.active) {
            self.init_group(idx, key);
            return idx;
        }

        let idx = self.find_oldest_group_idx();
        self.clear_group(idx);
        self.init_group(idx, key);
        idx
    }

    fn init_group(&mut self, idx: usize, key: ReassemblyKey) {
        let group_id = alloc_group_id();
        let token = NET_TIMER_WHEEL.schedule(
            REASSEMBLY_TIMEOUT_TICKS,
            TimerKind::ReassemblyTimeout,
            group_id,
        );

        let group = &mut self.groups[idx];
        *group = ReassemblyGroup::empty();
        group.active = true;
        group.key = key;
        group.timer_token = Some(token);
        group.group_id = group_id;
    }

    fn find_oldest_group_idx(&self) -> usize {
        let mut oldest_idx = 0usize;
        let mut oldest_id = u32::MAX;

        for (idx, group) in self.groups.iter().enumerate() {
            if group.active && group.group_id < oldest_id {
                oldest_id = group.group_id;
                oldest_idx = idx;
            }
        }

        oldest_idx
    }

    fn clear_group(&mut self, idx: usize) {
        if let Some(token) = self.groups[idx].timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        self.groups[idx] = ReassemblyGroup::empty();
    }

    fn is_complete(group: &ReassemblyGroup, total_len: u16) -> bool {
        if total_len as usize > MAX_REASSEMBLED_DATA {
            return false;
        }

        let mut expected_offset = 0u16;
        while expected_offset < total_len {
            let Some(fragment) = Self::find_fragment_by_offset(group, expected_offset) else {
                return false;
            };

            if fragment.len == 0 {
                return false;
            }

            let Some(next_offset) = expected_offset.checked_add(fragment.len) else {
                return false;
            };

            if next_offset > total_len {
                return false;
            }

            if !fragment.more_fragments && next_offset != total_len {
                return false;
            }

            expected_offset = next_offset;
        }

        true
    }

    fn find_fragment_by_offset(group: &ReassemblyGroup, offset: u16) -> Option<&Fragment> {
        group
            .fragments
            .iter()
            .find_map(|slot| slot.as_ref().filter(|fragment| fragment.offset == offset))
    }
}
