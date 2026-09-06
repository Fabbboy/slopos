//! A v2 verity trailer: verified *and* writable (plan item 6.6).
//!
//! The trailer is hand-encoded rather than generated, so it cross-checks the
//! writer: `scripts/gen_verity.py --version 2`, `fs/src/verity.rs` and this
//! fixture must agree byte for byte.
//!
//! The fixture omits the writer's pad to a 512-byte sector: a memory device
//! has no sector granularity, and the parser locates every region from
//! `capacity()` backwards.

use slopos_ostd::{KArc, KBox};
use slopos_testing::{TestResult, fail};

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::verity::{
    AttestTrust, FsExtent, VerityError, VerityStatus, build_verified_trusting, crc32,
};

const BS: usize = 512;
const N: usize = 8;
/// The blocks the ext2 superblock (1024 bytes at byte 1024) lives in — two at
/// 512-byte blocks, both permanently unattested.
const SB_BLOCKS: core::ops::Range<usize> = 2..4;

struct Fixture {
    version: u32,
    /// Blocks flipped *after* their hash is recorded, so the stored hash no
    /// longer describes the disk.
    corrupt: &'static [usize],
    bad_bitmap_crc: bool,
}

const V1: Fixture = Fixture {
    version: 1,
    corrupt: &[5],
    bad_bitmap_crc: false,
};

const fn v2(corrupt: &'static [usize]) -> Fixture {
    Fixture {
        version: 2,
        corrupt,
        bad_bitmap_crc: false,
    }
}

fn bitmap_len(n: usize) -> usize {
    n.div_ceil(8)
}

fn image_len(version: u32) -> usize {
    let bitmap = if version >= 2 { bitmap_len(N) } else { 0 };
    N * BS + N * 4 + bitmap + 32
}

#[inline(never)]
fn fill_image(img: &mut [u8], f: &Fixture) {
    for i in 0..N {
        for j in 0..BS {
            img[i * BS + j] = ((i.wrapping_mul(31).wrapping_add(j)) & 0xFF) as u8;
        }
    }
    let arr_off = N * BS;
    for i in 0..N {
        let h = crc32(&img[i * BS..(i + 1) * BS]).to_le_bytes();
        img[arr_off + i * 4..arr_off + i * 4 + 4].copy_from_slice(&h);
    }
    for &c in f.corrupt {
        img[c * BS] ^= 0xFF;
    }
    let root = crc32(&img[arr_off..arr_off + N * 4]);

    let bm_off = arr_off + N * 4;
    let mut bitmap_crc = 0u32;
    let hdr = if f.version >= 2 {
        let len = bitmap_len(N);
        for i in 0..N {
            img[bm_off + (i >> 3)] |= 1u8 << (i & 7);
        }
        for b in SB_BLOCKS {
            img[bm_off + (b >> 3)] &= !(1u8 << (b & 7));
        }
        bitmap_crc = crc32(&img[bm_off..bm_off + len]);
        if f.bad_bitmap_crc {
            bitmap_crc ^= 0x1;
        }
        bm_off + len
    } else {
        bm_off
    };

    write_header(img, hdr, f.version, root, bitmap_crc);
}

#[inline(never)]
fn write_header(img: &mut [u8], at: usize, version: u32, root: u32, bitmap_crc: u32) {
    img[at..at + 4].copy_from_slice(&0x5356_5254u32.to_le_bytes());
    img[at + 4..at + 8].copy_from_slice(&version.to_le_bytes());
    img[at + 8..at + 12].copy_from_slice(&1u32.to_le_bytes());
    img[at + 12..at + 16].copy_from_slice(&(BS as u32).to_le_bytes());
    img[at + 16..at + 24].copy_from_slice(&(N as u64).to_le_bytes());
    img[at + 24..at + 28].copy_from_slice(&root.to_le_bytes());
    img[at + 28..at + 32].copy_from_slice(&bitmap_crc.to_le_bytes());
}

/// The image behind a `KArc`, so a test can re-parse the same bytes after a
/// checkpoint: `build_verified_trusting` consumes the device it wraps.
struct SharedMem(KArc<MemoryBlockDevice>);

impl BlockDevice for SharedMem {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.0.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.0.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.0.capacity()
    }
}

#[inline(never)]
fn build_shared(f: &Fixture) -> Option<KArc<MemoryBlockDevice>> {
    let dev = MemoryBlockDevice::allocate(image_len(f.version))?;
    dev.with_buffer_mut(|img| fill_image(img, f));
    KArc::try_new(dev).ok()
}

type Mounted = (KBox<dyn BlockDevice + Send + Sync>, VerityStatus);

#[inline(never)]
fn mount(image: &KArc<MemoryBlockDevice>, trust: AttestTrust) -> Result<Mounted, VerityError> {
    let boxed: KBox<dyn BlockDevice + Send + Sync> =
        KBox::try_new(SharedMem(image.clone())).map_err(|_| VerityError::OutOfMemory)?;
    build_verified_trusting(
        boxed,
        FsExtent {
            block_size: BS as u32,
            blocks: N as u64,
        },
        trust,
    )
}

