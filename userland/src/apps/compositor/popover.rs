//! The system bar's popover: compositor-owned chrome anchored to a status item.
//!
//! Drawn by the compositor rather than by a client: it renders state the
//! compositor already holds, its light-dismiss pointer grab has to live here
//! whichever process draws the pixels, and it must stay available when
//! `ProtocolBridge::new()` returns `None`. Placement comes from
//! [`slopos_chrome_core::positioner`], not arithmetic here.

use core::fmt::Write;
use std::string::String;

use slopos_chrome_core::netstate::{
    GATEWAY_LABEL, IFACE_NO_CARRIER_LABEL, IFACE_OFF_LABEL, IFACE_SEPARATOR, IfaceState,
    NO_ADDRESS_LABEL, NetPanelModel, PANEL_TITLE, TRUNCATION, iface_state, indicator_label,
    indicator_state_for,
};
use slopos_chrome_core::positioner::{Positioner, Rect, Size, position};
use slopos_chrome_core::status::StatusKind;
use slopos_chrome_core::toggle::{TOGGLE_OFF, TOGGLE_ON, toggle_geometry};
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_net_core::Ipv4;
use slopos_net_core::render::iface_kind;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::process;
use crate::theme::*;
use std::time::Instant;

/// Width of the network panel. The widest line the model can produce — a detail
/// row of 26 cells — fits the 280 px budget from the second rail; anything
/// longer is truncated by [`fit`] rather than run under the panel edge.
const PANEL_WIDTH: i32 = 320;

/// The base spacing unit. Every gap in the panel is `U * k` for `k` in 1..=4.
///
/// Four, because every fixed dimension the panel inherits is already a multiple
/// of it, and because the panel needs a group gap exactly three times its row
/// gap, which 12:4 gives and an 8-only scale cannot. `k >= 5` is excluded: 16 to
/// 20 is +25%, at the just-noticeable-difference floor, so a 20 beside a 16
/// reads as a mistake rather than a decision.
const U: i32 = 4;
const SPACE_1: i32 = U;
const SPACE_2: i32 = 2 * U;
const SPACE_3: i32 = 3 * U;
const SPACE_4: i32 = 4 * U;

/// Padding inside the panel, on all four edges equally.
const PANEL_PAD: i32 = SPACE_3;

/// Diameter of the per-interface status dot. Ink, not a gap.
const DOT_D: i32 = SPACE_2;

/// The step from the panel's own rail to the interface rail. One indivisible
/// rail step, not `DOT_D + something`: the dot merely happens to live in the
/// first half of it.
const DOT_GUTTER: i32 = SPACE_4;

/// The panel's one hairline weight, for its edge and for the header rule.
const RULE_H: i32 = 1;

/// Corner radius: the shelf's pill.
const PANEL_RADIUS: i32 = SHELF_PILL_RADIUS;

/// Gap between the bar's border row and the top of the panel.
const PANEL_GAP_Y: i32 = SPACE_2;

/// The switch's track. Ink, and at the hit-target minimum.
const SWITCH_W: i32 = 10 * U;
const SWITCH_H: i32 = 5 * U;

/// One line of text, asked rather than assumed.
///
/// No floor of its own: [`gfx::cell_height`] already answers its own fallback
/// when no atlas is loaded, and a second floor here would be a claim that the
/// first can fail.
fn line_h() -> i32 {
    gfx::cell_height()
}

/// Where every element sits, computed once.
///
/// One function rather than a `header_h`/`row_h`/`footer_h` trio: those are
/// three partial views of one layout, and they leave the rule's position to two
/// expressions that have to agree by hand.
struct Layout {
    height: i32,
    title_y: i32,
    status_y: i32,
    switch_y: i32,
    rule_y: i32,
    has_rule: bool,
    rows_y: i32,
    row_pitch: i32,
    gateway_y: i32,
    has_gateway: bool,
}

