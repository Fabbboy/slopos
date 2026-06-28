//! Input event types (Wayland-style per-task input queues)

pub mod keycode;

/// Maximum number of tasks that can have input queues
pub const MAX_INPUT_TASKS: usize = 32;

/// Maximum events per task queue
pub const MAX_EVENTS_PER_TASK: usize = 64;
pub const CLIPBOARD_MAX_SIZE: usize = 4096;

/// Focus type for input_set_focus syscall
pub const INPUT_FOCUS_KEYBOARD: u32 = 0;
pub const INPUT_FOCUS_POINTER: u32 = 1;

/// Keyboard modifier bitfield (returned by `SYSCALL_INPUT_GET_MODIFIER_STATE`).
/// Follows the Wayland `wlr_keyboard_modifier` convention.
pub const MODIFIER_SHIFT: u8 = 1 << 0;
pub const MODIFIER_CTRL: u8 = 1 << 1;
pub const MODIFIER_ALT: u8 = 1 << 2;
pub const MODIFIER_SUPER: u8 = 1 << 3;
pub const MODIFIER_CAPS_LOCK: u8 = 1 << 4;
/// Num Lock active. Carried in the key-event modifier snapshot so a layout-
/// independent consumer can resolve keypad keys (digit vs navigation) without
/// re-tracking lock state.
pub const MODIFIER_NUM_LOCK: u8 = 1 << 5;
/// Scroll Lock active.
pub const MODIFIER_SCROLL_LOCK: u8 = 1 << 6;

/// Key-event flag bits, carried in `data0[8:16]` of a key `InputEvent`.
///
/// Set when the event carries the canonical (HID keycode + text codepoint)
/// payload in addition to the legacy `(scancode, ascii)` bytes. Older consumers
/// that only read `key_scancode()`/`key_ascii()` ignore these and keep working.
pub const KEY_FLAG_HAS_CANONICAL: u8 = 1 << 0;
/// This press is an auto-repeat, not a fresh physical key-down.
pub const KEY_FLAG_IS_REPEAT: u8 = 1 << 1;
/// The key came from the numeric keypad block.
pub const KEY_FLAG_FROM_KEYPAD: u8 = 1 << 2;

/// Axis identifiers for PointerAxis events (Wayland wl_pointer.axis convention)
pub const POINTER_AXIS_VERTICAL: u32 = 0;
pub const POINTER_AXIS_HORIZONTAL: u32 = 1;

/// Type of input event
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputEventType {
    /// Key pressed
    #[default]
    KeyPress = 0,
    /// Key released
    KeyRelease = 1,
    /// Pointer (mouse) motion
    PointerMotion = 2,
    /// Pointer button pressed
    PointerButtonPress = 3,
    /// Pointer button released
    PointerButtonRelease = 4,
    /// Pointer entered surface
    PointerEnter = 5,
    /// Pointer left surface
    PointerLeave = 6,
    /// Window manager requests this app to close gracefully
    CloseRequest = 7,
    /// Compositor notifies app of a new configured size (resize)
    Configure = 8,
    /// Pointer axis (scroll wheel) event
    PointerAxis = 9,
}

impl InputEventType {
    /// Convert from raw u8 value
    #[inline]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::KeyPress),
            1 => Some(Self::KeyRelease),
            2 => Some(Self::PointerMotion),
            3 => Some(Self::PointerButtonPress),
            4 => Some(Self::PointerButtonRelease),
            5 => Some(Self::PointerEnter),
            6 => Some(Self::PointerLeave),
            7 => Some(Self::CloseRequest),
            8 => Some(Self::Configure),
            9 => Some(Self::PointerAxis),
            _ => None,
        }
    }

    /// Returns true if this is a key event (press or release)
    #[inline]
    pub fn is_key_event(self) -> bool {
        matches!(self, Self::KeyPress | Self::KeyRelease)
    }

    /// Returns true if this is a pointer event
    #[inline]
    pub fn is_pointer_event(self) -> bool {
        matches!(
            self,
            Self::PointerMotion
                | Self::PointerButtonPress
                | Self::PointerButtonRelease
                | Self::PointerEnter
                | Self::PointerLeave
                | Self::PointerAxis
        )
    }
}

/// Input event data (union-like structure)
///
/// For key events the two 32-bit words are bit-packed (low→high):
/// ```text
///   data0[ 0: 8]  set-1 scancode (legacy)      -- key_scancode()
///   data0[ 8:16]  KEY_FLAG_* flags             -- key_flags()
///   data0[16:24]  ascii byte (legacy)          -- key_ascii()
///   data0[24:32]  MODIFIER_* snapshot          -- key_modifiers()
///   data1[ 0:16]  canonical HID usage keycode  -- key_keycode()
///   data1[16:32]  text codepoint (BMP; 0=none) -- key_codepoint()
/// ```
/// The legacy `scancode`/`ascii` byte positions are unchanged, so consumers
/// that only call `key_scancode()`/`key_ascii()` are unaffected by the added
/// fields.
///
/// For pointer motion: data0 is x coordinate, data1 is y coordinate
/// For pointer button: data0 contains button code
/// For close request: data0/data1 are zero
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEventData {
    pub data0: u32,
    pub data1: u32,
}

