//! Partition parsing against a real virtio-blk device: a GPT written through
//! the driver's own write capability, read back by the parser, then real I/O
//! through a [`PartitionDevice`] window.
//!
//! Destructive, so it targets the disposable scratch device (disk1) — never
//! disk0, the live root image. The table takes the first and last sectors and
//! its windows sit clear of the offsets the other scratch-disk tests write.

use slopos_fs::blockdev::{BlockDevice, BlockDeviceIndex};
use slopos_fs::partition::{
    LOGICAL_SECTOR, PartitionDevice, PartitionKind, PartitionScheme, probe,
};
use slopos_fs::verity::crc32;
use slopos_ostd::mm::heap::{KArc, KVec};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::virtio_blk;

const SCRATCH: BlockDeviceIndex = BlockDeviceIndex(1);

const ENTRY_SIZE: u32 = 128;
const ENTRY_COUNT: u32 = 4;
const ARRAY_LBA: u64 = 2;
const P1_FIRST: u64 = 64;
const P1_LAST: u64 = 127;
const P2_FIRST: u64 = 128;
const P2_LAST: u64 = 255;

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn entry(buf: &mut [u8], slot: usize, tag: u8, first_lba: u64, last_lba: u64) {
    let at = slot * ENTRY_SIZE as usize;
    let row = &mut buf[at..at + ENTRY_SIZE as usize];
    row[..16].copy_from_slice(&[tag; 16]);
    row[16..32].copy_from_slice(&[tag ^ 0xFF; 16]);
    put_u64(row, 32, first_lba);
    put_u64(row, 40, last_lba);
}

fn build_array() -> Option<KVec<u8>> {
    let mut array = KVec::<u8>::zeroed(LOGICAL_SECTOR as usize).ok()?;
    entry(&mut array, 0, 0x11, P1_FIRST, P1_LAST);
    entry(&mut array, 1, 0x22, P2_FIRST, P2_LAST);
    Some(array)
}

/// A GPT header at `lba`, per UEFI 2.10 §5.3: 92 bytes, its own CRC32 computed
/// with the CRC field zeroed.
fn build_header(lba: u64, alt_lba: u64, last_usable: u64, array_crc: u32) -> Option<KVec<u8>> {
    let mut sector = KVec::<u8>::zeroed(LOGICAL_SECTOR as usize).ok()?;
    sector[..8].copy_from_slice(b"EFI PART");
    put_u32(&mut sector, 8, 0x0001_0000);
    put_u32(&mut sector, 12, 92);
    put_u32(&mut sector, 16, 0);
    put_u64(&mut sector, 24, lba);
    put_u64(&mut sector, 32, alt_lba);
    put_u64(&mut sector, 40, ARRAY_LBA + 1);
    put_u64(&mut sector, 48, last_usable);
    sector[56..72].copy_from_slice(&[0x5A; 16]);
    put_u64(&mut sector, 72, ARRAY_LBA);
    put_u32(&mut sector, 80, ENTRY_COUNT);
    put_u32(&mut sector, 84, ENTRY_SIZE);
    put_u32(&mut sector, 88, array_crc);
    let header_crc = crc32(&sector[..92]);
    put_u32(&mut sector, 16, header_crc);
    Some(sector)
}

/// A protective MBR, so the disk reads as GPT-partitioned to anything that
/// stops at sector 0.
fn build_protective_mbr(sectors: u64) -> Option<KVec<u8>> {
    let mut sector = KVec::<u8>::zeroed(LOGICAL_SECTOR as usize).ok()?;
    let at = 446;
    sector[at + 4] = 0xEE;
    put_u32(&mut sector, at + 8, 1);
    let span = u32::try_from(sectors.saturating_sub(1)).unwrap_or(u32::MAX);
    put_u32(&mut sector, at + 12, span);
    sector[510] = 0x55;
    sector[511] = 0xAA;
    Some(sector)
}

