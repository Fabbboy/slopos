//! Input event types (Wayland-style per-task input queues)

pub mod keycode;
pub mod layout;

pub const MAX_INPUT_TASKS: usize = 32;

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
/// Num Lock active. Carried in the key-event modifier snapshot so a consumer can
/// resolve keypad keys (digit vs navigation) without tracking lock state itself.
pub const MODIFIER_NUM_LOCK: u8 = 1 << 5;
/// Scroll Lock active.
pub const MODIFIER_SCROLL_LOCK: u8 = 1 << 6;
/// AltGr (right Alt) held; `MODIFIER_ALT` is set too. Distinguishes
/// AltGr-as-text (`AltGr+2 = @`) from a left-Alt shortcut chord.
pub const MODIFIER_ALTGR: u8 = 1 << 7;

/// Key-event flag bits live in `data0[8:16]`. This one marks an event carrying
/// the canonical (HID keycode + text codepoint) payload beside the legacy bytes.
pub const KEY_FLAG_HAS_CANONICAL: u8 = 1 << 0;
/// This press is an auto-repeat, not a fresh physical key-down.
pub const KEY_FLAG_IS_REPEAT: u8 = 1 << 1;
/// The key came from the numeric keypad block.
pub const KEY_FLAG_FROM_KEYPAD: u8 = 1 << 2;

/// Axis identifiers for PointerAxis events (Wayland wl_pointer.axis convention)
pub const POINTER_AXIS_VERTICAL: u32 = 0;
pub const POINTER_AXIS_HORIZONTAL: u32 = 1;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputEventType {
    #[default]
    KeyPress = 0,
    KeyRelease = 1,
    PointerMotion = 2,
    PointerButtonPress = 3,
    PointerButtonRelease = 4,
    PointerEnter = 5,
    PointerLeave = 6,
    CloseRequest = 7,
    /// Compositor notifies the app of a new configured size (resize).
    Configure = 8,
    /// Pointer axis (scroll wheel) event.
    PointerAxis = 9,
}

impl InputEventType {
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

    #[inline]
    pub fn is_key_event(self) -> bool {
        matches!(self, Self::KeyPress | Self::KeyRelease)
    }

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
/// For pointer motion: data0 is x coordinate, data1 is y coordinate
/// For pointer button: data0 contains button code
/// For close request: data0/data1 are zero
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEventData {
    pub data0: u32,
    pub data1: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub event_type: InputEventType,
    pub _padding: [u8; 3],
    /// Timestamp in milliseconds since boot.
    pub timestamp_ms: u64,
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
    /// Key event carrying only `(scancode, ascii)`; the canonical fields are
    /// zero-filled. Prefer [`key_full`](Self::key_full).
    pub fn key(event_type: InputEventType, scancode: u8, ascii: u8, timestamp_ms: u64) -> Self {
        Self::key_full(event_type, scancode, ascii, 0, 0, 0, 0, timestamp_ms)
    }

    /// `keycode` is a canonical HID Keyboard/Keypad usage (see
    /// [`keycode`](self::keycode)); `codepoint` is the produced Unicode scalar
    /// (BMP only — the high 16 bits are truncated).
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

    pub fn close_request(timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::CloseRequest,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData { data0: 0, data1: 0 },
        }
    }

    /// Configure event — a window-resize notification.
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

    /// `axis` is `POINTER_AXIS_VERTICAL` or `POINTER_AXIS_HORIZONTAL`;
    /// `value_v120` is in value120 units (±120 = one wheel click, + is down/right).
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

    /// Axis identifier of a PointerAxis event (0 = vertical, 1 = horizontal).
    #[inline]
    pub fn axis_id(&self) -> u32 {
        self.data.data0
    }

    /// Scroll value in value120 units; ±120 = one physical wheel click.
    #[inline]
    pub fn axis_value_v120(&self) -> i32 {
        self.data.data1 as i32
    }

    #[inline]
    pub fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

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

    /// The produced text codepoint, falling back to the legacy `ascii` byte when
    /// the canonical codepoint field is empty.
    #[inline]
    pub fn key_codepoint(&self) -> u32 {
        let cp = (self.data.data1 >> 16) & 0xFFFF;
        if cp != 0 { cp } else { self.key_ascii() as u32 }
    }

    #[inline]
    pub fn key_has_canonical(&self) -> bool {
        self.key_flags() & KEY_FLAG_HAS_CANONICAL != 0
    }

    #[inline]
    pub fn key_is_repeat(&self) -> bool {
        self.key_flags() & KEY_FLAG_IS_REPEAT != 0
    }

    #[inline]
    pub fn key_from_keypad(&self) -> bool {
        self.key_flags() & KEY_FLAG_FROM_KEYPAD != 0
    }

    #[inline]
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    #[inline]
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    #[inline]
    pub fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    #[inline]
    pub fn configure_width(&self) -> u32 {
        self.data.data0
    }

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
        let e = InputEvent::key(InputEventType::KeyPress, 0x1E, b'a', 42);
        assert_eq!(e.key_scancode(), 0x1E);
        assert_eq!(e.key_ascii(), b'a');
        assert_eq!(e.data.data0, 0x1E | ((b'a' as u32) << 16));
        assert_eq!(e.data.data1, 0);
        assert_eq!(e.key_flags(), 0);
        assert_eq!(e.key_modifiers(), 0);
        assert_eq!(e.key_keycode(), 0);
        assert!(!e.key_has_canonical());
        assert!(!e.key_is_repeat());
        assert!(!e.key_from_keypad());
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