fn layout(model: &NetPanelModel) -> Layout {
    let lh = line_h();
    let n = model.listed_count() as i32;
    let gw = model.gateway != [0; 4];

    let title_y = PANEL_PAD;
    let status_y = title_y + lh;
    // Centred on the title's line, not on the two-line header: the switch acts
    // on the panel, and the title is the panel's own line.
    let switch_y = title_y + ((lh - SWITCH_H) / 2).max(0);
    let header_end = title_y + 2 * lh;

    // A rule with nothing beneath it is a seam, not a division.
    let has_rule = n >= 1 || gw;
    let rule_y = header_end + SPACE_3;
    let body_y = if has_rule {
        rule_y + RULE_H + SPACE_3
    } else {
        header_end
    };

    let row_pitch = 2 * lh + SPACE_1;
    let rows_y = body_y;
    // n rows of 2 lines with SPACE_1 between them, not after the last.
    let rows_end = if n >= 1 {
        rows_y + n * row_pitch - SPACE_1
    } else {
        rows_y
    };
    let gateway_y = if n >= 1 { rows_end + SPACE_3 } else { rows_end };
    let content_end = if gw { gateway_y + lh } else { rows_end };

    Layout {
        height: content_end + PANEL_PAD,
        title_y,
        status_y,
        switch_y,
        rule_y,
        has_rule,
        rows_y,
        row_pitch,
        gateway_y,
        has_gateway: gw,
    }
}

/// Fit `s` into `max_w`, truncating with [`TRUNCATION`] if it does not.
///
/// Always writes into `out` and returns a borrow of `out` alone, so the result
/// aliases exactly one buffer — which is what lets a caller compose into one
/// `String` field and fit into another without borrowing the same one twice.
fn fit<'a>(out: &'a mut String, s: &str, max_w: i32) -> &'a str {
    out.clear();
    if gfx::font::string_width(s) <= max_w {
        out.push_str(s);
        return out.as_str();
    }
    let budget = max_w - gfx::font::string_width(TRUNCATION);
    if budget > 0 {
        for ch in s.chars() {
            out.push(ch);
            if gfx::font::string_width(out.as_str()) > budget {
                out.pop();
                break;
            }
        }
        out.push_str(TRUNCATION);
    }
    out.as_str()
}

/// The dot's colour for each row state.
///
/// Every one clears 3:1 against the panel, the WCAG floor for informational
/// non-text — the states that mean something is wrong most of all.
fn dot_colour(state: IfaceState) -> slopos_abi::draw::Color32 {
    match state {
        IfaceState::Off => TEXT_SECONDARY,
        IfaceState::NoCarrier => SIGNAL_CLOSE,
        IfaceState::NoAddress => SIGNAL_MINIMIZE,
        IfaceState::Up => SIGNAL_EXPAND,
    }
}

/// How long a requested switch change may stay unconfirmed.
///
/// The control shows what was asked for immediately and settles when the
/// kernel's own event agrees — the shape a NetworkManager client uses over
/// D-Bus, and viable here only because the monitor fd demonstrably wakes a
/// blocked reader. The deadline is what stops a lost completion from wedging
/// the control in a busy state forever: at expiry it reverts to whatever the
/// kernel actually says, which may be the old value.
const SWITCH_SETTLE_MS: u128 = 10_000;

/// How long to wait for the kernel to agree before asking again.
///
/// `ip net on|off` is idempotent, so re-issuing is safe, and re-issuing is what
/// makes the control converge rather than sit in a busy state waiting out
/// [`SWITCH_SETTLE_MS`]. Two presses spawn two children and nothing orders
/// their execution, so the pair can land in the wrong order and leave the
/// kernel in the state the user asked for *first*.
const SWITCH_RETRY_MS: u128 = 750;

/// Popover state: which item owns it, and where it was put.
pub struct Popover {
    open: Option<StatusKind>,
    rect: Rect,
    /// Where it was last frame, so closing or moving repaints what it vacated.
    prev_rect: Rect,
    /// What the switch was last asked for, while the kernel has yet to agree.
    pending: Option<bool>,
    /// The switch changed appearance without the panel moving, so the panel
    /// needs a repaint that the rect comparison alone would not ask for.
    switch_dirty: bool,
    /// When the target was last changed, for [`SWITCH_SETTLE_MS`].
    pending_since: Option<Instant>,
    /// When a command was last spawned, for [`SWITCH_RETRY_MS`].
    issued_at: Option<Instant>,
    /// Composition buffer for a line being built.
    line: String,
    /// Output buffer for [`fit`]. Separate from `line` so a composed string can
    /// be fitted without borrowing one buffer twice.
    fitted: String,
    /// The item the panel was anchored under, kept so a resize re-anchors to
    /// the same place instead of to a synthesised rect one pixel off.
    anchor: Rect,
}

