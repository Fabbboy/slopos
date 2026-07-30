#!/usr/bin/env bash
# Task-ownership gate — the acceptance criterion for the migration that
# replaces raw task pointers with `KArc<Task>`.
#
# A `*mut Task` is a task handle with no owner. It says nothing about
# whether the task is still alive, whether anyone else is mutating it, or
# who is responsible for tearing it down. `KArc<Task>` says all three.
# This gate is the load-bearing check that the raw form does not drift
# back in once the migration lands — the same belt-and-braces role
# scripts/check_unsafe_outside_ostd.sh plays for the OSTD boundary.
#
# ---------------------------------------------------------------------
# The eight checks
# ---------------------------------------------------------------------
#
#   1. No raw task pointer in binding position — `: *mut Task`,
#      `: *const Task`, `: *mut TaskInner`, `: *const TaskInner`, and the
#      `*mut *mut Task` double-indirection. Covers fn arguments, struct
#      fields, and `let` bindings. Deliberately NOT return position
#      (`-> *mut Task`): the check is named for the binding form, and the
#      sanctioned surfaces below hand pointers *out* by design.
#
#   2. No `KernelSync<*mut Task>`. A task handle laundered through a
#      blanket Send/Sync wrapper is a task handle with no owner, with the
#      compiler's objection silenced.
#
#   3. No task handle laundered through `c_void`. This check matches the
#      *cast*, never the type: `pub type TaskEntry = extern "C"
#      fn(*mut c_void)` and `TaskInner::entry_arg: *mut c_void` carry a
#      caller-opaque payload that is not a task handle, and must not trip
#      the gate. Both halves of the laundering pair are caught:
#        3a (outbound) a `*mut Task` cast to `c_void` on one line, e.g.
#           `(task as *mut Task).cast::<c_void>()`;
#        3b (inbound)  a cast back to `Task` inside a function whose own
#           signature takes a `*mut c_void` parameter, e.g. the
#           `task_arg as *mut Task` at the top of a `*mut c_void` entry
#           shim. Without 3b the migration could satisfy 3a by keeping
#           only the receiving half of the same round trip.
#
#   4. `task_borrow` and `task_borrow_mut` are gone. This is the
#      migration's terminal criterion — the accessor layer that turns a
#      raw pointer back into a reference is what the whole exercise
#      exists to delete. No exemptions.
#
#   5. No `unsafe impl Send`/`Sync` justified by a task refcount. Flags an
#      `unsafe impl ... Send/Sync` whose preceding ~8 lines mention both a
#      refcount word and a task word. A refcount buys existence, never
#      exclusivity: holding a reference proves the allocation is still
#      there, and proves nothing at all about who may write to it.
#
#   6. No `refcnt` / `task_inc_ref` / `task_dec_ref` / `inc_ref` /
#      `dec_ref` in kernel crates. Manual refcount manipulation is what
#      `KArc` exists to make unnecessary. The page-table and frame
#      refcount domains are exempt — those count *mappings*, a different
#      lifetime domain with a different owner.
#
#   7. Panic/fault paths must not take a lock or upgrade a `KArc`.
#      `boot/src/exception.rs`, `boot/src/idt.rs`, and any
#      `slopos-ostd/src/panic*` file must not mention `task_find_by_id`,
#      `task_find_by_cr3`, `task_pointer_is_valid`, `to_owned`, or
#      `task_placement_clone`.
#
#      Rationale: those lookups all take the global `TASK_MANAGER`
#      cli-spinlock, so a fault arriving while some CPU already holds it
#      would deadlock the dump — the diagnostic path would hang the
#      machine instead of describing why it died. And a `KArc` upgrade
#      whose matching drop wins the one-to-zero race would run the
#      allocator-heavy destructor from an IST stack, where there is
#      neither the stack budget for it nor any guarantee the allocator
#      locks are free.
#
#   8. No function whose return type names a lifetime that appears in no
#      argument. Such a signature fabricates its output lifetime from
#      nothing: `'a` is chosen by the caller, so it can be any lifetime at
#      all, and two calls yield two simultaneously-live references to the
#      same place. For the `&'a mut` forms that is instant aliasing UB on
#      the second call, with no unsafe block anywhere in sight at the call
#      site.
#
#      This catches the *shape*, not a list of names — which is the whole
#      point of having it alongside check 4. Check 4 greps two identifiers,
#      so another function of the same shape could be added tomorrow and
#      check 4 would say nothing about it. No exemptions.
#
#      ## What check 8 does and does not see
#
#      It is a real parser rather than a regex: it splits a signature into
#      its generic list, argument list and return type by counting angle
#      brackets and parentheses (stepping over `->` so the arrow in a
#      `Fn(A) -> B` bound does not close a generic list), then reports a
#      declared lifetime the return type names and no argument does.
#      Signatures spanning several lines are joined first, because the tree
#      contains a three-line one that a line-at-a-time scan misses.
#
#      Known limits, stated rather than implied away:
#
#        - Only lifetimes declared on the function's own generic list are
#          considered. One declared on an enclosing `impl<'a>` block and
#          used in an inherent method's return type is NOT seen.
#        - A lifetime introduced by a higher-ranked bound inside the
#          argument list (`for<'x>`) is not treated as declared, which is
#          correct, but one written in the fn's own generic list purely as
#          a bound (`<'a: 'b>`) is, so an exotic bound-only lifetime could
#          in principle be reported.
#        - `'static` is excluded, being a fixed lifetime rather than a
#          parameter the caller picks.
#        - A shared receiver minting a mutable borrow is invisible, three
#          times over. `fn slot(&self) -> &mut T` carries no generic list, so
#          the joiner never considers the line; `fn slot<T>(&self) -> &mut T`
#          declares only a type parameter, so the predicate collects no
#          lifetime and returns early; `fn slot<'a>(&'a self) -> &'a mut T`
#          does reach the predicate and is silent because `&'a self` names
#          `'a`. All three are right about the lifetime and beside the point
#          — `&self` is `Copy`, so two calls yield two live `&mut` to one
#          place. The escalation from shared to exclusive is the defect, and
#          no rule here reads mutability. Rust-for-Linux's `Opaque<T>` omits
#          `get_mut` for this reason.
#        - Substituting `'static` for a caller-chosen `'a` hides the shape
#          rather than removing it: `'static` coerces to any shorter lifetime
#          at the call site, so the caller still ends up with a borrow of its
#          own choosing, and a second call still hands out another. It is
#          honest only when the region truly is never freed and the call truly
#          happens once, neither of which is in the signature. In-tree,
#          `ptr_buf::install_buf_mut` (one-shot install) and `dev/mod.rs`'s
#          `borrow_dyn` (published-once device handle) are both honest, and
#          nothing here could tell them from a region that is later freed.
#        - The argument scan tests whether a lifetime is *named* in the
#          argument list, not whether an argument *supplies* it. A mention
#          that supplies nothing — a closure bound written inline
#          (`g: impl FnOnce(&'a T)`), a bare `fn(&'a ())`, a
#          `PhantomData<&'a ()>` — therefore counts as constraining, while
#          the callee still mints the reference from a raw pointer under a
#          lifetime the caller picked. This is a bypass rather than a blind
#          spot: the shape this check is named for passes once any such
#          parameter is added. The sound spelling is the higher-ranked one,
#          which the `Fn(&T)` sugar gives for free and an explicit `for<'a>`
#          states — a lifetime the caller cannot name is one it cannot
#          choose.
#          Only an occurrence inside an argument's own type does this. The
#          bound spellings `<'a, F: FnOnce(&'a T)>` and
#          `where F: FnOnce(&'a T)` are both still reported, because neither
#          the generic list nor the `where` clause is part of the argument
#          list. `ret` is truncated at `where` deliberately, as a
#          false-positive guard: without it,
#          `fn f<'a, F>(x: u32, f: F) -> u32 where F: Fn(&'a u8)` would be
#          reported for a function that returns no reference at all.
#        - Elided lifetimes are not analysed at all. `fn f(x: &Task) -> &Foo`
#          is tied to its argument by the elision rules and is silent here,
#          which is the correct answer, but it is silent by not looking
#          rather than by checking.
#        - A signature longer than 20 lines is truncated at 20 and evaluated
#          as-is rather than skipped. A truncation that loses the `->` reads
#          as no return type and reports nothing.
#        - The shape is sound when every input the caller could present
#          again is consumed by value, because then the function cannot be
#          called twice on the same referent. std's `Box::leak<'a>(b: Self)
#          -> &'a mut T` is correct for exactly that reason: the box is moved
#          in, the allocation is deliberately never freed, and no second call
#          against it is possible. Check 8 would report it anyway —
#          distinguishing "consumed owning handle" from "re-presentable raw
#          pointer or address" needs type resolution, not a signature parse.
#          The in-tree `KBox::leak` declares no generic list of its own, so
#          the joiner skips it before any of this applies, and it says
#          `&'static mut T` where std says `&'a mut T` — the substitution the
#          bullet above calls a hiding move, honest here for the same reason
#          the shape is sound: the box is gone.
#
# ---------------------------------------------------------------------
# Sanctioned surfaces (exempt from checks 1 and 3)
# ---------------------------------------------------------------------
#
#   slopos-ostd/src/task/placement.rs
#       The placement state machine. The sole sanctioned way to move a
#       strong reference into and out of a container — every other
#       transfer of task ownership is expressed in terms of it. It has to
#       name the raw form because it is the thing that converts between
#       the raw form and the owned one.
#
#   slopos-ostd/src/task/link_roles.rs
#       Intrusive-link roles. A Treiber successor *is* a raw pointer; its
#       lifetime is governed by the parked reference the link represents,
#       not by a Rust borrow. Reference in, pointer out.
#
#   slopos-ostd/src/cpu/x86_64/pcr.rs
#       The PCR `current_task` slot: offset 40, ABI-frozen (assembly and
#       the switch path both hard-code it), type-erased to `*mut ()`.
#
#   sched/src/safestack_rt.rs
#       The pre-heap `.bss` bootstrap stubs the SafeStack runtime seeds.
#       These exist before the allocator does, so they cannot be `KArc`.
#
# Refcount-word exemptions for check 6 (a different lifetime domain —
# these count page-table mappings and frame references, not tasks):
#   mm/src/paging/
#   slopos-ostd/src/mm/
#
# Scope: kernel crates only. Userland-side crates (userland, slibc,
# slop-protocol, ktesting, appkit, slopos-rt, image, terminal-core,
# keymap-core) are out of scope, same as check_unsafe_outside_ostd.sh, as
# are plans/, verification/, vendor/, and every non-`.rs` file. The scope
# filter is a deny-list rather than an allow-list on purpose: a new kernel
# crate is scanned the day it is added, with nothing to keep in sync.
#
# Comment-line and `#[cfg(...)]`-gated occurrences are skipped using the
# same lookback pattern as scripts/check_alloc_dep.sh.
#
# ---------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------
#
# `--self-test` runs the regexes against built-in positive and negative
# fixtures and asserts each check fires on the positives and stays silent
# on the negatives. It proves the scan is not silently matching nothing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SELF_TEST=0
if [ "${1:-}" = "--self-test" ]; then
    SELF_TEST=1
