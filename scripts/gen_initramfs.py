#!/usr/bin/env python3
"""Pack a SlopOS initramfs as an uncompressed `newc` (SVR4) cpio archive.

Usage: gen_initramfs.py <out.cpio> <build_dir> <bin1> [bin2] ...

Mirrors scripts/build_fs_image.sh's argv and layout so the RAM root and the
ext2 disk image never drift: each binary lands at /bin/<name> except `init`
which goes to /sbin/init; fonts go to /usr/share/fonts; the wallpaper to
/usr/share/slopos/wallpapers/default.png.

Only file records are emitted — the kernel's cpio loader auto-creates parent
directories, and /tmp, /dev and /mnt are mount-point overlays rather than real
root entries, so they are deliberately absent here. No host `cpio` tool is
required (we emit the format directly, like Linux's gen_init_cpio).
"""

import os
import sys

# st_mode values: type bits | permission bits.
S_IFREG = 0o100000
MODE_EXEC = S_IFREG | 0o755  # binaries
MODE_DATA = S_IFREG | 0o644  # fonts, wallpaper

# Mirror the kernel's per-component name cap (fs/src/lib.rs MAX_NAME_LEN).
MAX_NAME_LEN = 32


def pad4(buf: bytearray) -> None:
    """NUL-pad to a 4-byte boundary. Every record starts 4-aligned, so the
    running length reflects each record's internal alignment."""
    while len(buf) % 4 != 0:
        buf.append(0)


def emit(buf: bytearray, name: bytes, mode: int, data: bytes) -> None:
    name_nul = name + b"\x00"
    fields = (
        0,            # c_ino
        mode,         # c_mode
        0, 0,         # c_uid, c_gid
        1,            # c_nlink
        0,            # c_mtime
        len(data),    # c_filesize
        0, 0,         # c_devmajor, c_devminor
        0, 0,         # c_rdevmajor, c_rdevminor
        len(name_nul),  # c_namesize (includes the trailing NUL)
        0,            # c_check (always 0 for newc)
    )
    buf += b"070701"
    buf += b"".join(b"%08X" % f for f in fields)
    buf += name_nul
    pad4(buf)
    buf += data
    pad4(buf)


def validate(path: bytes) -> None:
    for comp in path.lstrip(b"/").split(b"/"):
        if len(comp) > MAX_NAME_LEN:
            sys.exit(
                f"initramfs: path component exceeds {MAX_NAME_LEN} bytes: "
                f"{path.decode('utf-8', 'replace')}"
            )


def read_file(path: str) -> bytes:
    with open(path, "rb") as f:
        return f.read()


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit("Usage: gen_initramfs.py <out.cpio> <build_dir> <bin1> [bin2] ...")
    out_path = sys.argv[1]
    build_dir = sys.argv[2]
    bins = sys.argv[3:]

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    entries: list[tuple[bytes, int, bytes]] = []

    for name in bins:
        src = os.path.join(build_dir, name + ".elf")
        if not os.path.isfile(src):
            sys.exit(f"Missing userland binary: {src}")
        dest = b"/sbin/init" if name == "init" else b"/bin/" + name.encode()
        entries.append((dest, MODE_EXEC, read_file(src)))

    fonts_dir = os.path.join(repo_root, "assets", "fonts")
    if os.path.isdir(fonts_dir):
        for fname in sorted(os.listdir(fonts_dir)):
            if not fname.endswith(".ttf"):
                continue
            dest = b"/usr/share/fonts/" + fname.encode()
            entries.append((dest, MODE_DATA, read_file(os.path.join(fonts_dir, fname))))

    logo = os.path.join(repo_root, "assets", "logo.png")
    if os.path.isfile(logo):
        dest = b"/usr/share/slopos/wallpapers/default.png"
        entries.append((dest, MODE_DATA, read_file(logo)))

    buf = bytearray()
    for dest, mode, data in entries:
        validate(dest)
        emit(buf, dest, mode, data)
    emit(buf, b"TRAILER!!!", 0, b"")

    out_dir = os.path.dirname(out_path)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(out_path, "wb") as f:
        f.write(buf)

    print(f"initramfs: wrote {out_path} ({len(buf)} bytes, {len(entries)} files)")


if __name__ == "__main__":
    main()
