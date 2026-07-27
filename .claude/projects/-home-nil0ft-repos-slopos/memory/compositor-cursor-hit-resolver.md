---
name: compositor-cursor-hit-resolver
description: Compositor cursor/focus/click all derive from one topmost CursorHit resolve; SetCursorShape is enter-serial gated
metadata:
  type: reference
---

The SlopOS compositor (userland/src/apps/compositor) resolves "what is the
pointer over" exactly ONCE per frame via `InputHandler::resolve_cursor_hit`
(input.rs), returning a typed `CursorHit { part: CursorPart, window_idx,
task_id }`. Cursor shape (`cursor_shape_for`), pointer focus
(`sync_pointer_focus` in mod.rs), signal-button hover (`signal_hovered_task`),
and click routing (`on_button_press`) ALL derive from that one walk. It is
top-of-z-order first and STOPS at the first window whose frame (content +
title bar) contains the pointer — a window's decorations occlude everything
beneath, so it never falls through to a lower window.

**Why** (the bug that motivated it, 2026-06-18): the cursor shape used to be
computed by a separate fall-through loop that descended past a top window's
title bar (content-only hit) to a lower window, inheriting ITS cursor — e.g.
the terminal's I-beam showed over another window's three-dot buttons. Root
cause class = several divergent hit-test passes (resize-override vs content-only
focus vs signal-group hover) that could disagree. World-class systems
(wlroots `wlr_scene_node_at`, labwc `get_cursor_context`) do a single topmost
hit; Redox Orbital iterates front-to-back and breaks. See
[[world-class-no-tinkering]].

**How to apply:** when adding a new pointer-driven behavior, add a `CursorPart`
variant and read it from the resolved hit — do NOT write a new bespoke
hit-test loop (that reintroduces the divergence bug class). The old
`compositor_cursor_override` field, `update_resize_cursor`, and
`decorations::hit_test_title_bar` were deleted; don't resurrect them.

**Enter-serial gating (Wayland model):** `Event::PointerEnter` and
`Request::SetCursorShape` both carry a `serial` (slop-protocol types.rs/codec.rs).
`Client::poll_event` snoops the last enter serial and `set_cursor_shape`
echoes it automatically. Compositor `handle_set_cursor_shape` accepts only if
`serial == surface.last_enter_serial && surface.has_pointer && serial != 0`
(serial 0 = never entered). Consequence: a client can only set the cursor in
response to a live enter — so a client (the terminal) must (re)assert its
cursor shape ON each PointerEnter, NOT once at startup (startup set is
rejected: no enter yet). See terminal/mod.rs PointerEnter arm.