impl Popover {
    pub fn new() -> Self {
        Self {
            open: None,
            rect: Rect::EMPTY,
            prev_rect: Rect::EMPTY,
            pending: None,
            pending_since: None,
            issued_at: None,
            switch_dirty: false,
            line: String::new(),
            fitted: String::new(),
            anchor: Rect::EMPTY,
        }
    }

    /// The popover's rect while it is open.
    ///
    /// `None` when closed, which is what `resolve_cursor_hit` reads as "no
    /// grab" — so the closed state cannot accidentally swallow a click.
    pub fn rect(&self) -> Option<Rect> {
        if self.open.is_some() && !self.rect.is_empty() {
            Some(self.rect)
        } else {
            None
        }
    }

    /// Open the popover for `kind` under `item`, or close it if that item's
    /// popover is already the open one.
    pub fn toggle(
        &mut self,
        kind: StatusKind,
        item: DamageRect,
        model: &NetPanelModel,
        screen_w: u32,
        screen_h: u32,
    ) {
        if self.open == Some(kind) {
            self.dismiss();
            return;
        }
        let anchor = Rect::new(
            item.x0,
            item.y0,
            item.x1 - item.x0 + 1,
            item.y1 - item.y0 + 1,
        );
        self.anchor = anchor;
        // Sized from the model at open, so the panel does not appear at
        // header height and resize on the next frame.
        let size = Size {
            w: PANEL_WIDTH,
            h: if kind == StatusKind::Network {
                layout(model).height
            } else {
                2 * PANEL_PAD + 2 * line_h()
            },
        };
        let work_area = work_area(screen_w, screen_h);
        self.rect = position(
            &Positioner::below_bar_item(anchor, size, PANEL_GAP_Y),
            work_area,
        );
        self.open = Some(kind);
    }

    /// Close the popover. Idempotent: a dismiss with nothing open is what a
    /// click on the desktop looks like, and must not manufacture damage.
    pub fn dismiss(&mut self) {
        if self.open.is_some() {
            self.open = None;
        }
    }

    /// Resize the open popover to fit `model`, so the panel is as tall as its
    /// content rather than a guess.
    pub fn fit_to(&mut self, model: &NetPanelModel, screen_w: u32, screen_h: u32) {
        if self.open != Some(StatusKind::Network) || self.rect.is_empty() {
            return;
        }
        let wanted = layout(model).height;
        if wanted == self.rect.h {
            return;
        }
        // The anchor the panel was opened under, not a synthesised one: the two
        // differ by a pixel, which moves the panel's top edge on resize.
        self.rect = position(
            &Positioner::below_bar_item(
                self.anchor,
                Size {
                    w: self.rect.w,
                    h: wanted,
                },
                PANEL_GAP_Y,
            ),
            work_area(screen_w, screen_h),
        );
    }

    /// The switch's track, from the panel rect.
    ///
    /// One function, used by both the draw and the hit test. Two copies of
    /// this arithmetic is how a control comes to ignore a press that visibly
    /// lands on it.
    fn switch_track(rect: Rect, l: &Layout) -> Rect {
        Rect::new(
            rect.right() - PANEL_PAD - SWITCH_W,
            rect.y + l.switch_y,
            SWITCH_W,
            SWITCH_H,
        )
    }

    /// The switch's press target: the track, padded to the header band.
    ///
    /// Separate from the draw geometry because a 20 px control with zero hit
    /// padding is a control people miss.
    fn switch_hit(rect: Rect, l: &Layout) -> Rect {
        let track = Self::switch_track(rect, l);
        Rect::new(
            track.x - SPACE_2,
            rect.y + PANEL_PAD,
            track.w + SPACE_2 + PANEL_PAD,
            line_h(),
        )
    }

    /// The position the switch should draw in: what was asked for while a
    /// request is outstanding, otherwise what the kernel says.
    fn switch_on(&self, model: &NetPanelModel) -> bool {
        self.pending.unwrap_or(model.enabled)
    }

