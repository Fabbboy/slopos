/// Placeholder for embedded ext2 image.
/// The image is now loaded from a virtio-blk device at runtime,
/// so this constant is empty. It will be removed once the virtio-blk
/// driver is fully wired up.
pub const EXT2_IMAGE: &[u8] = &[];
