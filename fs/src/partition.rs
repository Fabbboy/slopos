//! Partition-table parsing and the [`BlockDevice`] adaptors that turn a table
//! entry into a device.
//!
//! GPT layout and validation follow the UEFI Specification (2.10) §5.3: the
//! header at LBA 1, the backup at the last logical block, a CRC32 over each of
//! the header and the entry array. MBR is the conventional 512-byte boot
//! sector, `0xAA55` at offset 510 and four 16-byte entries from 446. Only an
//! MBR entry's LBA fields are read: its CHS fields cannot address a modern
//! disk and disagree with the LBA fields often enough to be a trap.

use slopos_ostd::klog_info;
use slopos_ostd::{KArc, KVec};

use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::verity::crc32;

/// Table offsets are in units of this whatever the device's physical sector
/// size is: a 4K-native disk still describes its table in 512-byte blocks.
pub const LOGICAL_SECTOR: u64 = 512;

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_MIN: u32 = 92;
const GPT_HEADER_MAX: u32 = 512;
const GPT_MAX_ENTRIES: u32 = 128;
const GPT_MIN_ENTRY_SIZE: u32 = 128;
/// The array is staged whole to CRC it, so the allocation is bounded here
/// rather than by a header field.
const GPT_MAX_ARRAY_BYTES: u64 = 32 * 1024;
const GPT_PRIMARY_LBA: u64 = 1;