    /// Settle or abandon an outstanding request.
    ///
    /// Called every frame. The kernel agreeing is the normal exit; the
    /// deadline is the one that matters, because without it a request whose
    /// completion never arrives leaves the control permanently busy and
    /// permanently lying about the state.
    pub fn settle(&mut self, model: &NetPanelModel) {
        let Some(wanted) = self.pending else {
            return;
        };
        if model.enabled == wanted {
            self.clear_pending();
            return;
        }
        if self
            .pending_since
            .is_none_or(|since| since.elapsed().as_millis() >= SWITCH_SETTLE_MS)
        {
            // Given up on. The control reverts to whatever the kernel actually
            // says, which may be the old value — a wrong switch position is
            // worse than a refused one.
            self.clear_pending();
            return;
        }
        // Not agreed yet and not yet abandoned: ask again. Without this the
        // control has no way to recover from a command that was lost or that
        // landed before an earlier one, and simply displays the target in a
        // busy state until the deadline expires.
        if self
            .issued_at
            .is_none_or(|at| at.elapsed().as_millis() >= SWITCH_RETRY_MS)
        {
            self.issue(wanted);
        }
    }

    /// Spawn the command for `wanted` and note when.
    fn issue(&mut self, wanted: bool) {
        if spawn_ip_net(wanted) {
            self.issued_at = Some(Instant::now());
        }
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.pending_since = None;
        self.issued_at = None;
        self.switch_dirty = true;
    }

    /// Route a press inside the panel. Returns whether anything took it.
    ///
    /// The mutation is not issued here and cannot be: the compositor holds
    /// `TASK_FLAG_COMPOSITOR` and not `NET_ADMIN`, so it spawns the one
    /// program that does. Exactly one process in the system can change network
    /// state, and the compositor's trusted surface does not grow to include
    /// it.
    pub fn press(&mut self, x: i32, y: i32, model: &NetPanelModel) -> bool {
        // Only the network panel has controls. Without this a click on another
        // kind's popover — which draws nothing — reports "taken" and eats the
        // press.
        if self.open != Some(StatusKind::Network) {
            return false;
        }
        let Some(rect) = self.rect() else {
            return false;
        };
        if !Self::switch_hit(rect, &layout(model)).contains(x, y) {
            return false;
        }
        // Toggle from what the switch is SHOWING, not from what the kernel
        // last said. While a request is outstanding those differ, and reading
        // the kernel value makes a press during the busy window re-request the
        // state already being asked for, so the switch cannot be turned back
        // until it settles.
        let wanted = !self.switch_on(model);
        if self.pending == Some(wanted) {
            return true;
        }
        // A press during an outstanding request retargets it rather than being
        // dropped. Dropping it loses the person's intent silently: the switch
        // under their finger does not move, and the machine settles into the
        // state they just asked it to leave.
        self.pending = Some(wanted);
        self.pending_since = Some(Instant::now());
        self.switch_dirty = true;
        self.issue(wanted);
        true
    }

    /// Rects to repaint: whatever the popover covered last frame and whatever
    /// it covers now. Emitted only when the two differ, so an open popover
    /// over an idle network costs nothing per frame.
    pub fn take_damage(&mut self, out: &mut [DamageRect]) -> usize {
        let current = self.rect().unwrap_or(Rect::EMPTY);
        if current == self.prev_rect && !self.switch_dirty {
            return 0;
        }
        self.switch_dirty = false;
        let mut count = 0usize;
        for rect in [self.prev_rect, current] {
            if !rect.is_empty() && count < out.len() {
                out[count] = DamageRect {
                    x0: rect.x,
                    y0: rect.y,
                    x1: rect.right() - 1,
                    y1: rect.bottom() - 1,
                };
                count += 1;
            }
        }
        self.prev_rect = current;
        count
    }