fi

# Userland-side crates, planning docs, proofs, and vendored code.
OUT_OF_SCOPE_RE='^(userland|slibc|slop-protocol|ktesting|appkit|slopos-rt|image|terminal-core|keymap-core|plans|verification|vendor|third_party|builddir|target)/'

# Exempt from checks 1 and 3 — see the header for each one's reason.
SANCTIONED_SURFACES=(
    "slopos-ostd/src/task/placement.rs"
    "slopos-ostd/src/task/link_roles.rs"
    "slopos-ostd/src/cpu/x86_64/pcr.rs"
    "sched/src/safestack_rt.rs"
)

# Exempt from check 6 — the page-table / frame refcount domain.
REFCOUNT_DOMAIN_RE='^(mm/src/paging/|slopos-ostd/src/mm/)'

# Check 7's fault-path files. `slopos-ostd/src/panic*` is expanded at scan
# time so a newly added panic module is covered without editing this list.
FAULT_PATH_FILES=(
    "boot/src/exception.rs"
    "boot/src/idt.rs"
)
FAULT_PATH_FORBIDDEN='task_find_by_id|task_find_by_cr3|task_pointer_is_valid|to_owned|task_placement_clone'

# ---------------------------------------------------------------------
# File collection
# ---------------------------------------------------------------------

