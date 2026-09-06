//! GPT and MBR parsing, the [`PartitionDevice`] window, and `/dev` block
//! nodes.

use slopos_ostd::{KArc, klog_info};
use slopos_testing::{TestResult, fail};

use super::{Ext2ImageSpec, build_ext2_image};
use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::devfs::{DevFs, block_read_entitled, devfs_register_block_device};
use crate::partition::{
    LOGICAL_SECTOR, PartitionDevice, PartitionError, PartitionKind, PartitionScheme, probe,
};
use crate::verity::crc32;
use crate::vfs::{FileSystem, FileType, VfsError};

const TOTAL_SECTORS: u64 = 128;
const IMAGE_BYTES: usize = (TOTAL_SECTORS * LOGICAL_SECTOR) as usize;
const PRIMARY_HEADER_LBA: u64 = 1;
const PRIMARY_ARRAY_LBA: u64 = 2;
const BACKUP_ARRAY_LBA: u64 = TOTAL_SECTORS - 2;
const BACKUP_HEADER_LBA: u64 = TOTAL_SECTORS - 1;
const FIRST_USABLE: u64 = 3;
const LAST_USABLE: u64 = TOTAL_SECTORS - 3;
/// Room the fixture leaves for one entry-array copy.
const ARRAY_FIT_BYTES: usize = 32 * LOGICAL_SECTOR as usize;

fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// One MBR entry, LBA fields only. Also stamps the `0xAA55` signature, so any
/// entry written leaves a recognisable boot sector.
fn write_mbr_entry(buf: &mut [u8], slot: usize, type_byte: u8, first_lba: u32, sectors: u32) {
    put_u16(buf, 510, 0xAA55);
    let base = 446 + slot * 16;
    buf[base] = 0x00;
    buf[base + 4] = type_byte;
    put_u32(buf, base + 8, first_lba);
    put_u32(buf, base + 12, sectors);
}

/// Each test starts from [`GptSpec::valid`] and breaks exactly one thing.
#[derive(Clone, Copy)]
struct GptSpec {
    num_entries: u32,
    entry_size: u32,
    first_usable: u64,
    last_usable: u64,
    /// Inclusive LBA ranges written into slots 0 and 1.
    entries: [(u64, u64); 2],
    write_backup: bool,
    break_primary_header: bool,
    break_primary_array: bool,
    break_backup_array: bool,
    protective_mbr: bool,
    /// An extra MBR entry in slot 1: type byte, first LBA, sector count.
    mbr_entry: Option<(u8, u32, u32)>,
    /// Both copies' entry array moved to this LBA, for the usable-range check.
    array_lba: Option<u64>,
}

impl GptSpec {
    fn valid() -> Self {
        Self {
            num_entries: 4,
            entry_size: 128,
            first_usable: FIRST_USABLE,
            last_usable: LAST_USABLE,
            entries: [(8, 15), (16, 63)],
            write_backup: true,
            break_primary_header: false,
            break_primary_array: false,
            break_backup_array: false,
            protective_mbr: true,
            mbr_entry: None,
            array_lba: None,
        }
    }
}

#[inline(never)]
fn install_array(buf: &mut [u8], lba: u64, spec: &GptSpec) -> u32 {
    let at = (lba * LOGICAL_SECTOR) as usize;
    let len = spec.num_entries as usize * spec.entry_size as usize;
    let array = &mut buf[at..at + len];
    array.fill(0);
    for (slot, &(first, last)) in spec.entries.iter().enumerate() {
        let base = slot * spec.entry_size as usize;
        // A non-zero type GUID is what marks the slot used; the byte value
        // doubles as the fixture's identity check.
        array[base..base + 16].fill(0x11 * (slot as u8 + 1));
        put_u64(array, base + 32, first);
        put_u64(array, base + 40, last);
    }
    crc32(array)
}