    /// Draw the open popover.
    pub fn draw(&mut self, buf: &mut DrawBuffer, model: &NetPanelModel, clip: &DamageRect) {
        let Some(rect) = self.rect() else {
            return;
        };
        if self.open != Some(StatusKind::Network) {
            return;
        }

        let on = self.switch_on(model);
        let busy = self.pending.is_some();
        let l = layout(model);
        // Split borrows: the closure composes into `line` and fits into
        // `fitted`, so it cannot hold `&self`.
        let Self { line, fitted, .. } = self;

        // Two rails, and one rule for them: rail1 is where the panel speaks
        // about itself, rail2 where it speaks about an interface.
        let rail1 = rect.x + PANEL_PAD;
        let rail2 = rail1 + DOT_GUTTER;
        let content_w = rect.w - 2 * PANEL_PAD;
        let header_text_w = content_w - SWITCH_W - SPACE_3;
        let row_text_w = content_w - DOT_GUTTER;
        let gw_value_x = rail1 + gfx::font::string_width(GATEWAY_LABEL) + SPACE_3;
        let gw_value_w = rect.x + rect.w - PANEL_PAD - gw_value_x;

        buf.with_scissor(*clip, |buf| {
            // Two concentric opaque fills, not fill-then-outline: a stroked
            // rounded rect blends its corner arcs against the fill's own
            // antialiased boundary and fades out across each corner, leaving a
            // border with four holes in it. Giving the hairline its own pixels
            // keeps the edge solid the whole way round.
            rounded_rect_filled(
                buf,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                PANEL_RADIUS,
                SHELF_SEPARATOR,
            );
            rounded_rect_filled(
                buf,
                rect.x + RULE_H,
                rect.y + RULE_H,
                rect.w - 2 * RULE_H,
                rect.h - 2 * RULE_H,
                PANEL_RADIUS - RULE_H,
                PANEL_BG_OPAQUE,
            );

            // The colour rule, panel-wide: TEXT_PRIMARY names the thing,
            // TEXT_SECONDARY describes it. With one font size and no weight
            // axis, colour is the only channel that can say "this matters
            // more", so it is not asked to say anything else.
            //
            // The title is the dimmer of the two. Someone who clicked the
            // network indicator does not need to be told the panel is about
            // the network; the sentence underneath is what they came for.
            let text = fit(fitted, PANEL_TITLE, header_text_w);
            gfx::draw_str_clipped(
                buf,
                rail1,
                rect.y + l.title_y,
                text,
                TEXT_SECONDARY,
                PANEL_BG_OPAQUE,
                clip,
            );
            let text = fit(
                fitted,
                indicator_label(indicator_state_for(model)),
                header_text_w,
            );
            gfx::draw_str_clipped(
                buf,
                rail1,
                rect.y + l.status_y,
                text,
                TEXT_PRIMARY,
                PANEL_BG_OPAQUE,
                clip,
            );

            // The switch. Busy while a request is outstanding, so the control
            // says "asked, not yet confirmed" rather than showing a position
            // the kernel has not agreed to. Presence increases with state —
            // off, busy, on.
            let track = Self::switch_track(rect, &l);
            // Busy keeps the TARGET's own colour and dims only the knob: a
            // grey track under a knob in the on position reads as a disabled
            // control rather than a moving one.
            let track_colour = if on { SIGNAL_EXPAND } else { SIGNAL_INACTIVE };
            rounded_rect_filled(
                buf,
                track.x,
                track.y,
                track.w,
                track.h,
                track.h / 2,
                track_colour,
            );
            let geometry = toggle_geometry(track, if on { TOGGLE_ON } else { TOGGLE_OFF });
            if !geometry.knob.is_empty() {
                rounded_rect_filled(
                    buf,
                    geometry.knob.x,
                    geometry.knob.y,
                    geometry.knob.w,
                    geometry.knob.h,
                    geometry.knob.w / 2,
                    if busy { TEXT_SECONDARY } else { TEXT_PRIMARY },
                );
            }

            // Inset to the text rail rather than full-bleed: a rule that
            // reaches the panel's edges runs into the corner arcs and reads as
            // a window seam instead of a group division.
            if l.has_rule {
                rounded_rect_filled(
                    buf,
                    rail1,
                    rect.y + l.rule_y,
                    content_w,
                    RULE_H,
                    0,
                    SHELF_SEPARATOR,
                );
            }

            // One row per interface. The state is a coloured dot and a plain
            // word, never an RFC 2863 spelling: `UNKNOWN` and `LOWERLAYERDOWN`
            // belong in `ip link`, and in a status menu they ask a question
            // instead of answering one.
            for (i, iface) in model.listed_ifaces().enumerate() {
                let row_top = rect.y + l.rows_y + i as i32 * l.row_pitch;
                let state = iface_state(iface);

                // Centred on the name's line, which puts the disc's centre on
                // the cap band's optical centre for this font.
                rounded_rect_filled(
                    buf,
                    rail1,
                    row_top + (line_h() - DOT_D) / 2,
                    DOT_D,
                    DOT_D,
                    DOT_D / 2,
                    dot_colour(state),
                );

                let text = fit(fitted, iface.name_str(), row_text_w);
                gfx::draw_str_clipped(
                    buf,
                    rail2,
                    row_top,
                    text,
                    TEXT_PRIMARY,
                    PANEL_BG_OPAQUE,
                    clip,
                );

                line.clear();
                let _ = write!(line, "{}{}", iface_kind(iface.kind.abi()), IFACE_SEPARATOR);
                match state {
                    IfaceState::Off => line.push_str(IFACE_OFF_LABEL),
                    IfaceState::NoCarrier => line.push_str(IFACE_NO_CARRIER_LABEL),
                    IfaceState::NoAddress => line.push_str(NO_ADDRESS_LABEL),
                    IfaceState::Up => {
                        let _ = write!(line, "{}/{}", Ipv4(iface.ipv4), iface.prefix_len);
                    }
                }
                // Never dimmed by state. The fault is carried by the dot's hue
                // and by the status word — both additions of contrast. A
                // disabled grey measures 2.5:1 on exactly the line someone
                // opens this panel to read when something is wrong.
                let text = fit(fitted, line, row_text_w);
                gfx::draw_str_clipped(
                    buf,
                    rail2,
                    row_top + line_h(),
                    text,
                    TEXT_SECONDARY,
                    PANEL_BG_OPAQUE,
                    clip,
                );
            }

            // The gateway, once. At rail1, not rail2: it is not an interface
            // and must not sit in the dot column with a permanently empty
            // gutter, which reads as an orphaned row. Absent rather than shown
            // as 0.0.0.0 when there is no default route.
            if l.has_gateway {
                let y = rect.y + l.gateway_y;
                gfx::draw_str_clipped(
                    buf,
                    rail1,
                    y,
                    GATEWAY_LABEL,
                    TEXT_SECONDARY,
                    PANEL_BG_OPAQUE,
                    clip,
                );
                line.clear();
                let _ = write!(line, "{}", Ipv4(model.gateway));
                // A fixed key column, not a right rail: nothing else in the
                // panel ends at the right edge, and right-aligning would open a
                // trough that changes width with the address.
                let text = fit(fitted, line, gw_value_w);
                gfx::draw_str_clipped(
                    buf,
                    gw_value_x,
                    y,
                    text,
                    TEXT_PRIMARY,
                    PANEL_BG_OPAQUE,
                    clip,
                );
            }
        });
    }
}