collect_files() {
    local root="$1"
    cd "$root"
    {
        if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
            git ls-files '*.rs'
            git ls-files --others --exclude-standard '*.rs'
        else
            find . -type f -name '*.rs' \
                -not -path './builddir/*' \
                -not -path './third_party/*' \
                -not -path './target/*'
        fi
    } | sed 's|^\./||' | LC_ALL=C sort -u | grep -Ev "$OUT_OF_SCOPE_RE" || true
}

is_sanctioned() {
    local path="$1" exempt
    for exempt in "${SANCTIONED_SURFACES[@]}"; do
        [ "$path" = "$exempt" ] && return 0
    done
    return 1
}

# ---------------------------------------------------------------------
# The scan
#
# One awk pass per file emits `<check-tag>\t<file>:<line>: <text>` for
# every finding, so a single traversal feeds all seven source-level checks.
# ---------------------------------------------------------------------

scan_sources() {
    local root="$1"
    shift
    local file e13 e6
    cd "$root"
    for file in "$@"; do
        [ -z "$file" ] && continue
        [ -f "$file" ] || continue
        e13=0
        is_sanctioned "$file" && e13=1
        e6=0
        [[ "$file" =~ $REFCOUNT_DOMAIN_RE ]] && e6=1
        awk -v fname="$file" -v exempt13="$e13" -v exempt6="$e6" '
            # A `#[cfg(...)]` on the previous line gates this line; a
            # `#[cfg(...)]` two lines back gates it via an enclosing
            # `mod ... {`. Same lookback as check_alloc_dep.sh.
            function cfg_gated(n) {
                if (n - 1 >= 1 && lines[n - 1] ~ /^[[:space:]]*#\[cfg\(/) {
                    return 1
                }
                if (n - 2 >= 1 \
                    && lines[n - 2] ~ /^[[:space:]]*#\[cfg\(/ \
                    && lines[n - 1] ~ /mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{/) {
                    return 1
                }
                return 0
            }
            function emit(tag, n) {
                printf "%s\t%s:%d: %s\n", tag, fname, n, lines[n]
            }

            # ---- check 8 helpers: a small Rust signature parser --------
            # Bracket matchers that step over `->` so the `>` of an arrow
            # inside a bound (`F: Fn(u32) -> u32`) does not close a generic
            # list.
            function match_angle(s, p,   d, i, c) {
                d = 0
                i = p
                while (i <= length(s)) {
                    c = substr(s, i, 1)
                    if (c == "-" && substr(s, i + 1, 1) == ">") { i += 2; continue }
                    if (c == "<") { d++ }
                    else if (c == ">") { d--; if (d == 0) { return i } }
                    i++
                }
                return 0
            }
            function match_paren(s, p,   d, i, c) {
                d = 0
                i = p
                while (i <= length(s)) {
                    c = substr(s, i, 1)
                    if (c == "(") { d++ }
                    else if (c == ")") { d--; if (d == 0) { return i } }
                    i++
                }
                return 0
            }
            # Whole-token test, so `<Q>a` does not match inside `<Q>ab`.
            function names_lifetime(s, lt) {
                return (s ~ (lt "([^A-Za-z0-9_]|$)"))
            }
            # The check-8 predicate. Splits a joined signature into its
            # generic list, argument list and return type, then reports the
            # first declared lifetime that the return type names and no
            # argument does. Returns the lifetime, or "" for a clean
            # signature.
            function unconstrained_lifetime(sig,   i, gend, aend, generics, args,
                                            ret, rest, tmp, lt, k, nlt, lts) {
                if (match(sig, /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z0-9_]+/) == 0) {
                    return ""
                }
                i = RSTART + RLENGTH
                while (substr(sig, i, 1) == " " || substr(sig, i, 1) == "\t") { i++ }
                # No generic list means no declared lifetime to fabricate.
                if (substr(sig, i, 1) != "<") { return "" }
                gend = match_angle(sig, i)
                if (gend == 0) { return "" }
                generics = substr(sig, i, gend - i + 1)
                i = gend + 1
                while (substr(sig, i, 1) == " " || substr(sig, i, 1) == "\t") { i++ }
                if (substr(sig, i, 1) != "(") { return "" }
                aend = match_paren(sig, i)
                if (aend == 0) { return "" }
                args = substr(sig, i, aend - i + 1)
                rest = substr(sig, aend + 1)
                if (index(rest, "->") == 0) { return "" }
                ret = substr(rest, index(rest, "->") + 2)
                # A `where` clause constrains but does not tie a lifetime to
                # an argument, so it is not part of the return type.
                if (match(ret, /(^|[^A-Za-z0-9_])where([^A-Za-z0-9_]|$)/)) {
                    ret = substr(ret, 1, RSTART - 1)
                }
                if (index(ret, "{")) { ret = substr(ret, 1, index(ret, "{") - 1) }

                nlt = 0
                tmp = generics
                while (match(tmp, (Q "[A-Za-z_][A-Za-z0-9_]*"))) {
                    lt = substr(tmp, RSTART, RLENGTH)
                    tmp = substr(tmp, RSTART + RLENGTH)
                    if (lt == (Q "static")) { continue }
                    lts[++nlt] = lt
                }
                for (k = 1; k <= nlt; k++) {
                    if (names_lifetime(ret, lts[k]) && !names_lifetime(args, lts[k])) {
                        return lts[k]
                    }
                }
                return ""
            }

            BEGIN { Q = sprintf("%c", 39) }

            # Store first so every later rule can look back.
            { lines[NR] = $0; last = NR }

            {
                # Skip pure comment lines. `^\*` with no following space
                # is Rust dereference syntax, not a block-comment
                # continuation, so only `* ` / `*/` are treated as prose.
                if ($0 ~ /^[[:space:]]*(\/\/|\/\*|\*\/|\*[[:space:]])/) next
                stripped = $0
                sub(/\/\/.*/, "", stripped)
            }

            # ---- check 1: raw task pointer in binding position -------
            # `Task` and `TaskInner` are spelled as separate alternatives
            # with an explicit trailing non-word class so `TaskEntry`,
            # `TaskRef`, `TaskAddr`, `TaskAbi`, and `TaskStack` cannot
            # match under any regex engine alternation semantics.
            {
                if (!exempt13 \
                    && (stripped ~ /:[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+Task([^A-Za-z0-9_]|$)/ \
                        || stripped ~ /:[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+TaskInner([^A-Za-z0-9_]|$)/ \
                        || stripped ~ /\*[[:space:]]*(mut|const)[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+Task([^A-Za-z0-9_]|$)/ \
                        || stripped ~ /\*[[:space:]]*(mut|const)[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+TaskInner([^A-Za-z0-9_]|$)/) \
                    && !cfg_gated(NR)) {
                    emit("1", NR)
                }
            }

            # ---- check 2: KernelSync<*mut Task> ----------------------
            {
                if ((stripped ~ /KernelSync<[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+Task([^A-Za-z0-9_]|$)/ \
                     || stripped ~ /KernelSync<[[:space:]]*\*[[:space:]]*(mut|const)[[:space:]]+TaskInner([^A-Za-z0-9_]|$)/) \
                    && !cfg_gated(NR)) {
                    emit("2", NR)
                }
            }

            # ---- check 3a: outbound launder, *mut Task -> c_void -----
            # Requires a task raw pointer AND a cast-to-c_void on the same
            # line. `fn(*mut c_void)` and `entry_arg: *mut c_void` carry no
            # task pointer, so neither can reach this rule.
            {
                if (!exempt13 \
                    && stripped ~ /\*[[:space:]]*(mut|const)[[:space:]]+Task([^A-Za-z0-9_]|$)/ \
                    && (stripped ~ /cast::<[^>]*c_void[^>]*>/ \
                        || stripped ~ /as[[:space:]]+\*[[:space:]]*(mut|const)[[:space:]]+([A-Za-z0-9_]+::)*c_void([^A-Za-z0-9_]|$)/) \
                    && !cfg_gated(NR)) {
                    emit("3a", NR)
                }
            }

            # ---- check 3b: inbound launder, c_void -> *mut Task ------
            # A cast back to `Task` is only a launder if the enclosing
            # function received the value as `*mut c_void`. Walk back up
            # to 30 lines for the nearest `fn` signature and require the
            # c_void parameter on it.
            {
                if (!exempt13 \
                    && (stripped ~ /as[[:space:]]+\*[[:space:]]*(mut|const)[[:space:]]+Task([^A-Za-z0-9_]|$)/ \
                        || stripped ~ /cast::<[[:space:]]*\*?[[:space:]]*(mut|const)?[[:space:]]*Task[[:space:]]*>/) \
                    && !cfg_gated(NR)) {
                    for (i = NR; i >= 1 && i > NR - 30; i--) {
                        if (lines[i] ~ /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z0-9_]+/) {
                            if (lines[i] ~ /\*[[:space:]]*(mut|const)[[:space:]]+([A-Za-z0-9_]+::)*c_void/) {
                                emit("3b", NR)
                            }
                            break
                        }
                    }
                }
            }

            # ---- check 4: the accessor layer is gone -----------------
            {
                if (stripped ~ /(^|[^A-Za-z0-9_])task_borrow(_mut)?([^A-Za-z0-9_]|$)/ \
                    && !cfg_gated(NR)) {
                    emit("4", NR)
                }
            }

            # ---- check 5: Send/Sync justified by a task refcount -----
            # A refcount word AND a task word in the safety argument
            # attached to the impl. The lookback is capped at 8 lines and
            # stops at the first line that is neither comment, attribute,
            # nor blank — an unrelated `unsafe impl Sync` that merely
            # happens to sit below code mentioning a task refcount is not
            # justified by one, and the self-test fixture pins that
            # distinction.
            {
                if (stripped ~ /unsafe[[:space:]]+impl.*(Send|Sync).*[[:space:]]for([[:space:]]|$)/) {
                    ctx = tolower($0)
                    for (i = NR - 1; i >= 1 && i > NR - 9; i--) {
                        if (lines[i] !~ /^[[:space:]]*(\/\/|\/\*|\*|#\[|$)/) {
                            break
                        }
                        ctx = ctx " " tolower(lines[i])
                    }
                    if (ctx ~ /refcount|refcnt|reference count|ref count|inc_ref|dec_ref|refcounted|ref-count/ \
                        && ctx ~ /task/) {
                        emit("5", NR)
                    }
                }
            }

            # ---- check 6: manual task refcount manipulation ----------
            {
                if (!exempt6 \
                    && stripped ~ /(^|[^A-Za-z0-9_])(refcnt|task_inc_ref|task_dec_ref|inc_ref|dec_ref)([^A-Za-z0-9_]|$)/ \
                    && !cfg_gated(NR)) {
                    emit("6", NR)
                }
            }

            # ---- check 8: output lifetime tied to no argument ---------
            # Runs in END rather than per-line because a signature can span
            # several lines and the whole of it has to be seen at once. A
            # three-line signature is what a line-at-a-time scan misses
            # entirely; the spans_lines fixture in lifetimes_bad.rs pins
            # that case. No apostrophe may appear anywhere in this awk
            # program: it is single-quoted, and one would end it.
            #
            # Joining rule: start at a line carrying `fn <name><`, append
            # following lines until the accumulated text reaches the `{` or
            # `;` that ends the signature, stripping trailing `//` comments
            # as it goes. Capped so an unterminated signature cannot run
            # away over the rest of the file.
            END {
                for (n = 1; n <= last; n++) {
                    line = lines[n]
                    if (line ~ /^[[:space:]]*(\/\/|\/\*|\*\/|\*[[:space:]])/) { continue }
                    if (line !~ /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z0-9_]+[[:space:]]*</) { continue }
                    if (cfg_gated(n)) { continue }

                    sig = ""
                    for (j = n; j <= last && j < n + 20; j++) {
                        piece = lines[j]
                        sub(/\/\/.*/, "", piece)
                        sig = sig " " piece
                        if (piece ~ /[{;]/) { break }
                    }
                    lt = unconstrained_lifetime(sig)
                    if (lt != "") {
                        emit("8", n)
                    }
                }
            }
        ' "$file" || true
    done
}

scan_fault_paths() {
    local root="$1"
    local file
    cd "$root"
    {
        printf '%s\n' "${FAULT_PATH_FILES[@]}"
        find slopos-ostd/src -name 'panic*.rs' 2>/dev/null || true
    } | LC_ALL=C sort -u | while IFS= read -r file; do
        [ -z "$file" ] && continue
        [ -f "$file" ] || continue
        awk -v fname="$file" -v pat="$FAULT_PATH_FORBIDDEN" '
            /^[[:space:]]*(\/\/|\/\*|\*\/|\*[[:space:]])/ { next }
            {
                stripped = $0
                sub(/\/\/.*/, "", stripped)
                if (stripped ~ pat) {
                    printf "7\t%s:%d: %s\n", fname, NR, $0
                }
            }
        ' "$file" || true
    done
}

run_scan() {
    local root="$1"
    shift
    scan_sources "$root" "$@"
    scan_fault_paths "$root"
}

# ---------------------------------------------------------------------
# Self-test — prove every regex fires
# ---------------------------------------------------------------------

if [ "$SELF_TEST" -eq 1 ]; then
    fixture_root="$(mktemp -d)"
    trap 'rm -rf "$fixture_root"' EXIT

    mkdir -p "$fixture_root/sched/src/task" \
             "$fixture_root/slopos-ostd/src/task" \
             "$fixture_root/mm/src/paging" \
             "$fixture_root/boot/src"

    # Positive fixtures — every check must fire exactly once here.
    cat > "$fixture_root/sched/src/task/positives.rs" <<'FIXTURE'
fn takes_raw(task: *mut Task) {}
fn takes_inner(inner: *const TaskInner) {}
fn double(slot: *mut *mut Task) {}
struct Holder { slot: KernelSync<*mut Task> }
fn outbound(task: &mut Task) -> u64 {
    (task as *mut Task).cast::<c_void>() as u64
}
extern "C" fn shim(task_arg: *mut c_void) {
    let task_ptr = task_arg as *mut Task;
}
fn borrows() {
    task_borrow(ptr);
    task_borrow_mut(ptr);
}
/// Safe because the task refcount keeps the allocation alive for as
/// long as this handle exists.
unsafe impl Send for Holder {}
fn counts() {
    task_inc_ref(ptr);
    task_dec_ref(ptr);
    let n = slot.refcnt;
}
FIXTURE

    # Check 8 gets its own fixture pair: five positive signatures — four
    # dangerous shapes plus the multi-line join — and
    # the lookalikes that must stay silent. The negatives carry the weight
    # here — this is the check most able to produce false positives.
    # `Payload` rather than `Task` throughout, so these files exercise
    # check 8 and nothing else: a fixture that also tripped check 1 would
    # make both checks' counts uninterpretable. The predicate reads only
    # the lifetime tokens in the generic list, arguments and return type,
    # so the referent type is immaterial to what is under test.
    cat > "$fixture_root/sched/src/task/lifetimes_bad.rs" <<'FIXTURE'
pub fn borrow_mut<'a, K, U>(p: *mut Payload<K, U>) -> Option<&'a mut Payload<K, U>> {
    None
}
pub fn name_bytes<'a, K, U>(p: *const Payload<K, U>) -> Option<&'a [u8]> {
    None
}
pub fn fpu_mut<'a, K, U>(p: *mut Payload<K, U>) -> &'a mut FpuState {
    todo!()
}
pub fn plain_ref<'a>(p: *const Payload) -> &'a Payload {
    todo!()
}
pub fn spans_lines<'a, K, U>(
    p: *const Payload<K, U>,
) -> Option<&'a crate::sync::AtomicCell<ExitInfo>> {
    None
}
FIXTURE

    cat > "$fixture_root/sched/src/task/lifetimes_ok.rs" <<'FIXTURE'
// The lifetime IS tied to an argument — the whole point of the check.
pub fn tied<'a>(p: &'a Payload) -> &'a Foo {
    &p.foo
}
pub fn tied_mut<'a>(p: &'a mut Payload) -> Option<&'a mut Foo> {
    None
}
// Elided: bound to the argument by the elision rules.
pub fn elided(p: &Payload) -> &Foo {
    &p.foo
}
// A lifetime declared and used only in an argument.
pub fn consumes<'a>(p: &'a Payload) -> u32 {
    0
}
// 'static is a fixed lifetime, not one the caller picks.
pub fn statics<K, U>(p: *const Payload<K, U>) -> Option<&'static [u8]> {
    None
}
// No generic list at all.
pub fn no_generics(p: *mut Payload) -> u32 {
    0
}
// An arrow inside a bound must not close the generic list, and `F` is not
// a lifetime.
pub fn with_bound<'a, F: Fn(u32) -> u32>(p: &'a Payload, f: F) -> &'a Foo {
    &p.foo
}
// Higher-ranked bound in argument position: 'x is not declared on the fn.
pub fn hrtb<'a>(p: &'a Payload, f: impl for<'x> Fn(&'x u32)) -> &'a Foo {
    &p.foo
}
// Multi-line, but argument-tied.
pub fn tied_multiline<'a, K, U>(
    p: &'a Payload<K, U>,
) -> Option<&'a crate::sync::AtomicCell<ExitInfo>> {
    None
}
FIXTURE

    # Negative fixtures — nothing here may fire. The c_void payload types
    # are the false positive check 3 is written to avoid.
    cat > "$fixture_root/sched/src/task/negatives.rs" <<'FIXTURE'
