# Shrink OSTD's contract-bearing safe API surface

Sixteen safe `pub fn`s in `slopos-ostd` carry a `# Safety` section. A
`# Safety` section on a function that is not `unsafe fn` is a written
admission that a safe caller can break it — the fault then lands in OSTD's
code while the cause is an ordinary safe call in a service crate, which is
the debugging cost of a trusted-core bug without the containment the
trusted core is supposed to buy.

`scripts/check_safe_contract_surface.sh` holds the count at 16 and fails on
growth. Lower the baseline in the same commit as each removal.

This is deliberately not a push to zero. Some obligations cannot be
expressed on the pinned toolchain, and forcing the issue would move them
into undocumented functions, which is strictly worse. Each survivor should
end up with a written reason it cannot be typed.

## The list, by blast radius

| Site | Ext. sites | Direction |
|---|---:|---|
| `util/ptr_buf.rs:44` `nullable_write` | 24 | `Option<&mut MaybeUninit<T>>`, converted once at the syscall adapter rather than per call |
| `util/ptr_buf.rs:118` `anchored_buf` | 5 | Retire. Three of the five pass `&len` — a reference to the caller's own local — so the anchor relates to nothing. The two honest ones pass `self` and want an inherent `as_slice` on the owning struct |
| `util/ptr_buf.rs:169` `install_buf_mut` | 1 | Linear one-shot handle consumed by value. Its caller, `mm/src/page_alloc/buddy.rs:557`, is a boot-time install — exactly that shape. As written it hands out a repeatable `&'static mut` to fixed bytes |
| `dev/mod.rs:102` `borrow_dyn` | 6 | Sealed registration handle. All six are one field in `net/src/netdev.rs`, so the "published once, never freed" property becomes the registry's rather than each caller's |
| `sync/kernel_sync.rs:129` `cell_get_mut` | 4 | Take `&IrqGuard`; the guard's lifetime bounds the `&mut` mechanically. OSTD already uses the idiom at `io/port.rs:285` and `irq/line.rs:217` |
| `util/fn_ptr.rs:53` `fn_ptr_from_raw` | 2 | Delete in favour of `fn_ptr_decode_opt`. Returns a zero-bit-pattern `F` from a null input, which is a null fn pointer handed to safe code |
| `cpu/x86_64/control_regs.rs:354,393` `xcr0_read`/`xcr0_write` | 1 | `Osxsave` token from the CR4 setter. `xcr0_write` additionally needs bit 0 set and only CPUID-reported bits — a bitflags-with-validated-constructor problem |
| `arch/x86_64/safestack.rs:84` `install_ap_trampoline_as` | 1 | The `BspToken<'brand>` is right; the `F: Copy` bound is not — it admits any pointer-sized `Copy` type, and the compile-time size assert catches width but not shape. Needs a sealed ABI trait |
| `task/switch.rs:392` `init_current_context` | 1 | Its caller holds a raw pointer out of per-CPU storage, so a reference does not fit. Likely a documented survivor |
| `util/ptr_buf.rs:201` `anchored_ref`, `:225` `with_atomic_u64_at`, `:256` `nonnull_byte_offset` | 0–2 | `anchored_ref` has no callers; delete. The other two are small |
| `arch/x86_64/naked.rs:128` `__safestack_pointer_address`, `test_support/pcr.rs:38` `bsp_ist_restore`, `task/cell.rs:126` `get_ptr` | 0–1 | LLVM-called or test-support; likely documented survivors |

Keep every `with_*` closure form in `ptr_buf`. The higher-ranked closure
lifetime provably prevents the borrow escaping, which is the correct shape
and not part of this list.

## Also open

`.limine_requests` has no working delimiters. `boot/src/limine_protocol.rs`
emits `[u64; 1] = [0]` for both markers; the protocol wants a `[u64; 4]` /
`[u64; 2]` magic pair, which the pinned `limine` 0.6.3 crate already ships as
`RequestsStartMarker` / `RequestsEndMarker`. Limine full-image-scans today as
a result, and the two `KEEP()` lines in `link.ld` are dead weight. Base
revision 6 requires delimiters to be honoured when present.
