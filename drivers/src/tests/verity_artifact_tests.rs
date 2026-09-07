//! The shipped verified image, mounted from the device the test harness
//! attaches it on.
//!
//! `fs/assets/ext2.img` is what `just boot` runs from, and it is the only
//! image that carries a verity trailer: the tests image on disk0 is built
//! `VERITY=off` so the suite can write to it. Without this test no `just
//! test` run would exercise `fs/src/verity.rs` against a trailer a real block
//! device reports — which is exactly how SLOPOS-2026-0053 stayed invisible.

use slopos_fs::blockdev::{BlockDevice, BlockDeviceIndex};
use slopos_fs::ext2::cache::BlockCache;
use slopos_fs::ext2::{Ext2Error, Ext2Fs};
use slopos_fs::verity::{FsExtent, VerityStatus, build_verified};
use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{fail, pass};

use crate::virtio_blk;

/// virtio-disk2 in `scripts/qemu_run.sh`'s test mode: the shipped verified
/// image, attached `snapshot=on`.
const VERIFIED: BlockDeviceIndex = BlockDeviceIndex(2);

fn extent_of(device: &dyn BlockDevice) -> Result<FsExtent, TestResult> {
    match Ext2Fs::mount_params(device) {
        Ok((sb, bs, _)) => Ok(FsExtent {
            block_size: bs,
            blocks: sb.blocks_count as u64,
        }),
        Err(e) => Err(fail!("disk2 superblock unreadable: {:?}", e)),
    }
}

fn claim_verified() -> Result<KBox<dyn BlockDevice + Send + Sync>, TestResult> {
    let Some(handle) = virtio_blk::blk_device_by_index(VERIFIED) else {
        return Err(fail!(
            "verified image (disk2) not attached — is fs/assets/ext2.img built?"
        ));
    };
    if !virtio_blk::blk_is_ready(handle) {
        return Err(fail!("disk2 present but not ready"));
    }
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return Err(fail!("could not claim disk2: {:?}", e)),
    };
    match KBox::try_new(token) {
        Ok(boxed) => Ok(boxed),
        Err(_) => Err(fail!("out of memory boxing the disk2 handle")),
    }
}

/// The artifact's trailer must be *reachable* through a sector-granular
/// capacity — the whole point of SLOPOS-2026-0053 — and the wrapped device
/// must refuse writes.
pub fn test_verity_artifact_trailer_reachable() -> TestResult {
    let device = match claim_verified() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let cap = device.capacity();
    if cap % 512 != 0 {
        return fail!("virtio capacity {} is not sector-granular", cap);
    }
    let extent = match extent_of(&*device) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let (device, status) = match build_verified(device, extent) {
        Ok(parts) => parts,
        Err(e) => return fail!("build_verified refused the shipped image: {:?}", e),
    };
    let VerityStatus::Verified { blocks, block_size } = status else {
        return fail!(
            "the shipped image mounted with no trailer: the header sits beyond capacity {}",
            cap
        );
    };
    if block_size != 4096 || blocks == 0 {
        return fail!(
            "unexpected trailer geometry: {} blocks of {}",
            blocks,
            block_size
        );
    }
    if !device.write_protected() {
        return fail!("a verified device must be write-protected");
    }
    if device.write_at(0, &[0u8; 512]).is_ok() {
        return fail!("a verified device accepted a write");
    }
    pass!()
}

#[inline(never)]
fn mount_and_probe(device: &(dyn BlockDevice + Send + Sync)) -> TestResult {
    let (sb, bs, is) = match Ext2Fs::mount_params(device) {
        Ok(v) => v,
        Err(e) => return fail!("mount_params: {:?}", e),
    };
    let mut cache = match BlockCache::new_boxed(bs) {
        Ok(c) => c,
        Err(e) => return fail!("BlockCache::new: {:?}", e),
    };
    let mut fs = match Ext2Fs::new(device, &mut cache, sb, bs, is) {
        Ok(f) => f,
        Err(e) => return fail!("Ext2Fs::new: {:?}", e),
    };
    if !fs.is_read_only() {
        return fail!("ext2 over a verified device must come up read-only");
    }
    match fs.resolve_path(b"/sbin/init") {
        Ok(_) => {}
        Err(e) => return fail!("/sbin/init unreadable through verity: {:?}", e),
    }
    match fs.create_file(2, b"verity_probe") {
        Err(Ext2Error::ReadOnly) => {}
        other => return fail!("want ReadOnly, got {:?}", other.map(|_| ())),
    }
    if fs.dirty_count() != 0 {
        return fail!("the read-only mount dirtied {} blocks", fs.dirty_count());
    }
    pass!()
}

/// End to end: the shipped image mounts read-only, its binaries read through
/// verification, and a create is refused before it dirties anything.
pub fn test_verity_artifact_mounts_read_only() -> TestResult {
    let device = match claim_verified() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let extent = match extent_of(&*device) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let (device, status) = match build_verified(device, extent) {
        Ok(parts) => parts,
        Err(e) => return fail!("build_verified refused the shipped image: {:?}", e),
    };
    if status == VerityStatus::Absent {
        return fail!("the shipped image carries no trailer");
    }
    mount_and_probe(&*device)
}

slopos_testing::stest!(
    name = test_verity_artifact_trailer_reachable,
    suite = verity_artifact
);
slopos_testing::stest!(
    name = test_verity_artifact_mounts_read_only,
    suite = verity_artifact
);