#[inline(never)]
fn install_header(
    buf: &mut [u8],
    lba: u64,
    alt_lba: u64,
    array_lba: u64,
    crc: u32,
    spec: &GptSpec,
) {
    let at = (lba * LOGICAL_SECTOR) as usize;
    let header = &mut buf[at..at + LOGICAL_SECTOR as usize];
    header.fill(0);
    header[..8].copy_from_slice(b"EFI PART");
    put_u32(header, 8, 0x0001_0000);
    put_u32(header, 12, 92);
    put_u64(header, 24, lba);
    put_u64(header, 32, alt_lba);
    put_u64(header, 40, spec.first_usable);
    put_u64(header, 48, spec.last_usable);
    put_u64(header, 72, array_lba);
    put_u32(header, 80, spec.num_entries);
    put_u32(header, 84, spec.entry_size);
    put_u32(header, 88, crc);
    let header_crc = crc32(&header[..92]);
    put_u32(header, 16, header_crc);
}

#[inline(never)]
fn install_gpt(buf: &mut [u8], spec: &GptSpec) {
    if spec.protective_mbr {
        write_mbr_entry(buf, 0, 0xEE, 1, (TOTAL_SECTORS - 1) as u32);
    }
    if let Some((type_byte, first, sectors)) = spec.mbr_entry {
        write_mbr_entry(buf, 1, type_byte, first, sectors);
    }

    let primary_array_lba = spec.array_lba.unwrap_or(PRIMARY_ARRAY_LBA);
    let backup_array_lba = spec.array_lba.unwrap_or(BACKUP_ARRAY_LBA);
    let array_bytes = spec.num_entries as usize * spec.entry_size as usize;
    let mut primary_crc = 0;
    let mut backup_crc = 0;
    if array_bytes <= ARRAY_FIT_BYTES {
        primary_crc = install_array(buf, primary_array_lba, spec);
        backup_crc = install_array(buf, backup_array_lba, spec);
        if spec.break_primary_array {
            buf[(primary_array_lba * LOGICAL_SECTOR) as usize + 32] ^= 0xFF;
        }
        if spec.break_backup_array {
            buf[(backup_array_lba * LOGICAL_SECTOR) as usize + 32] ^= 0xFF;
        }
    }

    install_header(
        buf,
        PRIMARY_HEADER_LBA,
        BACKUP_HEADER_LBA,
        primary_array_lba,
        primary_crc,
        spec,
    );
    if spec.break_primary_header {
        // The reserved field is inside the CRC's coverage, so touching it
        // invalidates the header without changing what it claims.
        buf[(PRIMARY_HEADER_LBA * LOGICAL_SECTOR) as usize + 20] ^= 0xFF;
    }
    if spec.write_backup {
        install_header(
            buf,
            BACKUP_HEADER_LBA,
            PRIMARY_HEADER_LBA,
            backup_array_lba,
            backup_crc,
            spec,
        );
    }
}

fn gpt_device(spec: &GptSpec) -> Option<MemoryBlockDevice> {
    let device = MemoryBlockDevice::allocate(IMAGE_BYTES)?;
    device.with_buffer_mut(|buf| install_gpt(buf, spec));
    Some(device)
}

fn mbr_device(entries: &[(u8, u32, u32)]) -> Option<MemoryBlockDevice> {
    let device = MemoryBlockDevice::allocate(IMAGE_BYTES)?;
    device.with_buffer_mut(|buf| {
        for (slot, &(type_byte, first, sectors)) in entries.iter().enumerate() {
            write_mbr_entry(buf, slot, type_byte, first, sectors);
        }
    });
    Some(device)
}

pub fn test_partition_gpt_happy_path() -> TestResult {
    klog_info!("PART_TEST: GPT happy path");
    let Some(device) = gpt_device(&GptSpec::valid()) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("valid GPT rejected: {:?}", e),
    };
    if table.scheme != PartitionScheme::Gpt {
        return fail!("scheme was {:?}, want Gpt", table.scheme);
    }
    if table.entries.len() != 2 {
        return fail!("parsed {} entries, want 2", table.entries.len());
    }
    let first = table.entries[0];
    let second = table.entries[1];
    if first.number != 1 || first.start != 8 * LOGICAL_SECTOR || first.len != 8 * LOGICAL_SECTOR {
        return fail!("entry 1 window wrong: {:?}", first);
    }
    if second.number != 2
        || second.start != 16 * LOGICAL_SECTOR
        || second.len != 48 * LOGICAL_SECTOR
    {
        return fail!("entry 2 window wrong: {:?}", second);
    }
    match (first.kind, second.kind) {
        (PartitionKind::Gpt { type_guid: a }, PartitionKind::Gpt { type_guid: b })
            if a == [0x11; 16] && b == [0x22; 16] => {}
        other => return fail!("type GUIDs not carried through: {:?}", other),
    }
    TestResult::Pass
}

