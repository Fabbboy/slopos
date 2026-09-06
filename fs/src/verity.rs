//! Read-time block-integrity verification for an attested ext2 image.
//!
//! `scripts/gen_verity.py` appends the trailer at the very END of the image,
//! sector-aligned, so the kernel locates it from `device.capacity()` alone.
//!
//! Two trailer versions differ in writability:
//!
//! * **v1** — the device is **write-protected**, as dm-verity is read-only by
//!   design: a write would leave a block no trailer describes, and so a false
//!   integrity failure on the next boot.
//! * **v2** — the trailer also carries a persisted per-block *attested*
//!   bitmap; a set bit means the block still matches its build-time hash. A
//!   write un-attests every block it touches, including one it only partially
//!   covers, and then goes through, so the image is a writable root that stays
//!   attested for every block nobody wrote. [`BlockDevice::checkpoint`]
//!   persists the bitmap, and the filesystem marks the image clean only after
//!   that, so a crash degrades into "nothing attested" rather than into a
//!   false integrity failure.
//!
//! The block the ext2 superblock lives in is permanently unattested in v2: the
//! mount stamp rewrites it on every boot, so attesting it would invalidate the
//! bitmap the moment it is persisted.
//!
//! The filesystem's own extent decides whether the device's tail *is* a
//! trailer. On a writable image the last filesystem block is ordinary data a
//! user can fill, so magic found inside the extent is file contents, never a
//! trailer to refuse the mount over.
//!
//! CRC-32 is an integrity check, not an authenticity one: an adversary who
//! rewrites a block, the hash array and the root is not defeated.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, KVec, klog_info, lock_class};

use crate::blockdev::{BlockDevice, BlockDeviceError};

// On-disk trailer (little-endian, appended at the end of the image):
//   v1: [ ext2 region ][ pad to sector ][ hash array: N×u32 ][ 32-byte header ]
//   v2: [ ext2 region ][ pad to sector ][ hash array: N×u32 ]
//       [ attested bitmap: ceil(N/8) ][ 32-byte header ]
//                                                     ^ capacity() - 32
// Three places must agree byte for byte: scripts/gen_verity.py writes it, this
// file parses it, and fs/src/tests/verity_rw.rs hand-encodes it.
const VERITY_MAGIC: u32 = 0x5356_5254; // 'TVRS' LE — SlopOS verity
const VERITY_VERSION_PROTECTED: u32 = 1;
const VERITY_VERSION_ATTESTED: u32 = 2;
const VERITY_ALGO_CRC32: u32 = 1;
const HEADER_SIZE: u64 = 32;
/// The header's `reserved` u32, which v2 spends on the bitmap's CRC-32.
const HEADER_BITMAP_CRC_OFF: u64 = 28;
/// The ext2 superblock: 1024 bytes at byte 1024, whichever block that is.
const EXT2_SUPERBLOCK_OFFSET: u64 = 1024;
const EXT2_SUPERBLOCK_LEN: u64 = 1024;
/// Staging window for [`AttestBitmap::write_bits`]: the bitmap is copied out
/// of the lock in pieces, because no spinning lock may span a device write.
const PERSIST_CHUNK: usize = 512;

// CRC-32 (IEEE 802.3 / zlib, reflected, poly 0xEDB88320) — matches Python's
// `zlib.crc32`, which `scripts/gen_verity.py` uses to build the trailer.
const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Feed `data` into a running (pre-final-inversion) CRC-32 state.
fn crc32_feed(mut state: u32, data: &[u8]) -> u32 {
    for &b in data {
        let idx = ((state ^ b as u32) & 0xFF) as usize;
        state = (state >> 8) ^ CRC32_TABLE[idx];
    }
    state
}

/// CRC-32 (IEEE, reflected) of `data`. `crc32(&[]) == 0`, matching `zlib.crc32`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_feed(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// The byte range the filesystem claims, from its own superblock. A trailer
/// lives beyond it or does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsExtent {
    pub block_size: u32,
    pub blocks: u64,
}

