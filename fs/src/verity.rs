//! Read-time block-integrity verification ("fs-verity"/dm-verity-style) for the
//! ext2 root filesystem.
//!
//! A build-time tool (`scripts/gen_verity.py`) appends a trailer to the disk
//! image: a per-block CRC-32 hash array followed by a 32-byte header placed at
//! the very END of the image, so the kernel locates it from
//! `device.capacity()` alone — no filesystem parsing required. At mount,
//! [`build_verified`] wraps the backing block device in a
//! [`VerifiedBlockDevice`] decorator that, on every read, recomputes the CRC of
//! each fully-read block and compares it to the trusted array. A mismatch is
//! reported loudly as [`BlockDeviceError::IntegrityFailure`] instead of
//! silently returning corrupt bytes — the exact failure mode behind the
//! io_capture incident, where a corrupted `.text` block executed as a wild
//! instruction (#UD).
//!
//! ## Threat model (read this before trusting it)
//! This detects **accidental** corruption — a wrong write, host-side tampering
//! of the image, bit-rot, out-of-band DMA — of blocks that have **not been
//! written since mount**. Blocks the filesystem legitimately writes are
//! "re-blessed" (marked written) and no longer verified, mirroring dm-verity's
//! read-only-content guarantee. The hash is a CRC-32 (an *integrity* check, not
//! a *cryptographic authenticity* guarantee): it reliably catches corruption
//! but a determined adversary who rewrites a block AND its hash array AND the
//! root is not defeated. Swapping the CRC for a cryptographic hash plus an
//! externally-anchored root (kernel cmdline) is a localized change isolated to
//! [`crc32`] + [`parse_trailer`].

use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, KVec, klog_info};

use crate::blockdev::{BlockDevice, BlockDeviceError};

// ---------------------------------------------------------------------------
// On-disk trailer format (little-endian, appended at the end of the image)
//
//   [ ext2 filesystem region ][ hash array: N×u32 ][ 32-byte header ]
//                                                    ^ device.capacity() - 32
// ---------------------------------------------------------------------------

const VERITY_MAGIC: u32 = 0x5356_5254; // 'TVRS' LE — SlopOS verity
const VERITY_VERSION: u32 = 1;
const VERITY_ALGO_CRC32: u32 = 1;
const HEADER_SIZE: u64 = 32;

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3 / zlib, reflected, poly 0xEDB88320) — matches Python's
// `zlib.crc32`, which `scripts/gen_verity.py` uses to build the trailer.
// ---------------------------------------------------------------------------

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

/// CRC-32 (IEEE, reflected) of `data`. `crc32(&[]) == 0`, matching `zlib.crc32`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Bitset helpers over the "written since mount" tracking words.
// ---------------------------------------------------------------------------

#[inline]
fn bit_get(words: &[u64], idx: usize) -> bool {
    (words[idx / 64] >> (idx % 64)) & 1 != 0
}

#[inline]
fn bit_set(words: &mut [u64], idx: usize) {
    words[idx / 64] |= 1u64 << (idx % 64);
}

// ---------------------------------------------------------------------------
// VerifiedBlockDevice — the decorator
// ---------------------------------------------------------------------------

/// A [`BlockDevice`] decorator that verifies each fully-read, not-yet-written
/// block against a trusted per-block CRC array (see module docs).
pub struct VerifiedBlockDevice {
    inner: KBox<dyn BlockDevice + Send + Sync>,
    block_size: u32,
    /// Build-time CRC of every block `0..N`; immutable after mount.
    hashes: KVec<u32>,
    /// Bit `i` set ⇒ block `i` was written since mount and is no longer
    /// verified (the FS owns its mutable blocks). `&self` trait methods force
    /// interior mutability; access is also serialized by the FS's outer lock.
    written: SpinLock<KVec<u64>>,
}