pub fn test_partition_gpt_backup_header_fallback() -> TestResult {
    klog_info!("PART_TEST: GPT backup header fallback");
    let mut spec = GptSpec::valid();
    spec.break_primary_header = true;
    let Some(device) = gpt_device(&spec) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("a broken primary header must fall back, got {:?}", e),
    };
    if table.scheme != PartitionScheme::Gpt || table.entries.len() != 2 {
        return fail!(
            "backup parse gave {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    if table.entries[0].start != 8 * LOGICAL_SECTOR {
        return fail!("backup entry array not read: {:?}", table.entries[0]);
    }

    // Both headers gone: the protective MBR must be reported as such, not as
    // a partitionless disk.
    let mut both = GptSpec::valid();
    both.break_primary_header = true;
    both.write_backup = false;
    let Some(device) = gpt_device(&both) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::ProtectiveMbrWithoutGpt) => TestResult::Pass,
        other => fail!(
            "both headers gone should be ProtectiveMbr…, got {:?}",
            other
        ),
    }
}

pub fn test_partition_gpt_array_crc_falls_back_to_mbr() -> TestResult {
    klog_info!("PART_TEST: GPT array CRC bad on both copies");
    let mut spec = GptSpec::valid();
    spec.break_primary_array = true;
    spec.break_backup_array = true;
    spec.mbr_entry = Some((0x83, 8, 8));
    let Some(device) = gpt_device(&spec) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("an MBR was present, so the parse must use it: {:?}", e),
    };
    if table.scheme != PartitionScheme::Mbr || table.entries.len() != 1 {
        return fail!(
            "fallback gave {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    let entry = table.entries[0];
    if entry.number != 2 || entry.start != 8 * LOGICAL_SECTOR || entry.len != 8 * LOGICAL_SECTOR {
        return fail!("MBR fallback entry wrong: {:?}", entry);
    }

    // No MBR to fall back to: partitionless would hand the mount a stale
    // whole-device filesystem.
    let mut naked = GptSpec::valid();
    naked.break_primary_array = true;
    naked.break_backup_array = true;
    naked.protective_mbr = false;
    let Some(device) = gpt_device(&naked) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::CorruptGpt) => TestResult::Pass,
        other => fail!("want CorruptGpt with no MBR, got {:?}", other),
    }
}

pub fn test_partition_gpt_geometry_limits() -> TestResult {
    klog_info!("PART_TEST: GPT geometry limits");
    let mut many = GptSpec::valid();
    many.num_entries = 200;
    let Some(device) = gpt_device(&many) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::Unsupported) => {}
        other => return fail!("200 entries should be Unsupported, got {:?}", other),
    }

    let mut narrow = GptSpec::valid();
    narrow.entry_size = 64;
    let Some(device) = gpt_device(&narrow) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::Unsupported) => {}
        other => return fail!("64-byte entries should be Unsupported, got {:?}", other),
    }

    let mut huge = GptSpec::valid();
    huge.num_entries = 128;
    huge.entry_size = 512;
    let Some(device) = gpt_device(&huge) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::Unsupported) => {}
        other => return fail!("a 64 KiB array should be Unsupported, got {:?}", other),
    }

    // An entry past the usable range is dropped, not fatal: the rest of the
    // table still describes the disk.
    let mut past = GptSpec::valid();
    past.entries[1] = (16, TOTAL_SECTORS + 8);
    let Some(device) = gpt_device(&past) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("one bad entry must not fail the table: {:?}", e),
    };
    if table.entries.len() != 1 || table.entries[0].number != 1 {
        return fail!(
            "out-of-range entry not dropped: {} left",
            table.entries.len()
        );
    }
    TestResult::Pass
}