/// A complete input event
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    /// Type of event
    pub event_type: InputEventType,
    /// Padding for alignment
    pub _padding: [u8; 3],
    /// Timestamp in milliseconds since boot
    pub timestamp_ms: u64,
    /// Event-specific data
    pub data: InputEventData,
}

impl Default for InputEvent {
    fn default() -> Self {
        Self {
            event_type: InputEventType::KeyPress,
            _padding: [0; 3],
            timestamp_ms: 0,
            data: InputEventData::default(),
        }
    }
}

impl InputEvent {
    /// Create a key event carrying only `(scancode, ascii)`; the canonical
    /// keycode/codepoint/modifier/flag fields are zero-filled. Prefer
    /// [`key_full`](Self::key_full), which also carries the HID keycode and
    /// text codepoint.
    pub fn key(event_type: InputEventType, scancode: u8, ascii: u8, timestamp_ms: u64) -> Self {
        Self::key_full(event_type, scancode, ascii, 0, 0, 0, 0, timestamp_ms)
    }

    /// Create a fully-populated key event.
    ///
    /// `keycode` is a canonical HID Keyboard/Keypad usage (see
    /// [`keycode`](self::keycode)); `codepoint` is the produced Unicode scalar
    /// (BMP only — the high 16 bits are truncated); `modifiers` is a
    /// `MODIFIER_*` snapshot; `flags` is a `KEY_FLAG_*` bitset.
    #[allow(clippy::too_many_arguments)]
    pub fn key_full(
        event_type: InputEventType,
        scancode: u8,
        ascii: u8,
        keycode: u16,
        codepoint: u32,
        modifiers: u8,
        flags: u8,
        timestamp_ms: u64,
    ) -> Self {
        let data0 = (scancode as u32)
            | ((flags as u32) << 8)
            | ((ascii as u32) << 16)
            | ((modifiers as u32) << 24);
        let data1 = (keycode as u32) | ((codepoint & 0xFFFF) << 16);
        Self {
            event_type,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData { data0, data1 },
        }
    }

    /// Create a pointer motion event
    pub fn pointer_motion(x: i32, y: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::PointerMotion,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: x as u32,
                data1: y as u32,
            },
        }
    }

    /// Create a pointer button event
    pub fn pointer_button(pressed: bool, button: u8, timestamp_ms: u64) -> Self {
        Self {
            event_type: if pressed {
                InputEventType::PointerButtonPress
            } else {
                InputEventType::PointerButtonRelease
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: button as u32,
                data1: 0,
            },
        }
    }

    /// Create a pointer enter/leave event
    pub fn pointer_enter_leave(enter: bool, x: i32, y: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: if enter {
                InputEventType::PointerEnter
            } else {
                InputEventType::PointerLeave
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: x as u32,
                data1: y as u32,
            },
        }
    }

    /// Create a close-request event
    pub fn close_request(timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::CloseRequest,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData { data0: 0, data1: 0 },
        }
    }

    /// Create a configure event (window resize notification)
    pub fn configure(width: u32, height: u32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::Configure,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: width,
                data1: height,
            },
        }
    }

    /// Create a pointer axis (scroll) event.
    ///
    /// `axis`: `POINTER_AXIS_VERTICAL` (0) or `POINTER_AXIS_HORIZONTAL` (1)
    /// `value_v120`: scroll amount in value120 units (±120 = one wheel click).
    ///               Positive = down/right, negative = up/left.
    pub fn pointer_axis(axis: u32, value_v120: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::PointerAxis,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: axis,
                data1: value_v120 as u32,
            },
        }
    }

    /// Extract axis identifier from a PointerAxis event (0 = vertical, 1 = horizontal)
    #[inline]
    pub fn axis_id(&self) -> u32 {
        self.data.data0
    }

    /// Extract scroll value in value120 units from a PointerAxis event.
    /// ±120 = one physical wheel click.
    #[inline]
    pub fn axis_value_v120(&self) -> i32 {
        self.data.data1 as i32
    }

    /// Extract scancode from key event
    #[inline]
    pub fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// Extract ASCII from key event
    #[inline]
    pub fn key_ascii(&self) -> u8 {
        ((self.data.data0 >> 16) & 0xFF) as u8
    }

    /// Extract the `KEY_FLAG_*` bitset from a key event.
    #[inline]
    pub fn key_flags(&self) -> u8 {
        ((self.data.data0 >> 8) & 0xFF) as u8
    }

    /// Extract the `MODIFIER_*` snapshot captured when the key event was produced.
    #[inline]
    pub fn key_modifiers(&self) -> u8 {
        ((self.data.data0 >> 24) & 0xFF) as u8
    }

    /// Extract the canonical HID usage keycode (0 = none / legacy event).
    #[inline]
    pub fn key_keycode(&self) -> u16 {
        (self.data.data1 & 0xFFFF) as u16
    }

    /// Extract the produced text codepoint. Falls back to the legacy `ascii`
    /// byte when the canonical codepoint field is empty, so legacy and canonical
    /// events both yield a usable character.
    #[inline]
    pub fn key_codepoint(&self) -> u32 {
        let cp = (self.data.data1 >> 16) & 0xFFFF;
        if cp != 0 { cp } else { self.key_ascii() as u32 }
    }

    /// True if this event carries the canonical keycode/codepoint payload.
    #[inline]
    pub fn key_has_canonical(&self) -> bool {
        self.key_flags() & KEY_FLAG_HAS_CANONICAL != 0
    }

    /// True if this press is an auto-repeat rather than a fresh key-down.
    #[inline]
    pub fn key_is_repeat(&self) -> bool {
        self.key_flags() & KEY_FLAG_IS_REPEAT != 0
    }

    /// True if the key originated from the numeric keypad block.
    #[inline]
    pub fn key_from_keypad(&self) -> bool {
        self.key_flags() & KEY_FLAG_FROM_KEYPAD != 0
    }

    /// Extract X coordinate from pointer event
    #[inline]
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    /// Extract Y coordinate from pointer event
    #[inline]
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    /// Extract button from pointer button event
    #[inline]
    pub fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// Extract width from configure event
    #[inline]
    pub fn configure_width(&self) -> u32 {
        self.data.data0
    }

    /// Extract height from configure event
    #[inline]
    pub fn configure_height(&self) -> u32 {
        self.data.data1
    }
}