pub type TaskEntry = extern "C" fn(*mut c_void);
pub struct TaskInnerFields { pub entry_arg: *mut c_void }
fn refs(task: &KArc<Task>, r: TaskRef, a: TaskAddr, s: TaskStack, b: TaskAbi) {}
fn entry_types(e: TaskEntry, r: *mut TaskRef, a: *const TaskAddr) {}
// A comment mentioning task_borrow and refcnt and *mut Task must not fire.
/* Block comment naming task_borrow_mut and *const TaskInner. */
fn spelled_out(handle: TaskHandle) -> TaskEntry { todo!() }
#[cfg(test)]
fn cfg_gated_raw(task: *mut Task) {}
unsafe impl Sync for Unrelated {}
fn opaque_payload(arg: *mut c_void) -> *mut c_void { arg }
FIXTURE

    # Sanctioned surface — the same violations as positives.rs, but this
    # path is exempt from checks 1 and 3, so only check 4 may fire.
    cat > "$fixture_root/slopos-ostd/src/task/placement.rs" <<'FIXTURE'
fn takes_raw(task: *mut Task) {}
fn outbound(task: &mut Task) -> u64 {
    (task as *mut Task).cast::<c_void>() as u64
}
fn still_flagged() { task_borrow(ptr); }
FIXTURE

    # Refcount domain — exempt from check 6 only.
    cat > "$fixture_root/mm/src/paging/frames.rs" <<'FIXTURE'