const MBR_SIGNATURE_AT: usize = 510;
const MBR_SIGNATURE: u16 = 0xAA55;
const MBR_ENTRY_AT: usize = 446;
const MBR_ENTRY_SIZE: usize = 16;
const MBR_ENTRY_COUNT: usize = 4;
const MBR_TYPE_UNUSED: u8 = 0x00;
const MBR_TYPE_PROTECTIVE: u8 = 0xEE;
const MBR_TYPE_EXTENDED_CHS: u8 = 0x05;
const MBR_TYPE_EXTENDED_LBA: u8 = 0x0F;
const MBR_TYPE_EXTENDED_LINUX: u8 = 0x85;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionError {
    Io,
    NoMemory,
    /// Well-formed but beyond what this parser reads: an array over
    /// [`GPT_MAX_ARRAY_BYTES`], more than 128 entries, an entry stride under
    /// 128 bytes or not a multiple of 8, or a GPT major revision other than 1.
    Unsupported,
    /// A protective MBR (type `0xEE`) with no usable GPT behind it: the real
    /// table is unreadable, so reporting the disk partitionless would hand the
    /// mount a stale whole-device filesystem.
    ProtectiveMbrWithoutGpt,
    /// Both GPT copies failed validation and there is no MBR to fall back to.
    CorruptGpt,
    /// A [`PartitionDevice`] window that does not start on a logical-sector
    /// boundary.
    Misaligned,
    /// A [`PartitionDevice`] window that is empty or leaves the parent.
    OutOfRange,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionKind {
    Gpt { type_guid: [u8; 16] },
    Mbr { type_byte: u8 },
}

/// One usable partition: a byte window into the device it was parsed from.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PartitionEntry {
    /// 1-based, as `/dev/vda1` spells it.
    pub number: u8,
    pub start: u64,
    pub len: u64,
    pub kind: PartitionKind,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionScheme {
    None,
    Gpt,
    Mbr,
}

pub struct PartitionTable {
    pub scheme: PartitionScheme,
    pub entries: KVec<PartitionEntry>,
}

impl PartitionTable {
    pub const fn unpartitioned() -> Self {
        Self {
            scheme: PartitionScheme::None,
            entries: KVec::new(),
        }
    }

    pub fn find(&self, number: u8) -> Option<&PartitionEntry> {
        self.entries.iter().find(|e| e.number == number)
    }
}

#[inline]
fn le_u16(bytes: &[u8], at: usize) -> u16 {
    let Some(s) = bytes.get(at..at + 2) else {
        return 0;
    };
    (s[0] as u16) | ((s[1] as u16) << 8)
}

#[inline]
fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let Some(s) = bytes.get(at..at + 4) else {
        return 0;
    };
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

#[inline]
fn le_u64(bytes: &[u8], at: usize) -> u64 {
    (le_u32(bytes, at) as u64) | ((le_u32(bytes, at + 4) as u64) << 32)
}

/// Zeroed heap buffer: a sector or an entry array must not sit on the kernel
/// stack.
fn staged(len: usize) -> Result<KVec<u8>, PartitionError> {
    let mut buf = KVec::with_capacity(len).map_err(|_| PartitionError::NoMemory)?;
    for _ in 0..len {
        buf.push(0).map_err(|_| PartitionError::NoMemory)?;
    }
    Ok(buf)
}

/// Why one GPT copy was not usable.
///
/// Only a copy that *claimed* a GPT — one whose signature matched — may
/// suppress the MBR fallback, so a sector that could not be read or staged
/// before any signature matched is [`GptReject::Absent`]: a plain MBR disk
/// whose last logical block is unreadable must still parse its MBR.
#[derive(Copy, Clone, PartialEq, Eq)]
enum GptReject {
    Absent,
    Corrupt,
    Unsupported,
    /// A GPT was claimed, but its entry array could not be read or staged.
    Indeterminate(PartitionError),
}

/// Parse the partition table of `device`. Neither signature present is
/// [`PartitionScheme::None`], the whole-device case.
///
/// GPT is tried before MBR because a GPT disk carries a protective MBR that
/// would otherwise parse as one partition spanning the disk.
pub fn probe(device: &dyn BlockDevice) -> Result<PartitionTable, PartitionError> {
    let capacity = device.capacity();
    if capacity < 2 * LOGICAL_SECTOR {
        return Ok(PartitionTable::unpartitioned());
    }

    let gpt_claimed = match probe_gpt(device, capacity) {
        Ok(Some(table)) => return Ok(table),
        Ok(None) => false,
        Err(PartitionError::CorruptGpt) => true,
        Err(e) => return Err(e),
    };

    match probe_mbr(device, capacity) {
        Ok(table) if gpt_claimed && table.scheme == PartitionScheme::None => {
            Err(PartitionError::CorruptGpt)
        }
        other => other,
    }
}

/// `Ok(None)`: no GPT here, try MBR. `Err(CorruptGpt)`: a GPT was claimed by
/// at least one copy and neither validated, so an MBR fallback must not report
/// the disk as partitionless.
#[inline(never)]
fn probe_gpt(
    device: &dyn BlockDevice,
    capacity: u64,
) -> Result<Option<PartitionTable>, PartitionError> {
    let primary = match read_gpt_copy(device, capacity, GPT_PRIMARY_LBA) {
        Ok(table) => return Ok(Some(table)),
        Err(e) => e,
    };

    let backup_lba = capacity / LOGICAL_SECTOR - 1;
    let backup = if backup_lba == GPT_PRIMARY_LBA {
        GptReject::Absent
    } else {
        match read_gpt_copy(device, capacity, backup_lba) {
            Ok(table) => {
                klog_info!(
                    "PART: primary GPT header unusable, parsed the backup at LBA {backup_lba}"
                );
                return Ok(Some(table));
            }
            Err(e) => e,
        }
    };

    let verdict = if primary == GptReject::Absent {
        backup
    } else {
        primary
    };
    match verdict {
        GptReject::Absent => Ok(None),
        GptReject::Corrupt => Err(PartitionError::CorruptGpt),
        GptReject::Unsupported => Err(PartitionError::Unsupported),
        GptReject::Indeterminate(e) => Err(e),
    }
}

struct GptHeader {
    entry_lba: u64,
    num_entries: u32,
    entry_size: u32,
    array_crc: u32,
    first_usable: u64,
    last_usable: u64,
}

#[inline(never)]
fn read_gpt_copy(
    device: &dyn BlockDevice,
    capacity: u64,
    lba: u64,
) -> Result<PartitionTable, GptReject> {
    let header = parse_gpt_header(device, capacity, lba)?;
    let array_bytes = header.num_entries as u64 * header.entry_size as u64;

    let mut array = staged(array_bytes as usize)
        .map_err(|_| GptReject::Indeterminate(PartitionError::NoMemory))?;
    let at = header
        .entry_lba
        .checked_mul(LOGICAL_SECTOR)
        .ok_or(GptReject::Corrupt)?;
    device
        .read_at(at, array.as_mut_slice())
        .map_err(|_| GptReject::Indeterminate(PartitionError::Io))?;
    if crc32(&array) != header.array_crc {
        return Err(GptReject::Corrupt);
    }

    let mut entries = KVec::new();
    for index in 0..header.num_entries as usize {
        let base = index * header.entry_size as usize;
        let Some(raw) = array.get(base..base + GPT_MIN_ENTRY_SIZE as usize) else {
            break;
        };
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&raw[..16]);
        if type_guid.iter().all(|&b| b == 0) {
            continue;
        }
        let first = le_u64(raw, 32);
        let last = le_u64(raw, 40);
        let number = (index + 1) as u8;
        if first > last || first < header.first_usable || last > header.last_usable {
            klog_info!("PART: GPT entry {number} lies outside the usable range — skipped");
            continue;
        }
        let Some((start, len)) = window_bytes(first, last, capacity) else {
            klog_info!("PART: GPT entry {number} leaves the device — skipped");
            continue;
        };
        entries
            .push(PartitionEntry {
                number,
                start,
                len,
                kind: PartitionKind::Gpt { type_guid },
            })
            .map_err(|_| GptReject::Indeterminate(PartitionError::NoMemory))?;
    }

    Ok(PartitionTable {
        scheme: PartitionScheme::Gpt,
        entries,
    })
}

