use super::traits::WidgetId;

/// Pointer button identifiers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// Modifier key state.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// AltGr (right Alt) — distinct from `alt`. Set together with `alt`.
    pub altgr: bool,
    pub super_key: bool,
    pub caps_lock: bool,
}

impl Modifiers {
    pub fn from_raw(raw: u8) -> Self {
        Self {
            shift: raw & slopos_abi::input::MODIFIER_SHIFT != 0,
            ctrl: raw & slopos_abi::input::MODIFIER_CTRL != 0,
            alt: raw & slopos_abi::input::MODIFIER_ALT != 0,
            altgr: raw & slopos_abi::input::MODIFIER_ALTGR != 0,
            super_key: raw & slopos_abi::input::MODIFIER_SUPER != 0,
            caps_lock: raw & slopos_abi::input::MODIFIER_CAPS_LOCK != 0,
        }
    }

    /// A "plain Alt" chord (left Alt only) — a shortcut modifier, as opposed to
    /// AltGr which composes layout text.
    pub fn plain_alt(&self) -> bool {
        self.alt && !self.altgr
    }
}

/// Named (non-character) keys.
///
/// Re-exported from the canonical ABI keycode vocabulary (shared with the
/// kernel via `keymap-core`) so the named-key set is defined in one place.
pub use slopos_keymap_core::keycode::NamedKey;

/// Key produced by the keymap.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Named(NamedKey),
    Unknown,
}

/// Widget events dispatched through the tree.
#[derive(Clone, Debug)]
pub enum WidgetEvent {
    // Pointer events
    PointerDown {
        x: i32,
        y: i32,
        button: PointerButton,
    },
    PointerUp {
        x: i32,
        y: i32,
        button: PointerButton,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerEnter,
    PointerLeave,
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },

    // Keyboard events
    KeyDown {
        key: Key,
        modifiers: Modifiers,
        repeat: bool,
    },
    KeyUp {
        key: Key,
        modifiers: Modifiers,
    },
    TextInput {
        character: char,
    },

    // Focus events
    FocusGained,
    FocusLost,

    // Lifecycle
    Configure {
        width: u32,
        height: u32,
    },
}

/// Phase of event dispatch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EventPhase {
    /// Root -> target (preview / intercept).
    Tunnel,
    /// The direct target widget.
    Target,
    /// Target -> root (bubbling).
    Bubble,
}

/// Response from an event handler.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EventResponse {
    /// Event not consumed, continue propagation.
    Ignored,
    /// Event consumed, stop propagation.
    Consumed,
    /// Capture all pointer events to this widget until release.
    CapturePointer,
    /// Release a previous pointer capture.
    ReleasePointer,
}

impl EventResponse {
    pub fn is_consumed(&self) -> bool {
        !matches!(self, EventResponse::Ignored)
    }
}

/// Result of hit testing: which widget is under the pointer.
pub struct HitTestResult {
    /// The deepest widget containing the point.
    pub target: WidgetId,
    /// Ancestor chain from target to root (target first, root last).
    pub chain: Vec<WidgetId>,
}

/// Perform hit testing on a widget tree.
/// Walks in reverse paint order (last child first = topmost tested first).
/// Returns the deepest widget containing the point.
pub fn hit_test(
    root: &dyn super::traits::Widget,
    point_x: i32,
    point_y: i32,
) -> Option<HitTestResult> {
    let mut chain = Vec::new();
    if hit_test_recursive(root, point_x, point_y, &mut chain) {
        chain.reverse();
        let target = chain[0];
        Some(HitTestResult { target, chain })
    } else {
        None
    }
}

fn hit_test_recursive(
    widget: &dyn super::traits::Widget,
    px: i32,
    py: i32,
    chain: &mut Vec<WidgetId>,
) -> bool {
    let rect = widget.layout_rect();
    if !rect.contains(px, py) {
        return false;
    }

    // All widgets use absolute coordinates from layout, so pass
    // the original point through without local conversion.
    let children = widget.children();
    for child in children.iter().rev() {
        if hit_test_recursive(child.as_ref(), px, py, chain) {
            chain.push(widget.id());
            return true;
        }
    }

    // No child hit — this widget is the target.
    chain.push(widget.id());
    true
}

/// Message queue that widgets push to during event handling.
/// Uses type erasure (`Box<dyn Any>`) so that the object-safe `Widget` trait
/// can emit messages without knowing the concrete message type.
/// `run_app()` drains messages with `drain_typed::<M>()`.
pub struct MessageSink {
    messages: Vec<Box<dyn std::any::Any>>,
}

impl MessageSink {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Emit a type-erased message from a widget.
    pub fn emit_raw(&mut self, msg: Box<dyn std::any::Any>) {
        self.messages.push(msg);
    }

    /// Drain all pending messages that match type `M`, leaving others in place.
    pub fn drain_typed<M: 'static>(&mut self) -> Vec<M> {
        let mut typed = Vec::new();
        let mut remaining = Vec::new();
        for msg in self.messages.drain(..) {
            match msg.downcast::<M>() {
                Ok(m) => typed.push(*m),
                Err(other) => remaining.push(other),
            }
        }
        self.messages = remaining;
        typed
    }

    /// Whether any messages are pending.
    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }
}

impl Default for MessageSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch event from root. Containers forward to their children
/// in their own event() implementations. The hit test result is used
/// by the framework for focus management, not event routing.
pub fn dispatch_event(
    root: &mut dyn super::traits::Widget,
    _hit: &HitTestResult,
    event: &WidgetEvent,
    sink: &mut MessageSink,
) -> EventResponse {
    root.event(event, EventPhase::Target, sink)
}