fn map() { entry.inc_ref(); entry.dec_ref(); let n = entry.refcnt; }
FIXTURE

    # Fault path — check 7 must fire on each forbidden symbol.
    cat > "$fixture_root/boot/src/exception.rs" <<'FIXTURE'
fn dump(id: u32) {
    let t = task_find_by_id(id);
    let c = task_find_by_cr3(cr3);
    if task_pointer_is_valid(p) {}
    let owned = handle.to_owned();
    let cloned = task_placement_clone(slot);
}
FIXTURE

    fixture_files=(
        "sched/src/task/positives.rs"
        "sched/src/task/negatives.rs"
        "sched/src/task/lifetimes_bad.rs"
        "sched/src/task/lifetimes_ok.rs"
        "slopos-ostd/src/task/placement.rs"
        "mm/src/paging/frames.rs"
    )
    findings="$(run_scan "$fixture_root" "${fixture_files[@]}")"

    self_test_fail=0
    expect() {
        local tag="$1" want="$2" got
        got="$(printf '%s\n' "$findings" | grep -c "^$tag"$'\t' || true)"
        if [ "$got" -ne "$want" ]; then
            echo "check_task_ownership --self-test: check $tag expected $want hit(s), got $got" >&2
            printf '%s\n' "$findings" | grep "^$tag"$'\t' | sed 's/^/      /' >&2
            self_test_fail=1
        else
            echo "  check $tag: fires $got/$want as expected"
        fi
    }

    echo "check_task_ownership: self-test against built-in fixtures"
    expect 1  3   # raw arg, raw inner arg, double-indirection
    expect 2  1   # KernelSync<*mut Task>
    expect 3a 1   # outbound cast to c_void
    expect 3b 1   # inbound cast back from a c_void parameter
    expect 4  3   # two in positives.rs, one in the sanctioned surface
    expect 5  1   # Send impl justified by a task refcount
    expect 6  3   # task_inc_ref, task_dec_ref, refcnt
    expect 7  5   # five forbidden symbols on the fault path
    expect 8  5   # Option<&'a mut T>, Option<&'a [u8]>, &'a mut T, &'a T, multi-line

    # Nothing in either negative fixture may fire, on any check. The
    # lifetime negatives are the load-bearing ones: check 8 is the check
    # most able to cry wolf, and a gate that flags correct code gets
    # switched off.
    for neg in "negatives\.rs" "lifetimes_ok\.rs"; do
        hits="$(printf '%s\n' "$findings" | grep "$neg" || true)"
        if [ -n "$hits" ]; then
            echo "check_task_ownership --self-test: false positive in ${neg//\\/}:" >&2
            printf '%s\n' "$hits" | sed 's/^/      /' >&2
            self_test_fail=1
        fi
    done
    if [ "$self_test_fail" -eq 0 ]; then
        echo "  negatives: no false positives (TaskEntry / entry_arg / TaskRef / TaskAddr clean)"
        echo "  lifetime negatives: no false positives (argument-tied, elided, 'static,"
        echo "    Fn(..) -> .. bound, and for<'x> HRTB all stay silent)"
    fi

    if [ "$self_test_fail" -ne 0 ]; then
        echo "check_task_ownership: SELF-TEST FAILED — the gate's regexes are wrong" >&2
        exit 1
    fi
    echo "check_task_ownership: self-test OK"
    exit 0
