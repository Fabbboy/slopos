#!/usr/bin/env python3
"""Append a SlopOS block-integrity ("verity") trailer to a raw ext2 image.

Layout (little-endian), appended after the existing image content:

    [ existing image: N full blocks ][ hash array: N × u32 ][ 32-byte header ]

The 32-byte header is the LAST 32 bytes of the file, so the kernel locates the
trailer from the block device's capacity alone — no filesystem parsing. Each
entry of the hash array is the CRC-32 (IEEE/zlib, the same `zlib.crc32` used
here) of the corresponding 4 KiB data block; the header's `root` field is the
CRC-32 of the whole hash array, so a corrupt array is self-detecting.

Kernel side: fs/src/verity.rs (must keep the header layout + CRC in sync).

This is an INTEGRITY check (detects accidental corruption / tampering loudly at
read time), not a cryptographic authenticity guarantee — see verity.rs docs.
"""

import struct
import sys
import zlib

MAGIC = 0x53565254  # 'TVRS' LE — SlopOS verity
VERSION = 1
ALGO_CRC32 = 1
HEADER_FMT = "<IIIIQII"  # magic, version, algo, block_size, block_count(u64), root, reserved

EXT2_SUPERBLOCK_OFFSET = 1024
EXT2_MAGIC = 0xEF53


def ext2_block_size(data: bytes) -> int:
    """Derive the block size from the ext2 superblock so the verity trailer
    always matches the filesystem geometry — never hard-coded, so a change to
    `mkfs.ext2 -b` cannot silently desync verity (kernel uses the trailer's
    block_size, and the ext2 cache reads in the superblock's block_size; they
    MUST agree or every read is a partial read and verification no-ops)."""
    sb = EXT2_SUPERBLOCK_OFFSET
    if len(data) < sb + 64:
        raise SystemExit("gen_verity: image too small to contain an ext2 superblock")
    magic = struct.unpack_from("<H", data, sb + 56)[0]  # s_magic
    if magic != EXT2_MAGIC:
        raise SystemExit(f"gen_verity: not an ext2 image (magic {magic:#06x} != {EXT2_MAGIC:#06x})")
    log_block_size = struct.unpack_from("<I", data, sb + 24)[0]  # s_log_block_size
    return 1024 << log_block_size


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: gen_verity.py <image>", file=sys.stderr)
        return 2
    path = sys.argv[1]

    with open(path, "rb") as f:
        data = f.read()

    block_size = ext2_block_size(data)

    if len(data) == 0 or len(data) % block_size != 0:
        print(
            f"gen_verity: image size {len(data)} is not a positive multiple of {block_size}",
            file=sys.stderr,
        )
        return 1

    n = len(data) // block_size
    arr = bytearray()
    for i in range(n):
        block = data[i * block_size : (i + 1) * block_size]
        arr += struct.pack("<I", zlib.crc32(block) & 0xFFFFFFFF)

    root = zlib.crc32(bytes(arr)) & 0xFFFFFFFF
    header = struct.pack(HEADER_FMT, MAGIC, VERSION, ALGO_CRC32, block_size, n, root, 0)
    assert len(header) == 32, f"header is {len(header)} bytes, expected 32"

    with open(path, "ab") as f:
        f.write(bytes(arr))
        f.write(header)

    print(f"verity: appended trailer for {n} blocks ({block_size}B), root crc 0x{root:08x}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
