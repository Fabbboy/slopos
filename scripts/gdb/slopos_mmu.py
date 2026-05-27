# slopos_mmu.py — x86-64 page-table tools for SlopOS under QEMU/TCG GDB.
#
# Provides GDB commands:
#   v2p  <cr3> <va>   walk PML4->PDPT->PD->PT, print each level + flags, VA->PA
#   wpva <cr3> <va>   resolve VA->PA, then set a *physical* write watchpoint
#                     (fires regardless of which CR3/alias performs the write —
#                     essential when the corruptor runs in a different address
#                     space or via the kernel HHDM alias than the victim).
#
# Reads guest *physical* memory by toggling the QEMU gdbstub into physical
# address mode (Qqemu.PhyMemMode). If a future QEMU drops that packet, set
# SLOPOS_HHDM in the environment / `set $hhdm = <offset>` and the walker will
# instead read the kernel HHDM alias (phys + hhdm) while a kernel CR3 is live.

import gdb

PRESENT = 1 << 0
RW      = 1 << 1
US      = 1 << 2
PS      = 1 << 7   # huge page
NX      = 1 << 63
ADDR_MASK = 0x000FFFFFFFFFF000


def _phys_mode(on):
    try:
        gdb.execute("maintenance packet Qqemu.PhyMemMode:%d" % (1 if on else 0),
                    to_string=True)
        return True
    except gdb.error:
        return False


def _hhdm():
    # Optional HHDM offset for the fallback physical-read path.
    try:
        return int(gdb.parse_and_eval("$hhdm"))
    except gdb.error:
        return None


def _read_u64_phys(pa, phys_ok):
    inf = gdb.inferiors()[0]
    if phys_ok:
        raw = inf.read_memory(pa, 8)
    else:
        h = _hhdm()
        if h is None:
            raise gdb.error("no PhyMemMode and no $hhdm set; cannot read phys mem")
        raw = inf.read_memory(pa + h, 8)
    return int.from_bytes(bytes(raw), "little")


def _flags(e):
    out = ["P" if e & PRESENT else "-",
           "W" if e & RW else "R",
           "U" if e & US else "S"]
    if e & PS: out.append("PS")
    if e & NX: out.append("NX")
    return "|".join(out)


def walk(cr3, va):
    cr3 &= ~0xFFF
    idx = [(va >> 39) & 0x1FF, (va >> 30) & 0x1FF,
           (va >> 21) & 0x1FF, (va >> 12) & 0x1FF]
    names = ["PML4", "PDPT", "PD  ", "PT  "]
    phys_ok = _phys_mode(True)
    try:
        table = cr3
        for lvl in range(4):
            ent_pa = table + idx[lvl] * 8
            ent = _read_u64_phys(ent_pa, phys_ok)
            gdb.write("  %s[%3d] @phys 0x%012x = 0x%016x [%s]\n"
                      % (names[lvl], idx[lvl], ent_pa, ent, _flags(ent)))
            if not (ent & PRESENT):
                gdb.write("  -> NOT PRESENT\n")
                return None
            if (ent & PS) and lvl == 1:
                pa = (ent & 0x000FFFFFC0000000) | (va & 0x3FFFFFFF)
                gdb.write("  -> 1GiB page  VA 0x%x -> PA 0x%x\n" % (va, pa)); return pa
            if (ent & PS) and lvl == 2:
                pa = (ent & 0x000FFFFFFFE00000) | (va & 0x1FFFFF)
                gdb.write("  -> 2MiB page  VA 0x%x -> PA 0x%x\n" % (va, pa)); return pa
            table = ent & ADDR_MASK
        pa = table | (va & 0xFFF)
        gdb.write("  -> 4KiB page  VA 0x%x -> PA 0x%x\n" % (va, pa))
        return pa
    finally:
        if phys_ok:
            _phys_mode(False)


class V2P(gdb.Command):
    """v2p <cr3> <va> — walk page tables under CR3, print levels, resolve VA->PA."""
    def __init__(self):
        super().__init__("v2p", gdb.COMMAND_USER)

    def invoke(self, arg, from_tty):
        a = gdb.string_to_argv(arg)
        if len(a) != 2:
            gdb.write("usage: v2p <cr3> <va>\n"); return
        cr3 = int(gdb.parse_and_eval(a[0]))
        va = int(gdb.parse_and_eval(a[1]))
        gdb.write("v2p: cr3=0x%x va=0x%x\n" % (cr3, va))
        pa = walk(cr3, va)
        if pa is not None:
            gdb.write("RESULT PA = 0x%x\n" % pa)


class WPVA(gdb.Command):
    """wpva <cr3> <va> — resolve VA->PA under CR3, arm a PHYSICAL write watchpoint.
    Leaves GDB in physical-memory mode so the watchpoint address is physical;
    run 'maintenance packet Qqemu.PhyMemMode:0' before resuming symbol work."""
    def __init__(self):
        super().__init__("wpva", gdb.COMMAND_USER)

    def invoke(self, arg, from_tty):
        a = gdb.string_to_argv(arg)
        if len(a) != 2:
            gdb.write("usage: wpva <cr3> <va>\n"); return
        cr3 = int(gdb.parse_and_eval(a[0]))
        va = int(gdb.parse_and_eval(a[1]))
        pa = walk(cr3, va)
        if pa is None:
            gdb.write("wpva: VA not mapped; cannot watch\n"); return
        if not _phys_mode(True):
            gdb.write("wpva: PhyMemMode unavailable; cannot set physical watchpoint\n"); return
        gdb.execute("watch *(unsigned long*)0x%x" % pa)
        gdb.write("wpva: physical write-watchpoint armed at PA 0x%x\n" % pa)
        gdb.write("wpva: GDB is now in PHYSICAL mode — run "
                  "'maintenance packet Qqemu.PhyMemMode:0' before symbol/stack work.\n")


V2P()
WPVA()
gdb.write("[slopos_mmu] loaded: v2p <cr3> <va>, wpva <cr3> <va>\n")