impl FsExtent {
    fn bytes(&self) -> Option<u64> {
        self.blocks.checked_mul(self.block_size as u64)
    }
}

/// Whether this boot may trust a v2 trailer's persisted bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestTrust {
    /// The image was marked clean, so the bitmap on disk is current.
    Persisted,
    /// The image was never marked clean: a write may have landed after the
    /// bitmap was persisted, so nothing is verified this boot.
    NoneThisBoot,
}

/// What [`build_verified`] found on the device. Distinct outcomes rather than a
/// bool, because "no trailer" and "a trailer I refused" must never read alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerityStatus {
    /// No trailer: the image was built `VERITY=off`, and mounts writable.
    Absent,
    /// A valid v1 trailer covers `blocks` blocks; the device is
    /// write-protected.
    Verified { blocks: u64, block_size: u32 },
    /// A valid v2 trailer covers `blocks` blocks, of which `attested` still
    /// match their build-time hash; the device accepts writes.
    VerifiedWritable {
        blocks: u64,
        block_size: u32,
        attested: u64,
    },
}

/// A trailer was present and could not be trusted. The mount is refused: an
/// image that claims attestation and cannot deliver it is not one to read
/// unverified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerityError {
    /// Header fields outside what this kernel implements.
    UnsupportedTrailer,
    /// The hash array does not match the root CRC in the header.
    CorruptTrailer,
    /// The trailer does not fit the device, or does not cover the filesystem
    /// it sits behind.
    Geometry,
    /// The hash array or the device wrapper could not be allocated.
    OutOfMemory,
    /// The device failed the reads the trailer parse needs.
    Device,
}

/// The blocks a v2 trailer still attests. Bits only ever clear: a write
/// un-attests, and nothing re-attests short of rebuilding the image.
struct AttestBitmap {
    bits: SpinLock<KVec<u8>>,
    dirty: AtomicBool,
    /// Device offset of the bitmap. It lies beyond the filesystem extent, so
    /// no hash covers it and [`Self::persist`] may rewrite it in place.
    offset: u64,
    crc_offset: u64,
    blocks: u64,
}

impl AttestBitmap {
    fn un_attest(&self, offset: u64, len: u64, block_size: u64) {
        if len == 0 {
            return;
        }
        let first = offset / block_size;
        let last = offset.saturating_add(len - 1) / block_size;
        {
            let mut guard = self.bits.lock();
            let bits = guard.as_mut_slice();
            let mut b = first;
            while b <= last && b < self.blocks {
                bits[(b >> 3) as usize] &= !(1u8 << (b & 7));
                b += 1;
            }
        }
        self.dirty.store(true, Ordering::Release);
    }

    fn count_attested(&self) -> u64 {
        let guard = self.bits.lock();
        let bits = guard.as_slice();
        let full = (self.blocks / 8) as usize;
        let mut n = 0u64;
        for &b in &bits[..full] {
            n += b.count_ones() as u64;
        }
        let rem = (self.blocks % 8) as u32;
        if rem != 0 {
            if let Some(&tail) = bits.get(full) {
                let mask = ((1u16 << rem) - 1) as u8;
                n += (tail & mask).count_ones() as u64;
            }
        }
        n
    }

    fn persist(&self, inner: &dyn BlockDevice) -> Result<(), BlockDeviceError> {
        // Cleared first: a write that races the checkpoint re-dirties the
        // bitmap rather than being swallowed by this pass.
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        match self.write_bits(inner) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.dirty.store(true, Ordering::Release);
                Err(e)
            }
        }
    }

    /// The CRC covers exactly the bytes written, so a bit cleared mid-pass
    /// leaves a self-consistent (if older) bitmap rather than a torn one.
    #[inline(never)]
    fn write_bits(&self, inner: &dyn BlockDevice) -> Result<(), BlockDeviceError> {
        let total = self.blocks.div_ceil(8) as usize;
        let mut chunk = [0u8; PERSIST_CHUNK];
        let mut state = 0xFFFF_FFFFu32;
        let mut done = 0usize;
        while done < total {
            let n = core::cmp::min(PERSIST_CHUNK, total - done);
            {
                let guard = self.bits.lock();
                chunk[..n].copy_from_slice(&guard.as_slice()[done..done + n]);
            }
            state = crc32_feed(state, &chunk[..n]);
            inner.write_at(self.offset + done as u64, &chunk[..n])?;
            done += n;
        }
        inner.write_at(self.crc_offset, &(state ^ 0xFFFF_FFFF).to_le_bytes())
    }
}