fi

# ---------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------

# Drift guard: a sanctioned surface that no longer exists is a stale
# exemption, and a stale exemption is a hole nobody is watching.
for exempt in "${SANCTIONED_SURFACES[@]}"; do
    if [ ! -f "$REPO_ROOT/$exempt" ]; then
        echo "check_task_ownership: WARNING — sanctioned surface '$exempt' no longer exists;" >&2
        echo "  remove it from SANCTIONED_SURFACES in this script or fix the path." >&2
    fi
done

mapfile -t scan_files < <(collect_files "$REPO_ROOT")
if [ "${#scan_files[@]}" -eq 0 ]; then
    echo "check_task_ownership: no Rust sources in scope — the scan would be a no-op" >&2
    exit 2
fi

findings="$(run_scan "$REPO_ROOT" "${scan_files[@]}")"

CHECK_TAGS=(1 2 3a 3b 4 5 6 7 8)
declare -A CHECK_DESC=(
    [1]="raw task pointer in binding position (: *mut/*const Task/TaskInner, *mut *mut Task)"
    [2]="KernelSync<*mut Task> — an owner-less handle with Send/Sync silenced"
    [3a]="task handle cast out to c_void"
    [3b]="task handle cast back in from a c_void parameter"
    [4]="task_borrow / task_borrow_mut — the accessor layer must be gone"
    [5]="unsafe impl Send/Sync justified by a task refcount"
    [6]="manual task refcount manipulation (refcnt / inc_ref / dec_ref)"
    [7]="panic/fault path takes a lock or upgrades a KArc"
    [8]="return type names a lifetime no argument constrains — the caller picks it"
)

