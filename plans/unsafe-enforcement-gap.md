# Close the gap between the framekernel claim and its enforcement

`README.md` says the compiler itself refuses to let unsafety leak out of the
trusted core. It does not. `#![forbid(unsafe_code)]` does not reject `unsafe`
injected by a macro defined in another crate, and
`scripts/check_unsafe_outside_ostd.sh` is a source scan for the literal keyword,
which macro call sites do not contain. Both enforcement mechanisms are
structurally blind to the same construct.

Nothing here is currently exploitable by an attacker. Everything here is a false
statement in the document the safety argument rests on, or a hole a future patch
can walk through without review noticing.

## The mechanism, established empirically

Crate A:

```rust
#[macro_export]
macro_rules! poke { ($p:expr, $v:expr) => {{ unsafe { core::ptr::write($p, $v) } }}; }
```

Crate B, with `#![forbid(unsafe_code)]`, calling `lib_a::poke!(p, 7)`: compiles
with exit 0 and zero diagnostics on the pinned toolchain. The *same* macro body
defined in the *same* crate does trip `forbid`. The suppressor is crate-externality
of the macro definition, not the token's syntactic position.

The cause is not `UnsafeCode::report_unsafe`, which only early-returns on
`span.allows_unsafe()`. It is the generic lint-emission path in
`rustc_middle::lint::lint_level`, which drops any diagnostic whose primary span
satisfies `in_external_macro` unless the lint declares `@report_in_external_macro`.
`UNSAFE_CODE` does not declare it. The diagnostic never reaches the `forbid`
level check at all.

`#[allow_internal_unsafe]` is the explicit opt-in marker and works on the pinned
toolchain, but it requires `#![feature(allow_internal_unsafe)]` and emits a
warn-by-default `internal_features` diagnostic. No OSTD macro carries it today.

## What is actually out there

| Class | What | Sites | Where |
|---|---|---:|---|
| A | executable `unsafe {}` blocks | 115 | net 69, drivers 46 |
| B | `unsafe impl` / `unsafe fn` bodies | 26 | sched 17, hermetic 7, other 2 |
| C | `unsafe extern "C" { … }` blocks | 5 | boot, drivers ×2, hermetic, sched |
| D | attribute-form `#[unsafe(...)]` | ~2,800 | tree-wide |

Class D is the surprise. It is not the ~30 that `link_section_static!` and
friends account for directly: `stest!` is defined in `ktesting`, which all three
gate scripts exempt wholesale as "userland-side", and it emits
`#[unsafe(link_section = ".test_registry")]` at **2,707 call sites across forbid
kernel crates**. `boot_init!` adds 45, `utest!` 30, `pci_driver!` 11,
`platform_driver!` 4.

There are also second-order macros *defined in forbid crates* that re-export
unsafe-injecting OSTD macros — `core/src/lib.rs:50`,
`boot/src/early_init.rs:107`, `drivers/src/pci.rs:292`,
`drivers/src/platform_bus/mod.rs:202`. Any registry- or call-site-based gate
must resolve transitively or it undercounts by ~90 sites.

## Which obligations are actually undischarged

This classification is what the plan turns on. Most of the 115 executable
injections are **sound**, and rewriting them would be busywork:

- `write_field!` — discharged. `SlotPtr::from_raw` is `pub unsafe fn`, so a forbid
  crate cannot fabricate one; the field path and value both type-check.
- `write_array_field!` — discharged. `addr_of_mut!((*p)[i])` retains the array
  bounds check: a const-known out-of-range index is a compile error
  (`unconditional_panic`), otherwise a runtime panic. Verified directly.
- `write_init_field!` — discharged. `Init` is an unsafe trait.
- `__hermetic_register!` — discharged. OSTD fixes the section name.

The genuinely undischarged ones are a short list:

- **`zero_field!`** has no `Zeroable` bound. Its obligation — the all-zero pattern
  is a valid value for the field's type — is discharged only by a comment at each
  call site.
- **`hermetic_state!` / `declare_pcr_stack_type!`** — the invoking crate asserts
  the obligation in prose. `pcr_ty.rs`'s own module doc justifies the macro by
  saying an invocation lands "in a file the unsafe gate and code review both look
  at". The unsafe gate does not look at it; the invocation carries no keyword.