/// Verifies each fully-read attested block against a trusted per-block CRC
/// array.
pub struct VerifiedBlockDevice {
    inner: KBox<dyn BlockDevice + Send + Sync>,
    block_size: u32,
    /// Build-time CRC of every block `0..N`; immutable after mount.
    hashes: KVec<u32>,
    /// `None` on a v1 trailer, where no write is ever accepted.
    attested: Option<AttestBitmap>,
}

impl VerifiedBlockDevice {
    /// Only blocks fully contained in the read are verified: sub-block direct
    /// reads (the superblock at byte 1024) keep their own sanity checks
    /// (documented gap G1).
    #[inline(never)]
    fn verify_blocks(
        &self,
        offset: u64,
        buffer: &[u8],
        attested: Option<&[u8]>,
    ) -> Result<(), BlockDeviceError> {
        let bs = self.block_size as u64;
        let n = self.hashes.len() as u64;
        let end = offset + buffer.len() as u64;
        let mut b = offset.div_ceil(bs);
        while b < n && (b + 1) * bs <= end {
            if let Some(bits) = attested {
                if bits[(b >> 3) as usize] & (1u8 << (b & 7)) == 0 {
                    b += 1;
                    continue;
                }
            }
            let idx = b as usize;
            let buf_off = (b * bs - offset) as usize;
            let block = &buffer[buf_off..buf_off + self.block_size as usize];
            let got = crc32(block);
            let want = self.hashes.as_slice()[idx];
            if got != want {
                klog_info!(
                    "verity: block {} integrity check FAILED (crc {:#010x} != expected {:#010x})",
                    idx,
                    got,
                    want,
                );
                return Err(BlockDeviceError::IntegrityFailure);
            }
            b += 1;
        }
        Ok(())
    }
}

impl BlockDevice for VerifiedBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)?;

        if buffer.is_empty() {
            return Ok(());
        }

        // The lock is taken after the read — it must never span block I/O —
        // and held across the whole verification: `write_at` un-attests under
        // it before forwarding, so a block's bytes cannot change between the
        // bit test and its CRC.
        match self.attested.as_ref() {
            None => self.verify_blocks(offset, buffer, None),
            Some(map) => {
                let guard = map.bits.lock();
                self.verify_blocks(offset, buffer, Some(guard.as_slice()))
            }
        }
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        let Some(map) = self.attested.as_ref() else {
            return Err(BlockDeviceError::WriteProtected);
        };
        // Un-attest before forwarding: a write that fails halfway must not
        // leave a block the trailer still claims to describe.
        map.un_attest(offset, buffer.len() as u64, self.block_size as u64);
        self.inner.write_at(offset, buffer)
    }

    fn write_protected(&self) -> bool {
        self.attested.is_none()
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.inner.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        match self.attested.as_ref() {
            None => Ok(()),
            Some(map) => map.persist(&*self.inner),
        }
    }
}

/// Wrap `device` if it carries a valid verity trailer beyond `fs`.
///
/// Returns the device unchanged with [`VerityStatus::Absent`] when there is
/// no trailer, wrapped with [`VerityStatus::Verified`] or
/// [`VerityStatus::VerifiedWritable`] when there is one, and `Err` — consuming
/// the device — when a trailer is present but unusable.
pub fn build_verified(
    device: KBox<dyn BlockDevice + Send + Sync>,
    fs: FsExtent,
) -> Result<(KBox<dyn BlockDevice + Send + Sync>, VerityStatus), VerityError> {
    build_verified_trusting(device, fs, AttestTrust::Persisted)
}