pub fn test_partition_mbr_rules() -> TestResult {
    klog_info!("PART_TEST: MBR rules");
    let Some(device) = mbr_device(&[(0x83, 8, 8), (0x83, 16, 32)]) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("plain MBR rejected: {:?}", e),
    };
    if table.scheme != PartitionScheme::Mbr || table.entries.len() != 2 {
        return fail!(
            "MBR gave {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    if table.entries[1].start != 16 * LOGICAL_SECTOR
        || table.entries[1].len != 32 * LOGICAL_SECTOR
        || table.entries[1].kind != (PartitionKind::Mbr { type_byte: 0x83 })
    {
        return fail!("second MBR entry wrong: {:?}", table.entries[1]);
    }

    let Some(device) = mbr_device(&[(0xEE, 1, (TOTAL_SECTORS - 1) as u32)]) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::ProtectiveMbrWithoutGpt) => {}
        other => return fail!("protective MBR alone: {:?}", other),
    }

    // An extended container is skipped rather than descended into, so the
    // primary beside it is still found.
    let Some(device) = mbr_device(&[(0x83, 8, 8), (0x05, 16, 32)]) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("extended container broke the parse: {:?}", e),
    };
    if table.entries.len() != 1 || table.entries[0].number != 1 {
        return fail!(
            "extended entry not skipped: {} entries",
            table.entries.len()
        );
    }

    // A window that leaves the device is dropped.
    let Some(device) = mbr_device(&[(0x83, 8, u32::MAX)]) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("oversized MBR entry: {:?}", e),
    };
    if !table.entries.is_empty() || table.scheme != PartitionScheme::None {
        return fail!("oversized MBR entry accepted: {:?}", table.scheme);
    }
    TestResult::Pass
}

pub fn test_partition_device_windows_the_parent() -> TestResult {
    klog_info!("PART_TEST: PartitionDevice offsetting");
    let Some(memory) = MemoryBlockDevice::allocate(4096) else {
        return TestResult::Pass;
    };
    let Ok(memory) = KArc::try_new(memory) else {
        return TestResult::Pass;
    };
    let parent: KArc<dyn BlockDevice + Send + Sync> = memory.clone();

    if PartitionDevice::try_new(parent.clone(), 1024 + 1, 1024).is_ok() {
        return fail!("an unaligned start must be refused");
    }
    if PartitionDevice::try_new(parent.clone(), 3584, 1024).is_ok() {
        return fail!("a window past the parent must be refused");
    }
    if PartitionDevice::try_new(parent.clone(), 1024, 0).is_ok() {
        return fail!("an empty window must be refused");
    }

    let Ok(window) = PartitionDevice::try_new(parent.clone(), 1024, 1024) else {
        return fail!("a valid window was refused");
    };
    if window.capacity() != 1024 {
        return fail!("capacity() answered {}, want 1024", window.capacity());
    }

    let payload = b"partition-relative";
    if let Err(e) = window.write_at(16, payload) {
        return fail!("write through the window failed: {:?}", e);
    }
    let landed =
        memory.with_buffer_mut(|buf| &buf[1024 + 16..1024 + 16 + payload.len()] == payload);
    if !landed {
        return fail!("the write did not land at start + offset in the parent");
    }
    let mut back = [0u8; 18];
    if let Err(e) = window.read_at(16, &mut back) {
        return fail!("read through the window failed: {:?}", e);
    }
    if &back != payload {
        return fail!("read back the wrong bytes");
    }

    let mut spill = [0u8; 32];
    if window.read_at(1000, &mut spill) != Err(BlockDeviceError::OutOfBounds) {
        return fail!("a read crossing the window end must be OutOfBounds");
    }
    if window.write_at(1024, &spill) != Err(BlockDeviceError::OutOfBounds) {
        return fail!("a write past the window end must be OutOfBounds");
    }
    TestResult::Pass
}

