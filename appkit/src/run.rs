use slopos_abi::Canvas;
use slopos_abi::syscall::posix::POLLIN;
use slopos_gfx::RenderSurface;
use slopos_protocol::types::Event as ProtocolEvent;
use slopos_rt::Ring;
use slopos_rt::slopfut;

use slopos_windowing::Event;
use slopos_windowing::connection;
use slopos_windowing::{EVENT_BUF_LEN, Window};

use super::event::{self, HitTestResult, MessageSink, WidgetEvent};
use super::focus::FocusManager;
use super::input::{Keymap, translate_event};
use super::node::{Action, App};
use super::overlay::OverlayManager;
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{FocusPolicy, Widget};
use super::tree;

/// Run a widget-framework-driven application.
///
/// Sets up one ring and drives the event loop as an async root via
/// [`slopfut::block_on`]; the wait/wake step races the compositor socket,
/// the cross-thread wakeup pipe, and (when bounded) a timer.
pub fn run_app<A: App>(app: A, width: u32, height: u32) -> ! {
    let ring = Ring::setup(16).expect("appkit: ring setup failed");
    slopfut::block_on(ring, run_app_async(app, width, height))
}

async fn run_app_async<A: App>(mut app: A, width: u32, height: u32) -> ! {
    let handle = connection::connect().expect("compositor not running");
    let mut win = Window::new(handle.clone(), width, height).expect("failed to create window");
    win.set_title(app.title());
    let id = app.app_id();
    if !id.is_empty() {
        win.set_app_id(id);
    }
    let style = StyleSheet::dark();
    let keymap = Keymap::us_qwerty();
    let mut focus = FocusManager::new();
    let mut overlays = OverlayManager::new();
    let mut modifiers = super::event::Modifiers::default();

    // Build initial widget tree.
    let node = app.view();
    let mut root = tree::build_widget_tree(&node);

    // Initial measure + layout.
    let mut window_size = super::constraints::Size::new(width as i32, height as i32);
    tree::layout_tree(root.as_mut(), window_size, &style);
    focus.rebuild_tab_chain(root.as_ref());

    let mut needs_rebuild = false;
    let mut needs_repaint = true;
    let mut proto_events: [ProtocolEvent; EVENT_BUF_LEN] =
        core::array::from_fn(|_| ProtocolEvent::FrameDone {
            surface: slopos_protocol::types::SurfaceId::NONE,
            timestamp_ms: 0,
        });
    let mut last_tick_ms: u64 = slopos_windowing::get_time_ms();

    loop {
        // Flush any deferred Surface::drop destroy requests and execute
        // any closures posted by background threads via UiSender.
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        // --- Poll input events ---
        let count = win.poll_protocol_events(&mut proto_events);
        let mut unhandled_key: Option<(super::event::Key, super::event::Modifiers)> = None;
        let mut sink = MessageSink::new();

        for i in 0..count {
            let ev = match Event::from_protocol(&proto_events[i]) {
                Some(e) => e,
                None => continue,
            };
            win.track_pointer(&ev);
            update_modifiers(&ev, &mut modifiers);

            match &ev {
                Event::CloseRequest => std::process::exit(0),
                Event::Configure {
                    width: w,
                    height: h,
                } => {
                    let _ = win.resize(*w, *h);
                    window_size = super::constraints::Size::new(*w as i32, *h as i32);
                    needs_rebuild = true;
                    continue;
                }
                _ => {}
            }

            let widget_event = match translate_event(&ev, &keymap, &modifiers) {
                Some(e) => e,
                None => continue,
            };

            let (px, py) = win.pointer();
            let widget_event = fill_pointer_pos(widget_event, px, py);

            // Track focus modality.
            match &widget_event {
                WidgetEvent::PointerDown { .. } => focus.note_pointer_input(),
                WidgetEvent::KeyDown { .. } | WidgetEvent::TextInput { .. } => {
                    focus.note_keyboard_input()
                }
                _ => {}
            }

            // Tab/Shift+Tab focus navigation.
            if let WidgetEvent::KeyDown {
                key: super::event::Key::Named(super::event::NamedKey::Tab),
                modifiers: mods,
                ..
            } = &widget_event
            {
                if mods.shift {
                    focus.move_focus_prev();
                } else {
                    focus.move_focus_next();
                }
                needs_repaint = true;
                continue;
            }

            // Hit test and dispatch.
            let (px, py) = win.pointer();
            let resp = if let Some(hit) = event::hit_test(root.as_ref(), px, py) {
                if matches!(widget_event, WidgetEvent::PointerDown { .. }) {
                    let target_policy = find_focus_policy(root.as_ref(), hit.target);
                    if target_policy.is_focusable() {
                        focus.set_focused(Some(hit.target));
                    }
                    if overlays.hit_test(px, py).is_none() && !overlays.is_empty() {
                        overlays.dismiss_light(&mut focus);
                    }
                }
                event::dispatch_event(root.as_mut(), &hit, &widget_event, &mut sink)
            } else {
                let dummy_hit = HitTestResult {
                    target: focus.focused().unwrap_or(super::traits::WidgetId::NONE),
                    chain: Vec::new(),
                };
                event::dispatch_event(root.as_mut(), &dummy_hit, &widget_event, &mut sink)
            };

            if resp.is_consumed() {
                needs_repaint = true;
            } else {
                // Key event not consumed by any widget — forward to app.
                if let WidgetEvent::KeyDown {
                    key, modifiers: m, ..
                } = &widget_event
                {
                    unhandled_key = Some((*key, *m));
                }
            }
        }

        // --- Deliver widget messages to app ---
        for msg in sink.drain_typed::<A::Message>() {
            let action = app.update(msg);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        // --- Forward unhandled keys to app ---
        if let Some((key, mods)) = unhandled_key {
            let action = app.on_key(key, mods);
            process_action(action, &mut needs_rebuild, &mut needs_repaint);
        }

        // --- Timer tick ---
        if let Some(interval) = app.tick_interval_ms() {
            let now_ms = slopos_windowing::get_time_ms();
            if now_ms.wrapping_sub(last_tick_ms) >= interval {
                last_tick_ms = now_ms;
                let action = app.tick();
                process_action(action, &mut needs_rebuild, &mut needs_repaint);
            }
        }

        // --- Rebuild tree if needed ---
        if needs_rebuild {
            let node = app.view();
            root = tree::build_widget_tree(&node);
            tree::layout_tree(root.as_mut(), window_size, &style);
            focus.rebuild_tab_chain(root.as_ref());
            needs_rebuild = false;
            needs_repaint = true;
        }

        // --- Paint if needed ---
        if needs_repaint {
            if let Some(mut fb) = win.renderer_mut().frame() {
                let fmt = fb.pixel_format();
                fb.clear_canvas(fmt.encode(style.bg_primary));
                let mut ctx = PaintContext::new(&mut fb, &style);
                ctx.focus_visible = focus.is_focus_visible();
                tree::paint_tree(root.as_ref(), &mut ctx);
                overlays.paint(&mut ctx);
            }
            win.renderer().present();
            needs_repaint = false;
        }

        // Sleep until the compositor sends an event, a UiSender posts work,
        // or the next tick/refresh is due.  Replaces the old yield_now()
        // busy-spin with a proper poll()-based sleep.
        let timeout_ms: i64 = if needs_repaint || needs_rebuild {
            0
        } else if let Some(interval) = app.tick_interval_ms() {
            let now = slopos_windowing::get_time_ms();
            let elapsed = now.wrapping_sub(last_tick_ms);
            if elapsed >= interval {
                0
            } else {
                (interval - elapsed) as i64
            }
        } else {
            -1
        };
        // Race the compositor socket, the cross-thread wakeup pipe, and (for a
        // bounded wait) a timer — the async equivalent of `wait_events`.
        //   timeout_ms == 0 — non-blocking: return immediately, no await.
        //   timeout_ms  < 0 — wait indefinitely: race only the two poll_adds.
        //   timeout_ms  > 0 — bounded wait: add a timer arm.
        if timeout_ms < 0 {
            match slopfut::select2(
                slopfut::poll_add(handle.compositor_fd(), POLLIN),
                slopfut::poll_add(handle.wakeup_read_fd(), POLLIN),
            )
            .await
            {
                slopfut::Either2::A(_) => {}
                slopfut::Either2::B(_) => handle.drain_wakeup(),
            }
        } else if timeout_ms > 0 {
            // `sleep_ms` is an `async fn` (not `Unpin`); `Box::pin` makes it
            // usable by the by-reference `select3`. It takes milliseconds and
            // does the ms→ns conversion internally, so feed `timeout_ms`
            // directly — no manual multiply.
            let timer: core::pin::Pin<Box<dyn core::future::Future<Output = ()>>> =
                Box::pin(slopfut::time::sleep_ms(timeout_ms as u64));
            match slopfut::select3(
                slopfut::poll_add(handle.compositor_fd(), POLLIN),
                slopfut::poll_add(handle.wakeup_read_fd(), POLLIN),
                timer,
            )
            .await
            {
                slopfut::Either3::A(_) => {}
                slopfut::Either3::B(_) => handle.drain_wakeup(),
                slopfut::Either3::C(_) => {}
            }
        }
    }
}

fn process_action(action: Action, needs_rebuild: &mut bool, _needs_repaint: &mut bool) {
    match action {
        Action::None => {}
        Action::Rebuild => {
            *needs_rebuild = true;
        }
        Action::Exit => std::process::exit(0),
    }
}

fn fill_pointer_pos(mut event: WidgetEvent, px: i32, py: i32) -> WidgetEvent {
    match &mut event {
        WidgetEvent::PointerDown { x, y, .. } | WidgetEvent::PointerUp { x, y, .. } => {
            *x = px;
            *y = py;
        }
        _ => {}
    }
    event
}

fn update_modifiers(ev: &Event, mods: &mut super::event::Modifiers) {
    match ev {
        Event::KeyPress { scancode, .. } | Event::KeyRelease { scancode, .. } => {
            let pressed = matches!(ev, Event::KeyPress { .. });
            match *scancode {
                0x2A | 0x36 => mods.shift = pressed,
                0x1D => mods.ctrl = pressed,
                0x38 => mods.alt = pressed,
                0x3A if pressed => mods.caps_lock = !mods.caps_lock,
                _ => {}
            }
        }
        _ => {}
    }
}

fn find_focus_policy(widget: &dyn Widget, id: super::traits::WidgetId) -> FocusPolicy {
    if widget.id() == id {
        return widget.focus_policy();
    }
    for child in widget.children() {
        let result = find_focus_policy(child.as_ref(), id);
        if result.is_focusable() {
            return result;
        }
    }
    FocusPolicy::None
}