/// As [`build_verified`], but the caller states whether a v2 trailer's
/// persisted bitmap may be trusted this boot: an image never seen marked
/// clean passes [`AttestTrust::NoneThisBoot`].
pub fn build_verified_trusting(
    device: KBox<dyn BlockDevice + Send + Sync>,
    fs: FsExtent,
    trust: AttestTrust,
) -> Result<(KBox<dyn BlockDevice + Send + Sync>, VerityStatus), VerityError> {
    let Some(header) = read_header(&*device, fs)? else {
        return Ok((device, VerityStatus::Absent));
    };

    let hashes = load_hashes(&*device, &header)?;
    let (attested, status) = if header.version == VERITY_VERSION_ATTESTED {
        let map = load_bitmap(&*device, &header, trust)?;
        let status = VerityStatus::VerifiedWritable {
            blocks: header.block_count,
            block_size: header.block_size,
            attested: map.count_attested(),
        };
        (Some(map), status)
    } else {
        let status = VerityStatus::Verified {
            blocks: header.block_count,
            block_size: header.block_size,
        };
        (None, status)
    };
    wrap_device(device, &header, hashes, attested).map(|dev| (dev, status))
}

/// Its own frame: the wrapper's temporaries must not share one with the
/// trailer parse.
#[inline(never)]
fn wrap_device(
    device: KBox<dyn BlockDevice + Send + Sync>,
    header: &TrailerHeader,
    hashes: KVec<u32>,
    attested: Option<AttestBitmap>,
) -> Result<KBox<dyn BlockDevice + Send + Sync>, VerityError> {
    let wrapped = KBox::try_new(VerifiedBlockDevice {
        inner: device,
        block_size: header.block_size,
        hashes,
        attested,
    })
    .map_err(|_| VerityError::OutOfMemory)?;
    Ok(wrapped)
}

struct TrailerHeader {
    version: u32,
    block_size: u32,
    block_count: u64,
    root: u32,
    bitmap_crc: u32,
}

impl TrailerHeader {
    fn bitmap_bytes(&self) -> u64 {
        if self.version == VERITY_VERSION_ATTESTED {
            self.block_count.div_ceil(8)
        } else {
            0
        }
    }
}

/// `Ok(None)` when the device's tail is not a trailer: it lies inside the
/// filesystem's own extent, or carries no magic.
fn read_header(
    device: &dyn BlockDevice,
    fs: FsExtent,
) -> Result<Option<TrailerHeader>, VerityError> {
    let cap = device.capacity();
    let fs_bytes = fs.bytes().ok_or(VerityError::Geometry)?;
    if cap < HEADER_SIZE || cap - HEADER_SIZE < fs_bytes {
        return Ok(None);
    }

    let mut hdr = [0u8; HEADER_SIZE as usize];
    device
        .read_at(cap - HEADER_SIZE, &mut hdr)
        .map_err(|_| VerityError::Device)?;

    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != VERITY_MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let algo = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let block_size = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
    let block_count = u64::from_le_bytes([
        hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
    ]);
    let root = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);
    let bitmap_crc = u32::from_le_bytes([hdr[28], hdr[29], hdr[30], hdr[31]]);

    let known_version = version == VERITY_VERSION_PROTECTED || version == VERITY_VERSION_ATTESTED;
    if !known_version || algo != VERITY_ALGO_CRC32 || block_size == 0 {
        klog_info!(
            "verity: unsupported trailer (version {} algo {} block_size {})",
            version,
            algo,
            block_size,
        );
        return Err(VerityError::UnsupportedTrailer);
    }

    let header = TrailerHeader {
        version,
        block_size,
        block_count,
        root,
        bitmap_crc,
    };

    let arr_bytes = block_count.checked_mul(4).ok_or(VerityError::Geometry)?;
    let data_bytes = block_count
        .checked_mul(block_size as u64)
        .ok_or(VerityError::Geometry)?;
    let needed = data_bytes
        .checked_add(arr_bytes)
        .and_then(|v| v.checked_add(header.bitmap_bytes()))
        .and_then(|v| v.checked_add(HEADER_SIZE))
        .ok_or(VerityError::Geometry)?;
    if needed > cap {
        klog_info!(
            "verity: trailer claims {} blocks of {} bytes, which does not fit a {}-byte device",
            block_count,
            block_size,
            cap,
        );
        return Err(VerityError::Geometry);
    }
    // The cache reads whole filesystem blocks, and only a block the read fully
    // contains is verified: a trailer with a larger block than the filesystem
    // would verify nothing, silently. Fewer blocks than the filesystem would
    // leave its tail unverified.
    if block_size != fs.block_size || block_count < fs.blocks {
        klog_info!(
            "verity: trailer covers {} blocks of {} bytes, filesystem is {} of {}",
            block_count,
            block_size,
            fs.blocks,
            fs.block_size,
        );
        return Err(VerityError::Geometry);
    }

    Ok(Some(header))
}