/// The region a popover may occupy: the screen below the bar, inset on the
/// other three sides.
///
/// No inset on top — the gap below the bar is the positioner's offset, and
/// insetting here as well would count it twice.
fn work_area(screen_w: u32, screen_h: u32) -> Rect {
    let top = SYSTEM_BAR_HEIGHT + 1;
    Rect::new(
        PANEL_PAD,
        top,
        screen_w as i32 - 2 * PANEL_PAD,
        screen_h as i32 - top - PANEL_PAD,
    )
}

/// Ask `/bin/ip` to move the master networking switch.
///
/// Spawned rather than issued directly: `net_iface_ctl` needs `NET_ADMIN`, the
/// compositor is granted only `TASK_FLAG_COMPOSITOR`, and `/bin/ip` is the one
/// program the kernel's program-identity table confers `NET_ADMIN` on. Asking
/// for the capability here would widen the trusted surface of the process that
/// already owns the framebuffer and every input event.
///
/// Returns whether the child started. The result of the operation itself
/// arrives later, as a `net_monitor` event.
fn spawn_ip_net(enable: bool) -> bool {
    let verb: &[u8] = if enable { b"on\0" } else { b"off\0" };
    let argv: [*const u8; 3] = [b"ip\0".as_ptr(), b"net\0".as_ptr(), verb.as_ptr()];
    // stdout and stderr only: `ip` reads nothing, and handing it the
    // compositor's stdin would put a child on the console's input.
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_path_with_actions(
        b"/bin/ip",
        &argv,
        slopos_abi::task::TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    tid > 0
}