pub fn test_partition_whole_device_image_is_unpartitioned() -> TestResult {
    klog_info!("PART_TEST: whole-device ext2 image probes as None");
    let Some(device) = build_ext2_image(Ext2ImageSpec {
        blocks: 128,
        inodes: 32,
        file_name: Some(b"seed.txt"),
        file_data: Some(b"whole-device"),
        file_block: 40,
    }) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("an unpartitioned image must probe cleanly: {:?}", e),
    };
    if table.scheme != PartitionScheme::None || !table.entries.is_empty() {
        return fail!(
            "ext2 image probed as {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    TestResult::Pass
}

pub fn test_devfs_block_node_serves_the_device() -> TestResult {
    klog_info!("PART_TEST: devfs block node");
    let Some(memory) = MemoryBlockDevice::allocate(2048) else {
        return TestResult::Pass;
    };
    memory.with_buffer_mut(|buf| {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
    });
    let Ok(memory) = KArc::try_new(memory) else {
        return TestResult::Pass;
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = memory;

    let inode = match devfs_register_block_device(b"parttest0", device) {
        Ok(inode) => inode,
        // A rerun of the same boot's suite finds the name already taken.
        Err(VfsError::AlreadyExists) => return TestResult::Pass,
        Err(e) => return fail!("registration failed: {:?}", e),
    };
    if devfs_register_block_device(b"parttest0", {
        let Some(dup) = MemoryBlockDevice::allocate(512) else {
            return TestResult::Pass;
        };
        let Ok(dup) = KArc::try_new(dup) else {
            return TestResult::Pass;
        };
        dup
    })
    .is_ok()
    {
        return fail!("a duplicate name must be refused");
    }

    let fs = DevFs::new();
    match fs.lookup(fs.root_inode(), b"parttest0") {
        Ok(found) if found == inode => {}
        other => return fail!("lookup gave {:?}, want {}", other, inode),
    }
    match fs.stat(inode) {
        Ok(stat) if stat.file_type == FileType::BlockDevice && stat.size == 2048 => {}
        Ok(stat) => return fail!("stat gave {:?} size {}", stat.file_type, stat.size),
        Err(e) => return fail!("stat failed: {:?}", e),
    }

    let mut buf = [0u8; 64];
    match fs.read(inode, 1024, &mut buf) {
        Ok(64) => {}
        other => return fail!("read at 1024 gave {:?}", other),
    }
    if buf
        .iter()
        .enumerate()
        .any(|(i, &b)| b != ((1024 + i) % 251) as u8)
    {
        return fail!("read served the wrong bytes for its offset");
    }
    // Short only at the end of the device: the VFS reads a short count as EOF.
    match fs.read(inode, 2048 - 16, &mut buf) {
        Ok(16) => {}
        other => return fail!("read at the tail gave {:?}", other),
    }
    match fs.read(inode, 2048, &mut buf) {
        Ok(0) => {}
        other => return fail!("read past the end gave {:?}", other),
    }

    match fs.write(inode, 0, b"nope") {
        Err(VfsError::ReadOnly) => {}
        other => return fail!("a block node must refuse writes, got {:?}", other),
    }

    let mut seen = false;
    let listed = fs.readdir(fs.root_inode(), 0, &mut |name, ino, kind| {
        if name == b"parttest0" {
            seen = ino == inode && kind == FileType::BlockDevice;
        }
        true
    });
    if listed.is_err() || !seen {
        return fail!("the block node is missing from readdir");
    }
    TestResult::Pass
}

/// UEFI §5.3.2: the entry array must not intersect the usable range, or a
/// partition window could cover the GPT and a read-write mount inside it
/// would destroy the table. The rule is intersection, not "below
/// first_usable" — a backup array legitimately sits *above* `last_usable`,
/// which the second half checks.
pub fn test_partition_gpt_entry_array_must_miss_the_usable_range() -> TestResult {
    klog_info!("PART_TEST: GPT entry array vs usable range");
    let mut inside = GptSpec::valid();
    // Squarely inside [FIRST_USABLE, LAST_USABLE], and clear of both headers.
    inside.array_lba = Some(FIRST_USABLE + 4);
    inside.protective_mbr = false;
    let Some(device) = gpt_device(&inside) else {
        return TestResult::Pass;
    };
    match probe(&device).map(|t| t.scheme) {
        Err(PartitionError::CorruptGpt) => {}
        other => return fail!("an array inside the usable range was accepted: {:?}", other),
    }

    // The valid fixture's backup array is at BACKUP_ARRAY_LBA > LAST_USABLE:
    // breaking the primary header forces the parse through it.
    let mut backup_only = GptSpec::valid();
    backup_only.break_primary_header = true;
    let Some(device) = gpt_device(&backup_only) else {
        return TestResult::Pass;
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("a backup array above last_usable was rejected: {:?}", e),
    };
    if table.scheme != PartitionScheme::Gpt || table.entries.len() != 2 {
        return fail!(
            "backup-array parse gave {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    TestResult::Pass
}

/// Claims more capacity than it can serve, modelling a device reporting
/// capacity past its media or an image truncated mid-sector.
struct ShortMediaDevice {
    inner: MemoryBlockDevice,
    claimed: u64,
}

impl BlockDevice for ShortMediaDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.claimed
    }
}

/// An unreadable backup LBA must not suppress the MBR fallback: the primary
/// sector carried no `EFI PART`, so no GPT was claimed anywhere. Reporting
/// `Io` would degrade `root=/dev/vda1` to `NoDisk` on a plain MBR disk.
pub fn test_partition_unreadable_backup_lba_still_parses_mbr() -> TestResult {
    klog_info!("PART_TEST: unreadable backup LBA falls back to MBR");
    let Some(inner) = MemoryBlockDevice::allocate(IMAGE_BYTES) else {
        return TestResult::Pass;
    };
    inner.with_buffer_mut(|buf| write_mbr_entry(buf, 0, 0x83, 8, 8));
    let device = ShortMediaDevice {
        inner,
        claimed: IMAGE_BYTES as u64 + LOGICAL_SECTOR,
    };
    let table = match probe(&device) {
        Ok(t) => t,
        Err(e) => return fail!("an unreadable tail sector broke the MBR parse: {:?}", e),
    };
    if table.scheme != PartitionScheme::Mbr || table.entries.len() != 1 {
        return fail!(
            "gave {:?} with {} entries",
            table.scheme,
            table.entries.len()
        );
    }
    if table.entries[0].start != 8 * LOGICAL_SECTOR {
        return fail!("MBR entry window wrong: {:?}", table.entries[0]);
    }
    TestResult::Pass
}

/// The refusal is the point of the node: ext2 does not zero a freed block, so
/// an unprivileged reader would recover unlinked file contents.
pub fn test_devfs_block_node_read_requires_entitlement() -> TestResult {
    klog_info!("PART_TEST: devfs block node read entitlement");
    let Some(memory) = MemoryBlockDevice::allocate(1024) else {
        return TestResult::Pass;
    };
    memory.with_buffer_mut(|buf| buf.fill(0x5A));
    let Ok(memory) = KArc::try_new(memory) else {
        return TestResult::Pass;
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = memory;

    let inode = match devfs_register_block_device(b"parttest1", device) {
        Ok(inode) => inode,
        Err(VfsError::AlreadyExists) => return TestResult::Pass,
        Err(e) => return fail!("registration failed: {:?}", e),
    };

    let mut buf = [0u8; 32];
    match block_read_entitled(inode, 0, &mut buf, false) {
        Err(VfsError::PermissionDenied) => {}
        other => return fail!("an unprivileged raw read was not refused: {:?}", other),
    }
    if buf.iter().any(|&b| b != 0) {
        return fail!("a refused read still filled the buffer");
    }

    match block_read_entitled(inode, 0, &mut buf, true) {
        Ok(32) => {}
        other => return fail!("an entitled read was not served: {:?}", other),
    }
    if buf.iter().any(|&b| b != 0x5A) {
        return fail!("the entitled read served the wrong bytes");
    }

    // The production path runs as a kernel thread during the test phase, which
    // is entitled, so the real `read` must serve too.
    let fs = DevFs::new();
    match fs.read(inode, 0, &mut buf) {
        Ok(32) => TestResult::Pass,
        other => fail!("FileSystem::read on a kernel thread gave {:?}", other),
    }
}

slopos_testing::stest!(name = test_partition_gpt_happy_path, suite = fs);
slopos_testing::stest!(name = test_partition_gpt_backup_header_fallback, suite = fs);
slopos_testing::stest!(
    name = test_partition_gpt_array_crc_falls_back_to_mbr,
    suite = fs
);
slopos_testing::stest!(name = test_partition_gpt_geometry_limits, suite = fs);
slopos_testing::stest!(name = test_partition_mbr_rules, suite = fs);
slopos_testing::stest!(name = test_partition_device_windows_the_parent, suite = fs);
slopos_testing::stest!(
    name = test_partition_whole_device_image_is_unpartitioned,
    suite = fs
);
slopos_testing::stest!(name = test_devfs_block_node_serves_the_device, suite = fs);
slopos_testing::stest!(
    name = test_partition_gpt_entry_array_must_miss_the_usable_range,
    suite = fs
);
slopos_testing::stest!(
    name = test_partition_unreadable_backup_lba_still_parses_mbr,
    suite = fs
);
slopos_testing::stest!(
    name = test_devfs_block_node_read_requires_entitlement,
    suite = fs
);