#[inline(never)]
fn read_block(device: &dyn BlockDevice, block: usize) -> Result<(), BlockDeviceError> {
    let mut buf = [0u8; BS];
    device.read_at((block * BS) as u64, &mut buf)
}

#[inline(never)]
fn write_block(device: &dyn BlockDevice, block: usize, byte: u8) -> Result<(), BlockDeviceError> {
    let buf = [byte; BS];
    device.write_at((block * BS) as u64, &buf)
}

fn all_but_superblock() -> u64 {
    (N - SB_BLOCKS.len()) as u64
}

#[inline(never)]
fn attested_of(status: VerityStatus) -> Result<u64, TestResult> {
    match status {
        VerityStatus::VerifiedWritable {
            blocks,
            block_size,
            attested,
        } => {
            if blocks != N as u64 || block_size != BS as u32 {
                return Err(fail!(
                    "v2 trailer geometry: {} blocks of {}",
                    blocks,
                    block_size
                ));
            }
            Ok(attested)
        }
        other => Err(fail!("want VerifiedWritable, got {:?}", other)),
    }
}

/// A v2 image is a writable root: writes go through, and the status still
/// reports verified, so `verity=require` is satisfied.
pub fn test_verity_rw_v2_mounts_writable() -> TestResult {
    let Some(image) = build_shared(&v2(&[])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, status) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
    };
    let attested = match attested_of(status) {
        Ok(n) => n,
        Err(r) => return r,
    };
    if attested != all_but_superblock() {
        return fail!(
            "a fresh v2 image must attest every block but the superblock's, got {}",
            attested
        );
    }
    if device.write_protected() {
        return fail!("a v2 device must accept writes");
    }
    match write_block(&*device, 4, 0xC3) {
        Ok(()) => TestResult::Pass,
        Err(e) => fail!("write to a v2 device failed: {:?}", e),
    }
}

/// v1 is unchanged by the v2 code path: write-protected, and every block
/// verified on a full read.
pub fn test_verity_rw_v1_stays_write_protected() -> TestResult {
    let Some(image) = build_shared(&V1) else {
        return fail!("out of memory building the v1 fixture");
    };
    let (device, status) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v1 trailer must still mount, got {:?}", e),
    };
    match status {
        VerityStatus::Verified { blocks, block_size } if blocks == N as u64 => {
            if block_size != BS as u32 {
                return fail!("v1 block size {}", block_size);
            }
        }
        other => return fail!("a v1 trailer must report Verified, got {:?}", other),
    }
    if !device.write_protected() {
        return fail!("a v1 device must refuse writes");
    }
    match write_block(&*device, 4, 0xC3) {
        Err(BlockDeviceError::WriteProtected) => {}
        other => return fail!("want WriteProtected, got {:?}", other),
    }
    if read_block(&*device, 4).is_err() {
        return fail!("a clean block must read on a v1 device");
    }
    match read_block(&*device, 5) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        other => fail!("a corrupt block must fail on a v1 device, got {:?}", other),
    }
}

/// An unknown version is refused, not read unverified.
pub fn test_verity_rw_unknown_version_refused() -> TestResult {
    let future = Fixture {
        version: 7,
        corrupt: &[],
        bad_bitmap_crc: false,
    };
    let Some(image) = build_shared(&future) else {
        return fail!("out of memory building the fixture");
    };
    match mount(&image, AttestTrust::Persisted) {
        Err(VerityError::UnsupportedTrailer) => TestResult::Pass,
        other => fail!(
            "an unknown version must refuse, got {:?}",
            other.map(|(_, s)| s)
        ),
    }
}

/// Corruption in a block nobody wrote is still caught on a writable image.
pub fn test_verity_rw_untouched_block_is_verified() -> TestResult {
    let Some(image) = build_shared(&v2(&[5])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, _) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
    };
    if let Err(e) = read_block(&*device, 4) {
        return fail!("an untouched clean block must verify, got {:?}", e);
    }
    match read_block(&*device, 5) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        other => fail!(
            "corruption in an untouched block must be caught, got {:?}",
            other
        ),
    }
}

/// A write un-attests exactly the blocks it touched: block 4 reads back
/// unverified, corrupt block 5 — never written — still fails.
pub fn test_verity_rw_write_un_attests_block() -> TestResult {
    let Some(image) = build_shared(&v2(&[4, 5])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, _) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
    };
    match read_block(&*device, 4) {
        Err(BlockDeviceError::IntegrityFailure) => {}
        other => return fail!("block 4 must start attested, got {:?}", other),
    }
    if let Err(e) = write_block(&*device, 4, 0xC3) {
        return fail!("write failed: {:?}", e);
    }
    if let Err(e) = read_block(&*device, 4) {
        return fail!("a rewritten block must read unverified, got {:?}", e);
    }
    match read_block(&*device, 5) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        other => fail!("a block nobody wrote must stay attested, got {:?}", other),
    }
}