- **`extern_block!` / `no_mangle_static!` / `extern_c_entry!` /
  `link_section_static!`** — the invoking crate supplies the symbol or section
  name, and nothing checks the declared type against the real symbol.

## The largest hole is not a macro

`init_struct_with` (`slopos-ostd/src/mm/init.rs:359`) is a **safe `pub fn`** with a
`# Safety` doc section. It manufactures an `Init<T, E>` — an unsafe trait a forbid
crate cannot implement — out of an ordinary safe closure. So a forbid crate can
write

```rust
KBox::try_init(init_struct_with(|_slot| Ok(())))
```

and obtain a fully uninitialised `T`, with no macro and no `unsafe` token
anywhere. `KBox::try_init` and `PinBox::try_init` are safe and call
`init.__init(slot)` then `assume_init`. There are 13 call sites outside OSTD.

Fifteen more safe `pub fn`s carrying `# Safety` sections sit beside it, plus
`memory::memcpy`, which has no contract at all. `util::ptr_buf`'s own module doc
concedes the point: *"the only thing that moves is the `unsafe` keyword's
location."*

This is the real finding. A safe function with a prose safety contract is exactly
as unsound as an exported `unsafe` block, and it is invisible to both the lint and
the gate.

## Plan

### 1. Fix the claims (one day, zero risk, do first)

Correct `README.md`, `AGENTS.md` and `verification/STATUS.md` to state what is
actually mechanised: every line of `unsafe` is *authored* in `slopos-ostd`, and no
kernel crate may write one — which is true and is a strong claim — rather than
that no unsafe executes outside it, which is false. State the macro-injection
carve-out and the safe-fn-with-prose-contract surface explicitly.

The same pass should close six other claims that the tree does not support. They
are grouped here because they share a root cause — the documentation describes the
discipline as intended rather than as enforced — and because fixing them
individually would scatter one afternoon's work across six commits.

- **The vendored annexes are absent from the safety story.** `vendor/unwinding` and
  `vendor/gimli` are 58,721 lines carrying 197 lines of `unsafe`, they link into
  `kernel.elf`, they are exempt from the unsafe gate, and `STATUS.md` does not
  mention them.
- **"Inv. 1–10" is defined nowhere in the repository.** It is the vocabulary every
  SAFETY comment and the entire audited-only classification is written in, and only
  24 of 651 SAFETY comments name one. Either write the invariant list down or stop
  referring to it as though it exists.
- **`AGENTS.md` says task-ownership invariants I1–I7 are machine-checked.** The
  proof checks a different set, T1–T7, and `STATUS.md` explicitly places four of
  the seven out of model. Reconcile the two namings.
- **The TCB ratio compares unlike units.** `STATUS.md` puts SlopOS's
  unsafe-line-*density* (0.53 %) in the same table column as Asterinas's and
  Theseus's crate/code-size TCB fractions. A like-for-like crate-granular recount
  is materially higher — the honest statement is that the gap is far smaller than
  the table implies, possibly zero, not a specific counter-number, because the
  published comparators are themselves measured differently (Asterinas's ~14 % is
  post-LTO linked code size). `scripts/tcb_ratio.sh:16-32` states the line-based
  definition; `README.md:136-142` gives no metric qualifier at all and should.
- **The metric measures the wrong thing.** Density of `unsafe` keywords is not TCB
  size. OSTD exports 1,406 safe public functions against 60 unsafe ones, and it is
  the safe ones with prose contracts (item 4 below) that determine how much must be
  trusted. Whatever replaces the headline number should count *surface*, not
  keywords — otherwise the metric rewards moving `unsafe` behind a safe wrapper,
  which is precisely the pattern this plan exists to discourage.
- **"Linux-ABI" overstates what is shipped.** Syscall numbering is bespoke, so a
  Linux binary calling `read()` invokes yield. ABI-*shaped* is true and worth
  claiming; ABI-*compatible* is not.
- **Three modules filed under "unaudited — pure safe Rust, no `unsafe`"** contain 77
  lines of it between them, including fn-pointer transmutes on the klog path and
  raw CPU/GDT/page manipulation in the scaffolding all 2716 tests run under.

A false claim in the safety document is worse than the gap it hides, because it is
what stops the gap from being found again.

### 2. Gate what the source scan cannot see

The source scan is structurally blind; a second mechanism is needed, not a better
regex. This project already pays the "inspect the real artifact" cost twice —
`check_stack_sizes.sh` and `check_kernel_softfloat.sh` both gave up on source
heuristics for exactly this reason.