impl BlockDevice for VerifiedBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)?;

        if buffer.is_empty() || self.hashes.is_empty() {
            return Ok(());
        }

        let bs = self.block_size as u64;
        let n = self.hashes.len() as u64;
        let end = offset + buffer.len() as u64;
        // First block whose start lies at or after `offset` (i.e. the first
        // block fully contained in the read). Partially-read edge blocks are
        // skipped — the ext2 cache always reads whole blocks, so every block
        // that matters is verified; only sub-block direct reads (e.g. the
        // superblock at byte 1024) fall through, and those have their own
        // magic/sanity checks.
        let mut b = offset.div_ceil(bs);
        let written = self.written.lock();
        while b < n && (b + 1) * bs <= end {
            let idx = b as usize;
            if !bit_get(written.as_slice(), idx) {
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
            }
            b += 1;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        // The device write blocks (scheduler-backed completion wait), so it
        // must run OUTSIDE the spinning `written` lock. Marking happens
        // after, regardless of the result: a failed write leaves the block
        // content unknown, so its build-time CRC must no longer be enforced
        // either way.
        let result = self.inner.write_at(offset, buffer);
        if !buffer.is_empty() && !self.hashes.is_empty() {
            let bs = self.block_size as u64;
            let n = self.hashes.len() as u64;
            let end = offset + buffer.len() as u64;
            let first = offset / bs;
            let last = (end - 1) / bs;
            let mut written = self.written.lock();
            let mut b = first;
            while b <= last && b < n {
                bit_set(written.as_mut_slice(), b as usize);
                b += 1;
            }
        }
        result
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        // Verity has no durability state of its own; forward the barrier to the
        // backing device. The `written` bitset is in-memory metadata only.
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Mount-time construction
// ---------------------------------------------------------------------------

/// Wrap `device` in a [`VerifiedBlockDevice`] if it carries a valid verity
/// trailer; otherwise return it unchanged (verity is optional — images without
/// a trailer mount unverified). The trailer is self-anchored: the stored root
/// CRC must match a recomputation over the hash array, so a corrupt trailer
/// disables verity (logged) rather than blocking the mount.
pub fn build_verified(
    device: KBox<dyn BlockDevice + Send + Sync>,
) -> KBox<dyn BlockDevice + Send + Sync> {
    let Some((block_size, hashes, written)) = parse_trailer(&*device) else {
        return device;
    };

    match KBox::try_new(VerifiedBlockDevice {
        inner: device,
        block_size,
        hashes,
        written: SpinLock::new(
            written,
            lock_class!("VerifiedBlockDevice.written", LOCK_LEVEL_RESOURCE),
        ),
    }) {
        Ok(boxed) => boxed,
        // Unreachable in practice: the much larger hash-array allocation in
        // `parse_trailer` already succeeded, so this ~80-byte box will too.
        // If it truly cannot, the system is out of memory at mount — a fatal,
        // unrecoverable condition (the device handle is consumed and cannot be
        // recovered from a failed allocation), surfaced like other boot-time
        // fatal hardware/memory faults.
        Err(_) => panic!("verity: out of memory wrapping the root block device at mount"),
    }
}

/// Read and validate the trailer from the end of `device`. Returns
/// `(block_size, hashes, fresh-zeroed written-bitset)` on success.
fn parse_trailer(device: &dyn BlockDevice) -> Option<(u32, KVec<u32>, KVec<u64>)> {
    let cap = device.capacity();
    if cap < HEADER_SIZE {
        return None;
    }

    let mut hdr = [0u8; HEADER_SIZE as usize];
    device.read_at(cap - HEADER_SIZE, &mut hdr).ok()?;

    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != VERITY_MAGIC {
        // No verity trailer — mount unverified.
        return None;
    }
    let version = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let algo = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let block_size = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
    let block_count = u64::from_le_bytes([
        hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
    ]);
    let root = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);

    if version != VERITY_VERSION || algo != VERITY_ALGO_CRC32 || block_size == 0 {
        klog_info!(
            "verity: unsupported trailer (version {} algo {} block_size {}) — disabling",
            version,
            algo,
            block_size,
        );
        return None;
    }

    let n = block_count as usize;
    let arr_bytes = n.checked_mul(4)?;
    let arr_off = cap
        .checked_sub(HEADER_SIZE)?
        .checked_sub(arr_bytes as u64)?;

    // Allocating the (large) hash array is the failure point under memory
    // pressure; on failure we disable verity rather than abort the mount.
    let mut bytes = KVec::<u8>::zeroed(arr_bytes).ok()?;
    device.read_at(arr_off, bytes.as_mut_slice()).ok()?;
    if crc32(bytes.as_slice()) != root {
        klog_info!("verity: hash-array root mismatch (corrupt trailer) — disabling");
        return None;
    }

    let mut hashes = KVec::<u32>::with_capacity(n).ok()?;
    let mut i = 0usize;
    while i < n {
        let o = i * 4;
        let h = u32::from_le_bytes([
            bytes.as_slice()[o],
            bytes.as_slice()[o + 1],
            bytes.as_slice()[o + 2],
            bytes.as_slice()[o + 3],
        ]);
        hashes.push(h).ok()?;
        i += 1;
    }

    let words = n.div_ceil(64).max(1);
    let mut written = KVec::<u64>::with_capacity(words).ok()?;
    let mut w = 0usize;
    while w < words {
        written.push(0u64).ok()?;
        w += 1;
    }

    klog_info!(
        "verity: enabled — {} blocks of {} bytes, root crc {:#010x}",
        n,
        block_size,
        root,
    );
    Some((block_size, hashes, written))
}