fn parse_gpt_header(
    device: &dyn BlockDevice,
    capacity: u64,
    lba: u64,
) -> Result<GptHeader, GptReject> {
    // Nothing has claimed a GPT here yet, so a failure to stage or read the
    // sector leaves the disk a candidate for MBR.
    let mut header = staged(LOGICAL_SECTOR as usize).map_err(|_| GptReject::Absent)?;
    let at = lba.checked_mul(LOGICAL_SECTOR).ok_or(GptReject::Absent)?;
    device
        .read_at(at, header.as_mut_slice())
        .map_err(|_| GptReject::Absent)?;
    if &header[..8] != GPT_SIGNATURE {
        return Err(GptReject::Absent);
    }
    if le_u32(&header, 8) >> 16 != 1 {
        return Err(GptReject::Unsupported);
    }
    let header_size = le_u32(&header, 12);
    if !(GPT_HEADER_MIN..=GPT_HEADER_MAX).contains(&header_size) {
        return Err(GptReject::Corrupt);
    }
    let stored_crc = le_u32(&header, 16);
    // UEFI §5.3.2: the header CRC covers `header_size` bytes with its own
    // field taken as zero.
    header.as_mut_slice()[16..20].fill(0);
    if crc32(&header[..header_size as usize]) != stored_crc {
        return Err(GptReject::Corrupt);
    }
    // A header that disagrees about where it lives is a copy of the other one,
    // so its entry-array pointer cannot be trusted either.
    if le_u64(&header, 24) != lba {
        return Err(GptReject::Corrupt);
    }

    let first_usable = le_u64(&header, 40);
    let last_usable = le_u64(&header, 48);
    let entry_lba = le_u64(&header, 72);
    let num_entries = le_u32(&header, 80);
    let entry_size = le_u32(&header, 84);
    let array_crc = le_u32(&header, 88);

    if num_entries == 0 || num_entries > GPT_MAX_ENTRIES {
        return Err(GptReject::Unsupported);
    }
    if entry_size < GPT_MIN_ENTRY_SIZE || entry_size % 8 != 0 {
        return Err(GptReject::Unsupported);
    }
    let array_bytes = num_entries as u64 * entry_size as u64;
    if array_bytes > GPT_MAX_ARRAY_BYTES {
        return Err(GptReject::Unsupported);
    }

    let total_sectors = capacity / LOGICAL_SECTOR;
    if first_usable > last_usable || last_usable >= total_sectors {
        return Err(GptReject::Corrupt);
    }
    let array_end = entry_lba
        .checked_mul(LOGICAL_SECTOR)
        .and_then(|b| b.checked_add(array_bytes))
        .ok_or(GptReject::Corrupt)?;
    if array_end > capacity {
        return Err(GptReject::Corrupt);
    }
    // UEFI §5.3.2: the entry array must not intersect
    // `[FirstUsableLBA, LastUsableLBA]`, or a partition window could contain
    // the GPT itself and a read-write mount inside it would destroy the table.
    // Intersection, not "below first_usable": a *backup* array legitimately
    // sits above `LastUsableLBA`.
    let array_last_lba = entry_lba + (array_bytes - 1) / LOGICAL_SECTOR;
    if entry_lba <= last_usable && array_last_lba >= first_usable {
        return Err(GptReject::Corrupt);
    }

    Ok(GptHeader {
        entry_lba,
        num_entries,
        entry_size,
        array_crc,
        first_usable,
        last_usable,
    })
}

/// Inclusive LBA range to a byte window, `None` if it leaves the device.
fn window_bytes(first_lba: u64, last_lba: u64, capacity: u64) -> Option<(u64, u64)> {
    let start = first_lba.checked_mul(LOGICAL_SECTOR)?;
    let sectors = last_lba.checked_sub(first_lba)?.checked_add(1)?;
    let len = sectors.checked_mul(LOGICAL_SECTOR)?;
    if start.checked_add(len)? > capacity {
        return None;
    }
    Some((start, len))
}