/// Its own frame: three sector buffers live here.
#[inline(never)]
fn install_gpt(device: &dyn BlockDevice, sectors: u64) -> Result<(), &'static str> {
    let array = build_array().ok_or("array alloc")?;
    let array_crc = crc32(&array[..(ENTRY_COUNT * ENTRY_SIZE) as usize]);
    let primary = build_header(1, sectors - 1, sectors - 2, array_crc).ok_or("header alloc")?;
    let mbr = build_protective_mbr(sectors).ok_or("mbr alloc")?;

    device.write_at(0, &mbr).map_err(|_| "mbr write")?;
    device
        .write_at(LOGICAL_SECTOR, &primary)
        .map_err(|_| "header write")?;
    device
        .write_at(ARRAY_LBA * LOGICAL_SECTOR, &array)
        .map_err(|_| "array write")?;
    Ok(())
}

/// A window write at offset 0 must land at the parent's `start`, and nothing
/// may reach past `len`.
#[inline(never)]
fn check_window(
    shared: &KArc<dyn BlockDevice + Send + Sync>,
    start: u64,
    len: u64,
) -> Result<(), &'static str> {
    let part = PartitionDevice::try_new(shared.clone(), start, len).map_err(|_| "try_new")?;
    if part.capacity() != len {
        return Err("window capacity is not its length");
    }

    let mut pattern = KVec::<u8>::zeroed(LOGICAL_SECTOR as usize).map_err(|_| "pattern alloc")?;
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i * 31) ^ 0xA5) as u8;
    }
    part.write_at(0, &pattern).map_err(|_| "window write")?;

    let mut parent_view =
        KVec::<u8>::zeroed(LOGICAL_SECTOR as usize).map_err(|_| "readback alloc")?;
    shared
        .read_at(start, &mut parent_view)
        .map_err(|_| "parent read")?;
    if parent_view[..] != pattern[..] {
        return Err("a window write did not land at the parent's start offset");
    }
    if part.read_at(len, &mut parent_view).is_ok() {
        return Err("a read past the window was not refused");
    }
    Ok(())
}

pub fn test_partition_gpt_on_a_real_device() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    let sectors = token.capacity() / LOGICAL_SECTOR;
    assert_test!(
        sectors > P2_LAST + 4,
        "scratch disk too small for the table"
    );
    if let Err(why) = install_gpt(&token, sectors) {
        return fail!("could not install the GPT: {}", why);
    }

    let table = match probe(&token) {
        Ok(t) => t,
        Err(e) => return fail!("probe of the scratch disk failed: {:?}", e),
    };
    assert_eq_test!(
        table.scheme,
        PartitionScheme::Gpt,
        "a written GPT must parse as one"
    );
    assert_eq_test!(table.entries.len(), 2, "two entries were written");

    let first = table.entries[0];
    assert_eq_test!(first.number, 1, "first entry is partition 1");
    assert_eq_test!(
        first.start,
        P1_FIRST * LOGICAL_SECTOR,
        "partition 1 start offset"
    );
    assert_eq_test!(
        first.len,
        (P1_LAST - P1_FIRST + 1) * LOGICAL_SECTOR,
        "partition 1 length"
    );
    assert_test!(
        matches!(first.kind, PartitionKind::Gpt { type_guid } if type_guid == [0x11; 16]),
        "partition 1 keeps its type GUID"
    );
    assert_eq_test!(
        table.entries[1].start,
        P2_FIRST * LOGICAL_SECTOR,
        "partition 2 start offset"
    );

    let shared: KArc<dyn BlockDevice + Send + Sync> = match KArc::try_new(token) {
        Ok(k) => k,
        Err(_) => return TestResult::Pass,
    };
    if let Err(why) = check_window(&shared, first.start, first.len) {
        return fail!("{}", why);
    }
    pass!()
}

slopos_testing::stest!(
    name = test_partition_gpt_on_a_real_device,
    suite = partition_device
);
