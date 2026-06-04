# Terminal/Shell Split — Design Contract

Goal: replace the fused compositor-client shell with the classic UNIX pair —
a standalone **terminal emulator** app (compositor client, owns the PTY
master) and a **pure PTY-slave shell** (fd 0/1/2 on the slave, zero
compositor knowledge). Ctrl+C then works during anything because the
terminal's event loop never stops feeding the line discipline.

```
compositor ⇄ [terminal app] ⇄ PTY master
                              │ kernel ldisc (ISIG → SIGINT → fg pgrp)
                              ⇄ PTY slave ⇄ [shell] ⇄ forked jobs
```

Builds ON TOP of the uncommitted in-flight work: shell `interrupt.rs`
(SIGINT flag + handler), kernel IRQ-exit signal delivery, slibc restorer.

## Workstream 1 — `slopos-vt` crate + small kernel/userland plumbing

1. New workspace crate `vt/` (`slopos-vt`): `#![no_std]`,
   `#![forbid(unsafe_code)]`, zero dependencies. MOVE
   `drivers/src/tty/vtparser.rs` into it verbatim; bump `pub(crate)` →
   `pub` on `VtParser`, `VtAction`, `SgrAttr`, `Direction`, `EraseMode`.
   `drivers` depends on `slopos-vt`; `vconsole.rs` imports from it.
   Existing vtparser unit tests move with it (keep them running — check how
   drivers tests register and mirror that, or host-test them in the crate).
2. Userland ioctl wrappers in `userland/src/syscall/fs.rs`:
   `tiocgwinsz(fd) -> Result<UserWinsize>`, `tiocswinsz(fd, &UserWinsize)`
   (abi has `UserWinsize`, `TIOCGWINSZ`/`TIOCSWINSZ` at
   abi/src/syscall/termios.rs:360,36-37; kernel ioctl already implements
   them and raises SIGWINCH to the slave fg pgrp, drivers/src/tty/termios.rs:492-525).
3. ldisc VINTR echo (Linux ECHOCTL parity): when ISIG consumes
   VINTR/VQUIT/VSUSP and ECHO|ECHOCTL are set (and not NOFLSH-suppressed
   ordering issues — mirror Linux: echo before flush), emit `^C`-style echo
   to the output so the user sees `^C` at the keypress
   (drivers/src/tty/ldisc.rs:845-862). Add ldisc tests next to the existing
   ISIG tests (drivers/src/tty_tests/test_ldisc_signals.rs).

## Workstream 2 — terminal app (`userland/src/apps/terminal/`)

New compositor client; entry `terminal_user_main()`, bin
`userland/src/bin/terminal.rs`. Single-threaded slopfut `block_on` root
(ProtocolHandle is Rc/!Send — never touch protocol or master fd off-thread).