#[inline(never)]
fn probe_mbr(device: &dyn BlockDevice, capacity: u64) -> Result<PartitionTable, PartitionError> {
    let mut sector = staged(LOGICAL_SECTOR as usize)?;
    device
        .read_at(0, sector.as_mut_slice())
        .map_err(|_| PartitionError::Io)?;
    if le_u16(&sector, MBR_SIGNATURE_AT) != MBR_SIGNATURE {
        return Ok(PartitionTable::unpartitioned());
    }

    let mut entries = KVec::new();
    let mut protective = false;
    for index in 0..MBR_ENTRY_COUNT {
        let base = MBR_ENTRY_AT + index * MBR_ENTRY_SIZE;
        let Some(raw) = sector.get(base..base + MBR_ENTRY_SIZE) else {
            break;
        };
        let type_byte = raw[4];
        let number = (index + 1) as u8;
        match type_byte {
            MBR_TYPE_UNUSED => continue,
            MBR_TYPE_PROTECTIVE => {
                protective = true;
                continue;
            }
            MBR_TYPE_EXTENDED_CHS | MBR_TYPE_EXTENDED_LBA | MBR_TYPE_EXTENDED_LINUX => {
                klog_info!(
                    "PART: MBR entry {number} is an extended container (type {type_byte:#04x}) — \
                     logical partitions inside it are not enumerated"
                );
                continue;
            }
            _ => {}
        }
        let first = le_u32(raw, 8) as u64;
        let sectors = le_u32(raw, 12) as u64;
        if sectors == 0 {
            continue;
        }
        let Some((start, len)) = window_bytes(first, first + sectors - 1, capacity) else {
            klog_info!("PART: MBR entry {number} leaves the device — skipped");
            continue;
        };
        entries
            .push(PartitionEntry {
                number,
                start,
                len,
                kind: PartitionKind::Mbr { type_byte },
            })
            .map_err(|_| PartitionError::NoMemory)?;
    }

    if entries.is_empty() {
        if protective {
            return Err(PartitionError::ProtectiveMbrWithoutGpt);
        }
        return Ok(PartitionTable::unpartitioned());
    }
    Ok(PartitionTable {
        scheme: PartitionScheme::Mbr,
        entries,
    })
}

/// Whole-device delegate over a shared device, so the same claim can back both
/// the mount and a `/dev` node.
pub struct SharedBlockDevice(pub KArc<dyn BlockDevice + Send + Sync>);

impl BlockDevice for SharedBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.0.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.0.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn write_protected(&self) -> bool {
        self.0.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.0.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.0.checkpoint()
    }
}

/// A byte window into a parent device: the filesystem inside a partition sees
/// offset 0 as the partition's first byte.
pub struct PartitionDevice {
    parent: KArc<dyn BlockDevice + Send + Sync>,
    start: u64,
    len: u64,
}

impl PartitionDevice {
    /// `start` must be logical-sector aligned: virtio-blk turns a
    /// partial-sector write into a read-modify-write, so a misaligned window
    /// would put every filesystem metadata write over bytes outside the
    /// partition.
    pub fn try_new(
        parent: KArc<dyn BlockDevice + Send + Sync>,
        start: u64,
        len: u64,
    ) -> Result<Self, PartitionError> {
        if start % LOGICAL_SECTOR != 0 {
            return Err(PartitionError::Misaligned);
        }
        let end = start.checked_add(len).ok_or(PartitionError::OutOfRange)?;
        if len == 0 || end > parent.capacity() {
            return Err(PartitionError::OutOfRange);
        }
        Ok(Self { parent, start, len })
    }

    fn parent_offset(&self, offset: u64, len: usize) -> Result<u64, BlockDeviceError> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(BlockDeviceError::OutOfBounds)?;
        if end > self.len {
            return Err(BlockDeviceError::OutOfBounds);
        }
        offset
            .checked_add(self.start)
            .ok_or(BlockDeviceError::OutOfBounds)
    }
}

impl BlockDevice for PartitionDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        let at = self.parent_offset(offset, buffer.len())?;
        self.parent.read_at(at, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        let at = self.parent_offset(offset, buffer.len())?;
        self.parent.write_at(at, buffer)
    }

    /// The window length, cached: the parent's `capacity()` takes its state
    /// lock and this is on every bounds check.
    fn capacity(&self) -> u64 {
        self.len
    }

    fn write_protected(&self) -> bool {
        self.parent.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.parent.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.parent.checkpoint()
    }
}