total=0
declare -A CHECK_HITS=()
for tag in "${CHECK_TAGS[@]}"; do
    hits="$(printf '%s\n' "$findings" | grep "^$tag"$'\t' || true)"
    if [ -z "$hits" ]; then
        CHECK_HITS[$tag]=0
    else
        CHECK_HITS[$tag]="$(printf '%s\n' "$hits" | wc -l | tr -d ' ')"
    fi
    total=$((total + CHECK_HITS[$tag]))
done

if [ "$total" -eq 0 ]; then
    echo "check_task_ownership: OK — no declared output lifetime that no argument names, no borrow accessors, and no fault-path lookups; no raw task pointers or c_void launders outside the sanctioned surfaces"
    exit 0
fi

out=2
label="FAIL"
per_check_cap=0

{
    echo "check_task_ownership: $label — $total task-ownership finding(s):"
    for tag in "${CHECK_TAGS[@]}"; do
        [ "${CHECK_HITS[$tag]}" -eq 0 ] && continue
        echo "  check $tag (${CHECK_HITS[$tag]} hit(s)): ${CHECK_DESC[$tag]}"
        if [ "$per_check_cap" -gt 0 ] && [ "${CHECK_HITS[$tag]}" -gt "$per_check_cap" ]; then
            # `awk NR<=cap` rather than `head`: under `set -o pipefail`,
            # head closing the pipe early SIGPIPEs the upstream grep and
            # aborts the whole report.
            printf '%s\n' "$findings" | grep "^$tag"$'\t' | cut -f2- \
                | awk -v cap="$per_check_cap" 'NR <= cap' | sed 's/^/      /'
            echo "      … and $((CHECK_HITS[$tag] - per_check_cap)) more (run scripts/check_task_ownership.sh for the full list)"
        else
            printf '%s\n' "$findings" | grep "^$tag"$'\t' | cut -f2- | sed 's/^/      /'
        fi
    done
    echo "  A *mut Task is a task handle with no owner. Move the handle into a KArc<Task>,"
    echo "  or route the transfer through slopos-ostd/src/task/placement.rs."
    echo "  Sanctioned surfaces (exempt from checks 1 and 3) are listed in this script's header."
} >&"$out"

exit 1