- **Window**: `connection::connect()`, `Surface`/`SoftSurface` (reuse the
  shell's surface.rs wholesale — move it), title "Terminal", app_id
  "org.slopos.terminal", `CURSOR_SHAPE_TEXT`, 640x480 default.
- **PTY**: `process::openpty()` + `open_tty_fd(master)`. Terminal must NOT
  acquire the master as controlling tty (it is not a fresh session leader;
  verify, and prefer adding O_NOCTTY semantics if trivial). Keep `master_fd`;
  never use index-based `tty_read`/`tty_write` (they hardcode nonblock).
- **Spawn the shell**: transient `dup2(slave_fd, 0/1/2)` in the terminal's
  own fd table around `spawn_path` of `/bin/shell` (mirror
  exec.rs:459-509's save/restore pattern), WITHOUT `TASK_FLAG_NEW_PGRP`
  (the shell's `setsid()` must succeed), then restore terminal's fds and
  close the slave fd. Shell does setsid/TIOCSCTTY itself (unchanged).
- **Event loop**: select over `poll_add(compositor_fd, POLLIN)`,
  `poll_add(master_fd, POLLIN)`, and a cursor-blink sleep arm (select3 in
  slopos-rt; nest a select2 if a 4th arm is ever needed).
  - Compositor events: `KeyPress` → encoder → `write(master_fd)`. Encoder:
    printable + control bytes (0x03 etc.) pass through; kernel-baked
    0x80-0x88 ascii codes map to CSI (`\x1b[A/B/C/D/H/F`, `\x1b[3~/5~/6~`)
    — table currently at shell exec.rs:409-419. Shift+PgUp/PgDn are
    intercepted for local scrollback (xterm convention).
  - `Configure` → surface resize → recompute cols/rows from
    cell_width/height → `tiocswinsz(master_fd)` (kernel sends SIGWINCH).
  - `CloseRequest` → close master fd (kernel hangup path SIGHUPs the
    session) → exit.
  - Pointer: selection over the rendered grid; `clipboard_copy` on release
    (4096-byte cap); paste (Ctrl+Shift+V or compositor paste event) →
    bracketed-paste bytes `\x1b[200~…\x1b[201~` written to master.
  - Master readable: read → VT interpreter → dirty-region render → present.
  - Master `Ok(0)`/POLLHUP → shell gone → exit (close window).
- **VT interpreter**: `slopos-vt` parser + a userland port of vconsole's
  pure logic: Cell/attr model, ANSI_COLORS/bright/color256 tables,
  `execute_action` handlers (print/control/erase/scroll/insert/delete/SGR/
  cursor save-restore/scroll region/alt screen), wrap+scroll, ScrollbackBuf
  ring (drivers/src/tty/vconsole.rs:41-1110 as reference; copy logic, strip
  kernel render/SpinLock/atlas-global). Must handle `\r` and `\n`
  independently (ldisc ONLCR emits `\r\n`), BS-space-BS echo, `\t`.
- **Renderer**: per-app `GlyphAtlas` via the appkit OnceLock pattern
  (appkit/src/text/mod.rs:13-26), blit cells via `atlas.get_coverage` +
  `blend_coverage_u32` into the frame `DrawBuffer`; reuse the shell
  display.rs draw_char_at/redraw_view/scroll blit structure where it fits.
- **Serial mirror**: every byte read from the master is also written via
  `crate::syscall::tty::write` (SYSCALL_WRITE → kernel console). This
  preserves `just boot-log` visibility of shell output. One call site.

## Workstream 3 — shell de-fusion (`userland/src/apps/shell/`)

Module surface (`apps::shell::{exec,buffers,env,...}`) stays stable —
io_capture_test.rs and fork_test.rs call `exec::execute_tokens` with no
compositor and must keep passing.

- **mod.rs**: delete compositor connect, surface init, openpty/dup2 block
  (mod.rs:279-303), SHELL_PTY_* statics. fd 0/1/2 are the slave, provided
  by the terminal. Keep `exec::initialize_job_control()` +
  `interrupt::install()`. Banner prints via shell_write as today.
- **Output = ANSI to fd1** (rewrite display.rs into a slim emitter, keep fn
  names): `shell_write`/`shell_write_idx` keep the OUTPUT_FD redirect
  semantics (redirected → raw bytes, color stripped). Non-redirected →
  `write(1)`; map COLOR_* indexes to truecolor SGR `\x1b[38;2;R;G;Bm` from
  the existing PALETTE RGBs + `\x1b[0m` reset. If `write(1)` fails (EBADF —
  test bins), fall back to `tty::write` exactly once per call (preserves
  test behavior). Delete the surface half and the unconditional serial
  mirror (terminal mirrors instead). `shell_echo_char` → fd1.
  scrollback/selection/page-up/cursor-blink/grid code is deleted (terminal
  owns it). CTRL_L / `clear` emit `\x1b[2J\x1b[H` only.
- **Editor = fd0 escape-sequence reader** (input.rs): raw-mode toggle stays.
  Replace protocol-event polling with `poll_add(0, POLLIN)` + `read(0)` on
  the ring. Incremental escape parser (partial ESC state across reads;
  disambiguate bare ESC with a ~30ms select2 timeout): decode exactly the
  sequences the terminal emits, producing the existing internal 0x80-0x88
  codes so the `match` dispatch body survives. Bracketed paste: emit
  `\x1b[?2004h` on editor entry / `l` on exit; `\x1b[200~…201~` → literal
  insert. Redraw via `\r` + `\x1b[K` + prompt + buffer + cursor positioning
  (`\r` + `\x1b[<n>C`); drop the blink timer (terminal blinks), drop
  mouse/selection/clipboard/pageup paths. Width from `tiocgwinsz(0)` at
  each prompt (re-query after SIGWINCH later — not required now).
- **exec.rs**: DELETE `forward_compositor_keyboard` + all 6 call sites; the
  child-wait loops become plain waitpid/poll loops. `interrupt.rs`
  `take_pending()` becomes a pure flag swap (no pump) — SIGINT now arrives
  asynchronously (terminal always pumps; ldisc fires; kernel delivers on
  syscall exit or timer-IRQ exit). All in-flight cancellation points
  (yes/seq/sleep/tee) stay as-is.
- **prompt**: write_colored_prompt emits SGR runs to fd1 (drop the
  tty::write serial half). expand_ps1 unchanged (index buffer stays).

## Workstream 4 — integration

- program_registry.rs: add `terminal` spec (gui-capable like compositor
  entries; path `/bin/terminal`). Shell stays registered (terminal spawns
  it by path; test bins also need the registry intact).
- init_process.rs: spawn `terminal` instead of `shell` (both the gated
  async path and the sync fallback).
- compositor/dock.rs pinned entry → `/bin/terminal`, title "Terminal"
  (keep ICON_SHELL for now).
- Build plumbing: Cargo.toml `[[bin]] terminal`, justfile `userland_bins`,
  scripts/build_userland.sh build+copy, ISO copy list (follow how
  `shell`/`compositor` are wired).
- Retire-later (do NOT do now): index-based SYSCALL_TTY_READ/TTY_WRITE.

## Acceptance

- `just build` green (all framekernel gates).
- `just test` green (≥ current 2517; new ldisc echo + any new tests add to
  the count; io_capture/fork tests untouched and passing).
- `just boot-log`: compositor ready, terminal window spawns shell, banner
  visible on serial via the terminal mirror.
- Manual (VIDEO=1): typing works, `yes` floods, **Ctrl+C stops it** with
  `^C` echoed; `yes | head -5` stops; `sleep 5000` Ctrl+C-able; external
  cmds keep working; resize updates winsize.