fn load_hashes(device: &dyn BlockDevice, header: &TrailerHeader) -> Result<KVec<u32>, VerityError> {
    let n = usize::try_from(header.block_count).map_err(|_| VerityError::Geometry)?;
    let arr_bytes = n.checked_mul(4).ok_or(VerityError::Geometry)?;
    let arr_off = device.capacity() - HEADER_SIZE - header.bitmap_bytes() - arr_bytes as u64;

    let mut bytes = KVec::<u8>::zeroed(arr_bytes).map_err(|_| VerityError::OutOfMemory)?;
    device
        .read_at(arr_off, bytes.as_mut_slice())
        .map_err(|_| VerityError::Device)?;
    if crc32(bytes.as_slice()) != header.root {
        klog_info!("verity: hash-array root mismatch (corrupt trailer)");
        return Err(VerityError::CorruptTrailer);
    }

    let mut hashes = KVec::<u32>::with_capacity(n).map_err(|_| VerityError::OutOfMemory)?;
    for chunk in bytes.as_slice().chunks_exact(4) {
        let h = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        hashes.push(h).map_err(|_| VerityError::OutOfMemory)?;
    }
    Ok(hashes)
}

fn load_bitmap(
    device: &dyn BlockDevice,
    header: &TrailerHeader,
    trust: AttestTrust,
) -> Result<AttestBitmap, VerityError> {
    let len = usize::try_from(header.bitmap_bytes()).map_err(|_| VerityError::Geometry)?;
    let offset = device.capacity() - HEADER_SIZE - header.bitmap_bytes();

    let mut bits = KVec::<u8>::zeroed(len).map_err(|_| VerityError::OutOfMemory)?;
    device
        .read_at(offset, bits.as_mut_slice())
        .map_err(|_| VerityError::Device)?;
    if trust == AttestTrust::NoneThisBoot {
        klog_info!("verity: image not marked clean — no block is attested this boot");
        bits.as_mut_slice().fill(0);
    } else if crc32(bits.as_slice()) != header.bitmap_crc {
        // A torn bitmap is a crash, not an attack: attest nothing, but do not
        // refuse the mount.
        klog_info!("verity: attested bitmap CRC mismatch — no block is attested this boot");
        bits.as_mut_slice().fill(0);
    }

    let map = AttestBitmap {
        bits: SpinLock::new(
            bits,
            lock_class!("VerifiedBlockDevice.attested", LOCK_LEVEL_RESOURCE),
        ),
        dirty: AtomicBool::new(false),
        offset,
        crc_offset: device.capacity() - HEADER_SIZE + HEADER_BITMAP_CRC_OFF,
        blocks: header.block_count,
    };
    // Cleared here too, not only by `gen_verity.py`: a bitmap the kernel
    // writes back must never claim the superblock's block.
    map.un_attest(
        EXT2_SUPERBLOCK_OFFSET,
        EXT2_SUPERBLOCK_LEN,
        header.block_size as u64,
    );
    map.dirty.store(false, Ordering::Release);
    Ok(map)
}