/// A partial write leaves a block no hash describes, so it un-attests the
/// whole block rather than the bytes it covered.
pub fn test_verity_rw_partial_write_un_attests_block() -> TestResult {
    let Some(image) = build_shared(&v2(&[6])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, _) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
    };
    match read_block(&*device, 6) {
        Err(BlockDeviceError::IntegrityFailure) => {}
        other => return fail!("block 6 must start attested, got {:?}", other),
    }
    let payload = [0xA5u8; 16];
    if let Err(e) = device.write_at((6 * BS + 8) as u64, &payload) {
        return fail!("partial write failed: {:?}", e);
    }
    match read_block(&*device, 6) {
        Ok(()) => TestResult::Pass,
        Err(e) => fail!(
            "a partially rewritten block must be unattested, got {:?}",
            e
        ),
    }
}

/// The superblock's block is unattested from the start: the mount stamp
/// rewrites its bytes on every boot.
pub fn test_verity_rw_superblock_block_unattested() -> TestResult {
    let Some(image) = build_shared(&v2(&[2, 3])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, status) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
    };
    let attested = match attested_of(status) {
        Ok(n) => n,
        Err(r) => return r,
    };
    if attested != all_but_superblock() {
        return fail!("want {} attested, got {}", all_but_superblock(), attested);
    }
    for block in SB_BLOCKS {
        if let Err(e) = read_block(&*device, block) {
            return fail!(
                "the superblock's block {} must read unverified, got {:?}",
                block,
                e
            );
        }
    }
    TestResult::Pass
}

/// The persist half of the crash-safety contract: after a checkpoint, the
/// next mount sees exactly the blocks this boot rewrote.
pub fn test_verity_rw_checkpoint_persists_bitmap() -> TestResult {
    let Some(image) = build_shared(&v2(&[4, 5])) else {
        return fail!("out of memory building the v2 fixture");
    };
    {
        let (device, _) = match mount(&image, AttestTrust::Persisted) {
            Ok(v) => v,
            Err(e) => return fail!("a v2 trailer must mount, got {:?}", e),
        };
        if let Err(e) = write_block(&*device, 4, 0xC3) {
            return fail!("write failed: {:?}", e);
        }
        if let Err(e) = device.checkpoint() {
            return fail!("checkpoint failed: {:?}", e);
        }
    }

    let (device, status) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("the checkpointed image must remount, got {:?}", e),
    };
    let attested = match attested_of(status) {
        Ok(n) => n,
        Err(r) => return r,
    };
    if attested != all_but_superblock() - 1 {
        return fail!(
            "want {} attested after one rewritten block, got {}",
            all_but_superblock() - 1,
            attested
        );
    }
    if let Err(e) = read_block(&*device, 4) {
        return fail!("a block rewritten last boot must not verify, got {:?}", e);
    }
    match read_block(&*device, 5) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        other => fail!(
            "a block no boot wrote must stay attested across the checkpoint, got {:?}",
            other
        ),
    }
}

/// A torn bitmap is a crash, not an attack: nothing attested, no mount
/// refused.
pub fn test_verity_rw_bad_bitmap_crc_attests_nothing() -> TestResult {
    let torn = Fixture {
        version: 2,
        corrupt: &[4],
        bad_bitmap_crc: true,
    };
    let Some(image) = build_shared(&torn) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, status) = match mount(&image, AttestTrust::Persisted) {
        Ok(v) => v,
        Err(e) => return fail!("a torn bitmap must not refuse the mount, got {:?}", e),
    };
    match attested_of(status) {
        Ok(0) => {}
        Ok(n) => return fail!("a torn bitmap must attest nothing, got {}", n),
        Err(r) => return r,
    }
    if device.write_protected() {
        return fail!("a torn bitmap must not make the device read-only");
    }
    match read_block(&*device, 4) {
        Ok(()) => TestResult::Pass,
        Err(e) => fail!("nothing is attested, so no read may fail: {:?}", e),
    }
}

/// The mount half of the crash-safety contract: an image never marked clean
/// may have been written after its bitmap was persisted, so verify nothing.
pub fn test_verity_rw_unclean_image_attests_nothing() -> TestResult {
    let Some(image) = build_shared(&v2(&[4])) else {
        return fail!("out of memory building the v2 fixture");
    };
    let (device, status) = match mount(&image, AttestTrust::NoneThisBoot) {
        Ok(v) => v,
        Err(e) => return fail!("an unclean image must still mount, got {:?}", e),
    };
    match attested_of(status) {
        Ok(0) => {}
        Ok(n) => return fail!("an unclean image must attest nothing, got {}", n),
        Err(r) => return r,
    }
    match read_block(&*device, 4) {
        Ok(()) => TestResult::Pass,
        Err(e) => fail!("an unclean image must verify nothing, got {:?}", e),
    }
}

slopos_testing::stest!(name = test_verity_rw_v2_mounts_writable, suite = verity_rw);
slopos_testing::stest!(
    name = test_verity_rw_v1_stays_write_protected,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_unknown_version_refused,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_untouched_block_is_verified,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_write_un_attests_block,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_partial_write_un_attests_block,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_superblock_block_unattested,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_checkpoint_persists_bitmap,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_bad_bitmap_crc_attests_nothing,
    suite = verity_rw
);
slopos_testing::stest!(
    name = test_verity_rw_unclean_image_attests_nothing,
    suite = verity_rw
);