Build `scripts/check_unsafe_expansion.sh`: expand each forbid crate
(`-Zunpretty=expanded`), scan the expansion for `unsafe`, and diff against a
checked-in per-crate expected count with the injecting macro named. Growth fails
the build; a new injector fails the build until it is classified and recorded.

This is the only mechanism that is complete against *future* injectors, which is
the property that matters — the current 115 are sound, and the risk is the 116th.

Mark every intentionally-injecting OSTD macro `#[allow_internal_unsafe]` as a
byproduct. It documents intent at the definition and makes the set enumerable,
but it is not a substitute for the gate: it suppresses nothing that is not already
suppressed.

While here, stop exempting `ktesting` as "userland-side". It is a normal,
non-optional dependency of every kernel subsystem, it ships in every kernel image,
it carries 11 unconditional `unsafe` sites, and it is the only kernel-image crate
missing `#![forbid(unsafe_code)]`. `scripts/kernel_crates.sh:9-11` documents the
opposite of what it computes.

### 3. Close the undischarged obligations

- `zero_field!` — add a `Zeroable` bound. The trait already exists. This converts
  a prose contract into a type-system check for both live call sites.
- `hermetic_state!` / `declare_pcr_stack_type!` — either seal the trait so only
  OSTD-blessed types can implement it, or accept the obligation and record the
  invocation sites in the phase-2 registry so review sees them.
- `extern_block!` and the symbol-supplying family — the obligation is that the
  declared type matches the real symbol. That is checkable: cross-reference the
  declared names against the ELF symbol table in a post-link gate, in the same
  place the other ELF gates already run.

### 4. Shrink the safe-unsafe surface

Audit all sixteen safe `pub fn`s carrying `# Safety` sections plus
`memory::memcpy`. For each, decide: can it be replaced by a type-safe API — a
slice, a handle, a guard — or must the contract remain?

`init_struct_with` is the priority. Its contract is "initialise every field", and
that is expressible: require the closure to return a witness the caller cannot
forge, or make the macro family the only way to construct one and seal the
closure form. Thirteen call sites outside OSTD is a tractable migration.

`ptr_buf` is the next. Its own doc admits the keyword merely moves; the question
is whether its 73 call sites can be served by a bounded accessor instead.

### 5. Not now

Do not rewrite the 105 `write_field!` sites to `offset_of!`. They are already
sound, the only benefit is making the gate's number smaller, and constructing
large structs by value is exactly what the 2 KiB stack ceiling forbids.

## Phases

| # | Work | Done when |
|---|---|---|
| 1 | Correct README/AGENTS/STATUS; document the annexes | The safety claim matches what is enforced |
| 2 | `zero_field!` gains its `Zeroable` bound | Both call sites compile without a prose justification |
| 3 | `check_unsafe_expansion.sh` with per-crate expected counts; `ktesting` treated as a kernel crate | A new macro-injected `unsafe` in a forbid crate fails CI |
| 4 | `#[allow_internal_unsafe]` on every injecting macro; symbol-table gate for the extern family | The injector set is enumerable from the definitions |
| 5 | `init_struct_with` sealed or witness-gated; the 13 external call sites migrated | A forbid crate can no longer produce an uninitialised `T` |
| 6 | Audit and shrink the remaining safe-`# Safety` surface | Each survivor has a written reason it cannot be typed |

## Risks

- **Phase 3 is expensive per crate.** `-Zunpretty=expanded` on 20 kernel crates is
  not free. Run it in its own CI job rather than on the interactive build path,
  the way the Miri and Verus jobs already are.
- **Expansion output is not stable across toolchain bumps.** Count `unsafe`
  occurrences per crate rather than diffing text, and re-baseline in the same
  commit that bumps `rust-toolchain.toml`.
- **`internal_features` is warn-by-default and the workspace denies warnings.**
  `#[allow_internal_unsafe]` needs `#![feature(allow_internal_unsafe)]` plus a
  targeted allow in `slopos-ostd` only.
- **Phase 5 may not be fully achievable.** If `init_struct_with`'s contract cannot
  be expressed in the type system without a large refactor, say so in the plan and
  keep it as a documented, gated, enumerated exception — which is still strictly
  better than an undocumented safe fn that hands out uninitialised memory.
