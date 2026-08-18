//! Touchpad gesture engine: multitouch contact frames in, relative-pointer
//! events out — motion, buttons, scroll axis, tap-to-click. Apps never see the
//! raw absolute/multitouch coordinates.

use crate::input_event;

pub const MAX_CONTACTS: usize = 5;

const BUTTON_LEFT: u8 = 0x01;

#[derive(Clone, Copy, Default)]
pub struct Contact {
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub tip: bool,
}

pub struct Frame {
    pub contacts: [Contact; MAX_CONTACTS],
    pub count: usize,
    /// Physical clickpad button.
    pub button: bool,
}

impl Frame {
    pub fn empty() -> Self {
        Self {
            contacts: [Contact::default(); MAX_CONTACTS],
            count: 0,
            button: false,
        }
    }
}

const TAP_MAX_FRAMES: u32 = 12; // poll intervals
const TAP_MAX_MOVE: i32 = 60; // pad units
const SCROLL_STEP: i32 = 80; // pad units per notch
const SCROLL_NOTCH_V120: i32 = 120;

pub struct GestureEngine {
    w: i32,
    h: i32,
    cx: i32,
    cy: i32,
    pad_max_x: i32,
    pad_max_y: i32,

    active_id: Option<u8>,
    last_x: i32,
    last_y: i32,

    scroll_id_a: Option<u8>,
    scroll_last_y: i32,
    scroll_last_x: i32,
    scroll_accum_y: i32,
    scroll_accum_x: i32,

    finger_was_down: bool,
    max_fingers_in_gesture: usize,
    gesture_frames: u32,
    gesture_moved: bool,
    gesture_start_x: i32,
    gesture_start_y: i32,

    btn_down: bool,
}