#[cfg(test)]
mod tests {
    use super::keycode::*;
    use super::*;

    #[test]
    fn legacy_key_zero_fills_new_fields() {
        // Legacy layout: scancode in the low byte, ascii in byte 2, the
        // canonical fields zero.
        let e = InputEvent::key(InputEventType::KeyPress, 0x1E, b'a', 42);
        assert_eq!(e.key_scancode(), 0x1E);
        assert_eq!(e.key_ascii(), b'a');
        assert_eq!(e.data.data0, 0x1E | ((b'a' as u32) << 16));
        assert_eq!(e.data.data1, 0);
        // New accessors read as empty.
        assert_eq!(e.key_flags(), 0);
        assert_eq!(e.key_modifiers(), 0);
        assert_eq!(e.key_keycode(), 0);
        assert!(!e.key_has_canonical());
        assert!(!e.key_is_repeat());
        assert!(!e.key_from_keypad());
        // codepoint falls back to the ascii byte when the canonical field is 0.
        assert_eq!(e.key_codepoint(), b'a' as u32);
    }

    #[test]
    fn full_key_round_trips_all_fields() {
        let flags = KEY_FLAG_HAS_CANONICAL | KEY_FLAG_FROM_KEYPAD;
        let mods = MODIFIER_SHIFT | MODIFIER_CTRL;
        let e = InputEvent::key_full(
            InputEventType::KeyPress,
            0x4F,     // set-1 scancode (KP 1 with numlock)
            b'1',     // legacy ascii
            KEY_KP_1, // canonical HID usage
            '1' as u32,
            mods,
            flags,
            1000,
        );
        assert_eq!(e.key_scancode(), 0x4F);
        assert_eq!(e.key_ascii(), b'1');
        assert_eq!(e.key_flags(), flags);
        assert_eq!(e.key_modifiers(), mods);
        assert_eq!(e.key_keycode(), KEY_KP_1);
        assert_eq!(e.key_codepoint(), '1' as u32);
        assert!(e.key_has_canonical());
        assert!(e.key_from_keypad());
        assert!(!e.key_is_repeat());
    }

    #[test]
    fn codepoint_prefers_canonical_over_ascii() {
        // Non-ASCII codepoint with no legacy ascii byte.
        let e = InputEvent::key_full(
            InputEventType::KeyPress,
            0x10,
            0, // ascii 0 (legacy can't represent this char)
            KEY_Q,
            0x00E9, // 'é'
            0,
            KEY_FLAG_HAS_CANONICAL,
            0,
        );
        assert_eq!(e.key_codepoint(), 0x00E9);
        assert_eq!(e.key_ascii(), 0);
    }

    #[test]
    fn keycode_classifiers() {
        assert!(is_modifier(KEY_LEFTCTRL));
        assert!(is_modifier(KEY_RIGHTMETA));
        assert!(!is_modifier(KEY_A));
        assert!(is_keypad(KEY_KP_0));
        assert!(is_keypad(KEY_KP_SLASH));
        assert!(!is_keypad(KEY_NUMLOCK)); // NumLock is not a keypad text key
        assert!(!is_keypad(KEY_HOME));
    }
}
