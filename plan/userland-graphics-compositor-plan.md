# Plan

We’ll restructure boot/userland startup so the roulette runs before init, then introduce a safe graphics pipeline (compositor + windowed surfaces) so userland apps no longer draw directly to the framebuffer. The approach is incremental: keep roulette fullscreen/privileged, move shell to the new window system, then open the APIs to other apps.

## Requirements
- Roulette must still draw fullscreen and run before init/shell.
- Roulette stays userland but is scheduled as a pre-init task that always runs before init.
- Shell should become pid=1 (or be launched by init) and not draw directly to the framebuffer.
- Userland drawing must be safe: per-process buffers and a compositor.
- Integrate W/L currency events for new graphics operations.
- Preserve existing boot flow where possible; keep no_std constraints.
- Keep the compositor and related APIs in Rust; avoid new extern C boundaries unless absolutely required.
- make test must pass before finalizing.

## Scope
- In: process/boot ordering changes, compositor or window server, new syscalls for surfaces/buffers, shell updates to render via window system, roulette’s privileged path.
- Out: GPU acceleration, full multi-user, complex window manager features, persistent storage for UI state.

## Files and entry points
- userland/src/bootstrap.rs (current roulette spawn + shell hook)
- userland/src/roulette.rs, userland/src/shell.rs
- userland/src/syscall.rs (new UI syscalls)
- drivers/src/syscall_handlers.rs
- drivers/src/video_bridge.rs
- video/src/framebuffer.rs, video/src/graphics.rs, video/src/roulette_core.rs
- New compositor module location (likely video/src/compositor.rs or drivers/src/compositor.rs)
- Scheduler/process init glue in boot/ or sched/ as needed

## Data model / API changes
- New syscalls for windowing:
  - create/destroy surface (returns handle)
  - map/unmap shared buffer or provide copy-in buffer
  - present/swap for a surface
  - set z-order/focus (minimal at first)
- Capability/permission flag for fullscreen exclusive (roulette only).
- Input focus routing (keyboard to focused surface).
- Compositor runs in userland with a privileged capability to access scanout/framebuffer APIs.

## Action items
[x] Wire roulette as a userland pre-init task that always runs before init; define pid=1 behavior for init/shell.
[x] Add compositor core (kernel-owned or userland server) that composites surfaces into framebuffer.
[x] Add surface/buffer syscalls and hook them into compositor; integrate W/L awards on success/error.
[x] Update shell rendering to draw into a surface buffer and present via compositor.
[x] Gate roulette to fullscreen-exclusive mode; keep its draw path privileged via compositor capability.
[ ] Add basic input focus routing (shell gets keyboard by default).
[ ] Keep legacy direct-draw syscalls for kernel-only use; deprecate for userland except roulette.

## Testing and validation
- make build then make boot-log to confirm roulette → init → shell sequence.
- Verify test_output.log shows roulette banner and no framebuffer warnings.
- Run make test to ensure no regressions in boot/test harness.

## Risks and edge cases
- Incorrect capability checks could let userland scribble the full framebuffer.
- Compositor timing could starve if scheduled poorly; watch for flicker/tearing.
- Input routing bugs could leave shell unusable if focus isn’t set.

## Open questions
- None.