impl GestureEngine {
    pub fn new(width: i32, height: i32, pad_max_x: i32, pad_max_y: i32) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        Self {
            w,
            h,
            cx: w / 2,
            cy: h / 2,
            pad_max_x: pad_max_x.max(1),
            pad_max_y: pad_max_y.max(1),
            active_id: None,
            last_x: 0,
            last_y: 0,
            scroll_id_a: None,
            scroll_last_y: 0,
            scroll_last_x: 0,
            scroll_accum_y: 0,
            scroll_accum_x: 0,
            finger_was_down: false,
            max_fingers_in_gesture: 0,
            gesture_frames: 0,
            gesture_moved: false,
            gesture_start_x: 0,
            gesture_start_y: 0,
            btn_down: false,
        }
    }

    pub fn set_bounds(&mut self, width: i32, height: i32) {
        self.w = width.max(1);
        self.h = height.max(1);
        self.cx = self.cx.clamp(0, self.w - 1);
        self.cy = self.cy.clamp(0, self.h - 1);
    }

    pub fn process(&mut self, frame: &Frame, ts: u64) {
        if frame.button != self.btn_down {
            self.btn_down = frame.button;
            input_event::input_route_pointer_button(BUTTON_LEFT, frame.button, ts);
        }

        let down = frame.count;
        if down > self.max_fingers_in_gesture {
            self.max_fingers_in_gesture = down;
        }
        if down > 0 {
            self.gesture_frames = self.gesture_frames.saturating_add(1);
        }

        match down {
            1 => self.handle_one_finger(frame, ts),
            n if n >= 2 => self.handle_two_finger(frame, ts),
            _ => {}
        }

        if down == 0 && self.finger_was_down {
            self.on_lift(ts);
        }
        self.finger_was_down = down > 0;
    }

    fn handle_one_finger(&mut self, frame: &Frame, ts: u64) {
        self.reset_scroll();
        let Some(c) = frame.contacts[..frame.count].iter().find(|c| c.tip) else {
            return;
        };
        if self.active_id != Some(c.id) {
            // New finger: baseline only, no motion emitted.
            self.active_id = Some(c.id);
            self.last_x = c.x;
            self.last_y = c.y;
            self.gesture_start_x = c.x;
            self.gesture_start_y = c.y;
            self.gesture_moved = false;
            return;
        }
        let dx = c.x - self.last_x;
        let dy = c.y - self.last_y;
        self.last_x = c.x;
        self.last_y = c.y;

        if (c.x - self.gesture_start_x).abs() > TAP_MAX_MOVE
            || (c.y - self.gesture_start_y).abs() > TAP_MAX_MOVE
        {
            self.gesture_moved = true;
        }

        let sx = self.scale_x(dx);
        let sy = self.scale_y(dy);
        if sx != 0 || sy != 0 {
            self.cx = (self.cx + sx).clamp(0, self.w - 1);
            self.cy = (self.cy + sy).clamp(0, self.h - 1);
            input_event::input_route_pointer_motion(self.cx, self.cy, ts);
        }
    }

    fn handle_two_finger(&mut self, frame: &Frame, ts: u64) {
        self.active_id = None; // not driving the cursor with 2 fingers
        let mut it = frame.contacts[..frame.count].iter().filter(|c| c.tip);
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            return;
        };
        let avg_x = (a.x + b.x) / 2;
        let avg_y = (a.y + b.y) / 2;
        if self.scroll_id_a != Some(a.id) {
            self.scroll_id_a = Some(a.id);
            self.scroll_last_x = avg_x;
            self.scroll_last_y = avg_y;
            self.scroll_accum_x = 0;
            self.scroll_accum_y = 0;
            return;
        }
        self.scroll_accum_y += avg_y - self.scroll_last_y;
        self.scroll_accum_x += avg_x - self.scroll_last_x;
        self.scroll_last_x = avg_x;
        self.scroll_last_y = avg_y;

        while self.scroll_accum_y.abs() >= SCROLL_STEP {
            let dir = self.scroll_accum_y.signum();
            self.scroll_accum_y -= dir * SCROLL_STEP;
            input_event::input_route_pointer_axis(0, dir * SCROLL_NOTCH_V120, ts);
        }
        while self.scroll_accum_x.abs() >= SCROLL_STEP {
            let dir = self.scroll_accum_x.signum();
            self.scroll_accum_x -= dir * SCROLL_STEP;
            input_event::input_route_pointer_axis(1, dir * SCROLL_NOTCH_V120, ts);
        }
    }

    fn on_lift(&mut self, ts: u64) {
        let tapped = self.max_fingers_in_gesture == 1
            && self.gesture_frames <= TAP_MAX_FRAMES
            && !self.gesture_moved
            && !self.btn_down;
        if tapped {
            input_event::input_route_pointer_button(BUTTON_LEFT, true, ts);
            input_event::input_route_pointer_button(BUTTON_LEFT, false, ts);
        }
        self.active_id = None;
        self.reset_scroll();
        self.max_fingers_in_gesture = 0;
        self.gesture_frames = 0;
        self.gesture_moved = false;
    }

    fn reset_scroll(&mut self) {
        self.scroll_id_a = None;
        self.scroll_accum_x = 0;
        self.scroll_accum_y = 0;
    }

    fn scale_x(&self, dpad: i32) -> i32 {
        accelerate(dpad, self.w, self.pad_max_x)
    }
    fn scale_y(&self, dpad: i32) -> i32 {
        accelerate(dpad, self.h, self.pad_max_y)
    }
}

fn accelerate(dpad: i32, screen: i32, pad_max: i32) -> i32 {
    if dpad == 0 || pad_max <= 0 {
        return 0;
    }
    // Full-pad swipe ≈ full screen, times a sensitivity of 1.4.
    let base = dpad * screen * 14 / (pad_max * 10);
    let extra = if dpad.abs() > pad_max / 16 {
        base / 2
    } else {
        0
    };
    let v = base + extra;
    if v == 0 {
        // Preserve sub-pixel intent so slow drags still move.
        dpad.signum()
    } else {
        v
    }
}
