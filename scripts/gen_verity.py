#!/usr/bin/env python3
"""Append a SlopOS block-integrity ("verity") trailer to a raw ext2 image.

Layout (little-endian), appended after the existing image content:

    v1  [ image: N full blocks ][ pad ][ hash array: N × u32 ][ 32-byte header ]
    v2  [ image: N full blocks ][ pad ][ hash array: N × u32 ]
        [ attested bitmap: ceil(N/8) ][ 32-byte header ]

The 32-byte header is the LAST 32 bytes of the file, so the kernel locates the
trailer from the block device's capacity alone — no filesystem parsing. Each
entry of the hash array is the CRC-32 (IEEE/zlib, the same `zlib.crc32` used
here) of the corresponding 4 KiB data block; the header's `root` field is the
CRC-32 of the whole hash array, so a corrupt array is self-detecting.

Version 2 adds the attested bitmap and spends the header's `reserved` u32 on
its CRC-32: a set bit means the block still matches its build-time hash. A v2
image is a writable root; a v1 image is write-protected outright. A re-run
AND-s an existing v2 bitmap into the new one: the hashes are recomputed from
the current bytes, so re-blessing would vouch for whatever the last boot wrote.

The pad makes the finished file a whole number of 512-byte sectors. A block
device reports its capacity in sectors, so an unpadded trailer whose header
straddled the last partial sector would sit *beyond* the reported capacity and
the kernel would never see it (SLOPOS-2026-0053). The pad goes before the
hash array, never after the header, so the header stays the last 32 bytes, and
neither the hash array nor the bitmap describes it.

Kernel side: fs/src/verity.rs (must keep the header layout + CRC in sync). A
v1 trailer-carrying device is write-protected there: verification and
writability are one decision, as in dm-verity.

This is an INTEGRITY check (detects accidental corruption / tampering loudly at
read time), not a cryptographic authenticity guarantee — see verity.rs docs.
"""

import argparse
import struct
import sys
import zlib

MAGIC = 0x53565254  # 'TVRS' LE — SlopOS verity
ALGO_CRC32 = 1
HEADER_FMT = "<IIIIQII"  # magic, version, algo, block_size, block_count(u64), root, reserved
HEADER_SIZE = 32
SECTOR_SIZE = 512

EXT2_SUPERBLOCK_OFFSET = 1024
EXT2_SUPERBLOCK_LEN = 1024
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


def strip_trailer(data: bytes) -> bytes:
    """Drop a trailer this script already wrote, so a re-run recomputes rather
    than hashing the previous trailer as filesystem data."""
    if len(data) < HEADER_SIZE:
        return data
    magic, version, algo, block_size, block_count, _root, _reserved = struct.unpack(
        HEADER_FMT, data[-HEADER_SIZE:]
    )
    if magic != MAGIC:
        return data
    if version not in (1, 2) or algo != ALGO_CRC32 or block_size == 0:
        raise SystemExit(
            f"gen_verity: the image carries a trailer this script does not understand"
            f" (version {version}, algo {algo}, block_size {block_size})"
        )
    fs_bytes = block_size * block_count
    if fs_bytes == 0 or fs_bytes > len(data) - HEADER_SIZE:
        raise SystemExit(
            f"gen_verity: the existing trailer claims {fs_bytes}B of filesystem,"
            f" which does not fit a {len(data)}B file"
        )
    return data[:fs_bytes]


def superblock_block_range(block_size: int) -> range:
    """The blocks the ext2 superblock lives in. Permanently unattested in v2:
    the kernel rewrites them on every mount."""
    first = EXT2_SUPERBLOCK_OFFSET // block_size
    last = (EXT2_SUPERBLOCK_OFFSET + EXT2_SUPERBLOCK_LEN - 1) // block_size
    return range(first, last + 1)


def attested_bitmap(n: int, block_size: int) -> bytearray:
    """Every block attested except the superblock's. Bit `i` is `1 << (i % 8)`
    of byte `i // 8`; bits past block `n` are zero so the CRC is defined."""
    bitmap = bytearray(b"\xff" * ((n + 7) // 8))
    if n % 8:
        bitmap[-1] = (1 << (n % 8)) - 1
    for block in superblock_block_range(block_size):
        if block < n:
            bitmap[block >> 3] &= ~(1 << (block & 7)) & 0xFF
    return bitmap


def prior_attested(data: bytes) -> bytearray | None:
    """The attested bitmap of a v2 trailer already on `data`, or `None`.

    Must be read before the trailer is stripped. A torn one attests nothing:
    it says nothing about which blocks are still as built.
    """
    if len(data) < HEADER_SIZE:
        return None
    magic, version, _algo, _bs, block_count, _root, bitmap_crc = struct.unpack(
        HEADER_FMT, data[-HEADER_SIZE:]
    )
    if magic != MAGIC or version != 2:
        return None
    length = (block_count + 7) // 8
    end = len(data) - HEADER_SIZE
    if length > end:
        return None
    bitmap = bytearray(data[end - length : end])
    if zlib.crc32(bytes(bitmap)) & 0xFFFFFFFF != bitmap_crc:
        return bytearray(length)
    return bitmap


def carry_forward(fresh: bytearray, prior: bytearray | None) -> bytearray:
    """`fresh` AND `prior` over the blocks both describe. Blocks past the old
    bitmap are new (a grown image), and this build did write them."""
    if prior is None:
        return fresh
    for i in range(min(len(fresh), len(prior))):
        fresh[i] &= prior[i]
    return fresh


def main() -> int:
    parser = argparse.ArgumentParser(description="append a SlopOS verity trailer")
    parser.add_argument("image", help="raw ext2 image to seal in place")
    parser.add_argument(
        "--version",
        type=int,
        choices=(1, 2),
        default=1,
        help="trailer version: 1 = write-protected (default), 2 = writable with an attested bitmap",
    )
    args = parser.parse_args()
    path = args.image
    version = args.version

    with open(path, "rb") as f:
        data = f.read()

    prior = prior_attested(data)
    data = strip_trailer(data)
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
    if version == 2:
        bitmap = bytes(carry_forward(attested_bitmap(n, block_size), prior))
        reserved = zlib.crc32(bitmap) & 0xFFFFFFFF
    else:
        bitmap = b""
        reserved = 0
    header = struct.pack(HEADER_FMT, MAGIC, version, ALGO_CRC32, block_size, n, root, reserved)
    assert len(header) == HEADER_SIZE, f"header is {len(header)} bytes, expected {HEADER_SIZE}"

    unpadded = len(data) + len(arr) + len(bitmap) + len(header)
    pad = (-unpadded) % SECTOR_SIZE

    # `r+b` plus truncate, not `ab`: a re-run replaces the stripped trailer
    # instead of appending a second one.
    with open(path, "r+b") as f:
        f.seek(len(data))
        f.write(b"\0" * pad)
        f.write(bytes(arr))
        f.write(bitmap)
        f.write(header)
        f.truncate()

    total = unpadded + pad
    assert total % SECTOR_SIZE == 0, f"image is {total} bytes, not sector-aligned"

    attested = ""
    if version == 2:
        kept = sum(bin(b).count("1") for b in bitmap)
        attested = f", {kept}/{n} blocks attested (crc 0x{reserved:08x})"
    print(
        f"verity: appended v{version} trailer for {n} blocks ({block_size}B),"
        f" root crc 0x{root:08x}{attested}, {pad}B pad, {total} bytes total"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
