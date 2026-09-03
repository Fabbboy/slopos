//! Read-time block-integrity verification for an attested ext2 image.
//!
//! `scripts/gen_verity.py` appends the trailer at the very END of the image,
//! sector-aligned, so the kernel locates it from `device.capacity()` alone.
//!
//! A verified device is **write-protected**, as dm-verity is read-only by
//! design: the hash array describes the bytes the image was built with, and a
//! write would leave a block no trailer describes — unverifiable on this boot
//! and a false integrity failure on the next. Refusing the write at the device
//! is what keeps "verified" true.
//!
//! The filesystem's own extent decides whether the device's tail *is* a
//! trailer. On a writable image the last filesystem block is ordinary data a
//! user can fill, so magic found inside the extent is file contents, never a
//! trailer to refuse the mount over.
//!
//! CRC-32 is an integrity check, not an authenticity one: an adversary who
//! rewrites a block, the hash array and the root is not defeated.

use slopos_ostd::{KBox, KVec, klog_info};

use crate::blockdev::{BlockDevice, BlockDeviceError};

// On-disk trailer (little-endian, appended at the end of the image):
//   [ ext2 region ][ pad to sector ][ hash array: N×u32 ][ 32-byte header ]
//                                                         ^ capacity() - 32
const VERITY_MAGIC: u32 = 0x5356_5254; // 'TVRS' LE — SlopOS verity
const VERITY_VERSION: u32 = 1;
const VERITY_ALGO_CRC32: u32 = 1;
const HEADER_SIZE: u64 = 32;

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

/// CRC-32 (IEEE, reflected) of `data`. `crc32(&[]) == 0`, matching `zlib.crc32`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
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

/// What [`build_verified`] found on the device. Three outcomes rather than a
/// bool, because "no trailer" and "a trailer I refused" must never read alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerityStatus {
    /// No trailer: the image was built `VERITY=off`, and mounts writable.
    Absent,
    /// A valid trailer covers `blocks` blocks; the device is write-protected.
    Verified { blocks: u64, block_size: u32 },
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

/// Verifies each fully-read block against a trusted per-block CRC array and
/// refuses every write.
pub struct VerifiedBlockDevice {
    inner: KBox<dyn BlockDevice + Send + Sync>,
    block_size: u32,
    /// Build-time CRC of every block `0..N`; immutable after mount.
    hashes: KVec<u32>,
}

impl BlockDevice for VerifiedBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)?;

        if buffer.is_empty() {
            return Ok(());
        }

        let bs = self.block_size as u64;
        let n = self.hashes.len() as u64;
        let end = offset + buffer.len() as u64;
        // Only blocks fully contained in the read are verified: sub-block direct
        // reads (the superblock at byte 1024) keep their own sanity checks.
        let mut b = offset.div_ceil(bs);
        while b < n && (b + 1) * bs <= end {
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

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn write_protected(&self) -> bool {
        true
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        Ok(())
    }
}

/// Wrap `device` if it carries a valid verity trailer beyond `fs`.
///
/// Returns the device unchanged with [`VerityStatus::Absent`] when there is
/// no trailer, wrapped with [`VerityStatus::Verified`] when there is one, and
/// `Err` — consuming the device — when a trailer is present but unusable.
pub fn build_verified(
    device: KBox<dyn BlockDevice + Send + Sync>,
    fs: FsExtent,
) -> Result<(KBox<dyn BlockDevice + Send + Sync>, VerityStatus), VerityError> {
    let Some(header) = read_header(&*device, fs)? else {
        return Ok((device, VerityStatus::Absent));
    };

    let hashes = load_hashes(&*device, &header)?;
    let status = VerityStatus::Verified {
        blocks: header.block_count,
        block_size: header.block_size,
    };
    let wrapped = KBox::try_new(VerifiedBlockDevice {
        inner: device,
        block_size: header.block_size,
        hashes,
    })
    .map_err(|_| VerityError::OutOfMemory)?;
    Ok((wrapped, status))
}

struct TrailerHeader {
    block_size: u32,
    block_count: u64,
    root: u32,
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

    if version != VERITY_VERSION || algo != VERITY_ALGO_CRC32 || block_size == 0 {
        klog_info!(
            "verity: unsupported trailer (version {} algo {} block_size {})",
            version,
            algo,
            block_size,
        );
        return Err(VerityError::UnsupportedTrailer);
    }

    let arr_bytes = block_count.checked_mul(4).ok_or(VerityError::Geometry)?;
    let data_bytes = block_count
        .checked_mul(block_size as u64)
        .ok_or(VerityError::Geometry)?;
    let needed = data_bytes
        .checked_add(arr_bytes)
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

    Ok(Some(TrailerHeader {
        block_size,
        block_count,
        root,
    }))
}

fn load_hashes(device: &dyn BlockDevice, header: &TrailerHeader) -> Result<KVec<u32>, VerityError> {
    let n = usize::try_from(header.block_count).map_err(|_| VerityError::Geometry)?;
    let arr_bytes = n.checked_mul(4).ok_or(VerityError::Geometry)?;
    let arr_off = device.capacity() - HEADER_SIZE - arr_bytes as u64;

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
