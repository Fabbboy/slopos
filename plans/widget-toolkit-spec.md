# SlopOS Widget Toolkit — Technical Specification

> Supersedes the Phase 6 section of `desktop-ui.md`.
> This document is the authoritative reference for implementing the widget
> toolkit. It specifies architecture, algorithms, data structures, and
> API contracts.

## 0. Design Goals

1. **Declarative retained-mode widget tree.** Apps describe their UI as a tree of
   typed widget nodes. The framework owns layout, painting, hit testing, focus,
   and input routing. Apps respond to typed events/messages.
2. **Two-pass constraint layout.** Constraints flow down the tree; sizes flow up.
   Positions are assigned in a second top-down pass. Identical to the model used
   by every major retained-mode toolkit.
3. **Keyboard-first.** Every interactive widget is reachable and operable via
   keyboard. Focus management, tab order, and focus scopes are first-class
   concepts, not afterthoughts. This makes accessibility structurally easy
   to add later.
4. **CPU-only rendering.** All painting uses the existing `DrawBuffer` + gfx
   primitives + TTF font renderer. No GPU, no display lists, no shaders.
5. **No allocations in the hot path.** The per-frame repaint path must not call
   `alloc`. Widget tree mutations (add/remove children) may allocate.
6. **Incremental adoption.** Apps can mix raw `DrawBuffer` painting with widget
   tree rendering in the same frame. Existing apps (compositor, shell) are not
   forced to convert.

---

## 1. Architecture Overview

```
Application
  |  implements ViewBuilder: fn build(&self) -> Node
  |  implements on_event(&mut self, WidgetEvent) -> Action
  v
+------------------------------------------------------------------+
|                        WIDGET FRAMEWORK                           |
|                                                                   |
|  Build Phase         build() -> Node tree (declarative snapshot)  |
|  Diff/Reconcile      compare old tree vs new tree, patch retained |
|  Measure Phase       constraints down, sizes up                   |
|  Layout Phase        positions down (assign rects)                |
|  Event Phase         hit-test -> tunnel -> target -> bubble       |
|  Paint Phase         walk tree, paint dirty nodes into DrawBuffer |
|  Focus Manager       tab order, focus scopes, focus-visible       |
|  Text Input          keymap -> key repeat -> character generation |
|  Overlay Manager     popup/menu z-ordering above main tree        |
+------------------------------------------------------------------+
  |
  v
AppKit (Window, Surface, Event, run())
  |
  v
Kernel (syscalls, SHM, input, compositor)
```

### Module Layout

```
userland/src/ui/
+-- mod.rs              Public API re-exports
+-- node.rs             Node enum (widget tree description)
+-- tree.rs             Retained tree, reconciliation/diff engine
+-- traits.rs           Widget trait (measure, layout, paint, event)
+-- constraints.rs      BoxConstraints, SizePolicy, Alignment
+-- layout.rs           VStack, HStack, ZStack, Padding, Spacer
+-- event.rs            WidgetEvent, EventResponse, hit testing
+-- focus.rs            FocusManager, FocusScope, tab chain
+-- input.rs            Keymap, key repeat, character generation
+-- overlay.rs          Overlay layer for popups/menus
+-- paint.rs            PaintContext (wraps DrawBuffer + clip + scroll offset)
+-- style.rs            StyleSheet (colors, fonts, spacing, corner radii)
+-- dirty.rs            Dirty flag propagation, repaint scheduling
+-- widgets/
    +-- mod.rs
    +-- label.rs        Static text (single/multi-line, alignment)
    +-- button.rs       Clickable button with hover/press/disabled states
    +-- text_field.rs   Single-line text input with cursor + selection
    +-- checkbox.rs     Toggle with label
    +-- separator.rs    Horizontal/vertical divider line
    +-- scroll_view.rs  Scrollable container with scrollbar
    +-- list_view.rs    Virtualized list with item recycling
    +-- tab_bar.rs      Tab header + panel switching
    +-- menu.rs         Context menu + dropdown menu
    +-- image.rs        Pixel buffer display (icon, thumbnail)
```

---

## 2. The Widget Trait

Every widget implements a common trait that the framework calls during
each phase of the frame cycle.

```rust
pub trait Widget {
    /// Compute this widget's desired size given parent constraints.
    ///
    /// Called during the measure phase (bottom-up). The widget measures
    /// its children first (if any), then reports its own size. The
    /// returned size MUST satisfy the constraints (clamped if necessary).
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size;

    /// Assign final position and size. Called top-down after measure.
    ///
    /// `rect` is in parent-local coordinates. The widget stores its
    /// layout rect and recursively lays out children.
    fn layout(&mut self, rect: Rect);

    /// Paint this widget into the paint context.
    ///
    /// The PaintContext provides a DrawBuffer scoped to this widget's
    /// clip rect plus helper methods (fill_rect, draw_text, etc.).
    /// Paint order: background -> content -> children -> foreground.
    fn paint(&self, ctx: &mut PaintContext);

    /// Handle an input event. Return whether the event was consumed.
    ///
    /// Events arrive after hit-testing. The framework calls this in
    /// three phases: tunnel (root->target), target, bubble (target->root).
    fn event(&mut self, event: &WidgetEvent, phase: EventPhase) -> EventResponse;

    /// Return this widget's accessibility role (for future a11y tree).
    fn role(&self) -> Role { Role::None }

    /// Return this widget's accessible name (for future a11y tree).
    fn accessible_name(&self) -> Option<&str> { None }

    /// Focus policy: can this widget receive keyboard focus?
    fn focus_policy(&self) -> FocusPolicy { FocusPolicy::None }

    /// The unique ID of this widget instance (assigned by the framework).
    fn id(&self) -> WidgetId;
}
```

### Supporting Types

```rust
/// Unique identifier for a widget instance in the retained tree.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct WidgetId(u32);

/// Pixel dimensions.
#[derive(Copy, Clone)]
pub struct Size { pub width: f32, pub height: f32 }

/// Position + size in parent-local coordinates.
#[derive(Copy, Clone)]
pub struct Rect { pub x: f32, pub y: f32, pub width: f32, pub height: f32 }

/// Accessibility role (minimal set for v1).
#[derive(Copy, Clone, Eq, PartialEq)]
pub enum Role {
    None,           // Decorative / layout-only, pruned from a11y tree
    Button,
    TextField,
    Checkbox,
    Label,
    List,
    ListItem,
    ScrollArea,
    Tab,
    TabPanel,
    Menu,
    MenuItem,
    Separator,
    Group,
    Window,
}

pub enum FocusPolicy {
    None,           // Never focusable
    TabFocus,       // Focusable via Tab key
    ClickFocus,     // Focusable via mouse click only
    StrongFocus,    // Focusable via both Tab and click
}
```

---

## 3. Constraint-Based Layout

### 3.1 BoxConstraints

The universal currency of layout negotiation. Passed top-down during
the measure phase.

```rust
#[derive(Copy, Clone)]
pub struct BoxConstraints {
    pub min_width: f32,
    pub max_width: f32,
    pub min_height: f32,
    pub max_height: f32,
}
```

**Invariants:**
- `0 <= min_width <= max_width`
- `0 <= min_height <= max_height`
- `max_width` and `max_height` may be `f32::INFINITY` (unbounded)

**Constraint modes:**
- **Tight:** `min == max`. The child must be exactly this size. Used by
  fixed-size containers and the root (window size).
- **Loose:** `min < max`. The child chooses within the range.
- **Unbounded:** `max == INFINITY`. The child can be any size. Used inside
  scroll views on the scroll axis.

### 3.2 Size Policies

Each widget (or slot in a layout container) declares how it wants to
participate in space distribution.

```rust
#[derive(Copy, Clone)]
pub enum SizePolicy {
    /// Use exactly the intrinsic content size. Do not grow or shrink.
    Fixed,
    /// Use intrinsic content size as preferred, but may shrink to min.
    Shrink,
    /// Expand to fill available space. Weight controls proportional share.
    Expand { weight: u16 },
}
```

### 3.3 Stack Layout Algorithm (VStack / HStack)

VStack arranges children vertically; HStack horizontally. The algorithm
is identical, parameterized by main axis (vertical or horizontal) and
cross axis.

**Measure phase (called with parent `BoxConstraints`):**

1. **Classify children** by size policy: `Fixed`, `Shrink`, or `Expand`.

2. **Measure fixed and shrink children first.** Pass each child loose
   constraints on the main axis (`min=0, max=remaining_space`) and the
   parent's cross-axis constraints unchanged. Sum their main-axis sizes.
   Track `remaining_space = max_main - sum_fixed_shrink`.

3. **Distribute remaining space to expand children.** Compute
   `total_weight = sum(child.weight for expand children)`. Each expand
   child receives `share = remaining_space * (child.weight / total_weight)`.
   Measure each expand child with tight main-axis constraint
   (`min=share, max=share`) and parent's cross-axis constraints.

4. **Compute total size.** Main axis = sum of all children + spacing
   between children. Cross axis = max of all children's cross sizes
   (or parent's cross constraint if tighter).

5. **Clamp** to parent constraints and return.

**Layout phase:**

1. Walk children in order. Assign each child's main-axis position as
   `cursor + alignment_offset`. Advance cursor by `child_main_size + spacing`.

2. Cross-axis position depends on `CrossAxisAlignment`:
   - `Start`: `x = 0` (or `y = 0`)
   - `Center`: `x = (available - child_cross) / 2`
   - `End`: `x = available - child_cross`
   - `Stretch`: child gets `cross_size = available` (re-measure with tight)

### 3.4 ZStack (Overlay Layout)

All children occupy the same rect. Each child is measured with the
parent's constraints. Children are laid out in tree order; last child
paints on top. Used for overlapping content (e.g., a badge on an icon).

### 3.5 Padding, Spacer, Alignment Containers

- **Padding(top, right, bottom, left):** Wraps a single child. Subtracts
  padding from constraints before measuring child, adds padding back to
  reported size.
- **Spacer(size):** Fixed-size empty widget. In a VStack, a `Spacer(8.0)`
  adds 8px of vertical space.
- **Align(alignment, child):** Positions child within available space
  according to alignment (top-left, center, bottom-right, etc.) without
  constraining child size beyond parent constraints.

---

## 4. Event Dispatch

### 4.1 Event Types

```rust
pub enum WidgetEvent {
    // --- Pointer events ---
    PointerDown { x: f32, y: f32, button: PointerButton },
    PointerUp { x: f32, y: f32, button: PointerButton },
    PointerMove { x: f32, y: f32 },
    PointerEnter,
    PointerLeave,
    Scroll { delta_x: f32, delta_y: f32 },

    // --- Keyboard events ---
    KeyDown { key: Key, modifiers: Modifiers, repeat: bool },
    KeyUp { key: Key, modifiers: Modifiers },
    TextInput { character: char },

    // --- Focus events ---
    FocusGained,
    FocusLost,

    // --- Lifecycle ---
    Configure { width: u32, height: u32 },
}

pub enum EventPhase {
    Tunnel,     // Root -> target (preview)
    Target,     // The widget that was hit-tested
    Bubble,     // Target -> root
}

pub enum EventResponse {
    Ignored,    // Event not consumed, continue propagation
    Consumed,   // Event consumed, stop propagation
}
```

### 4.2 Hit Testing

On every pointer event, the framework determines which widget is under
the pointer:

1. Walk the tree in **reverse paint order** (last-painted = topmost = tested first).
2. For each widget, check `rect.contains(point)`.
3. Recurse into children (also in reverse order). The deepest matching widget wins.
4. Overlay layer is tested before the main tree (popups intercept first).

Result: a **target widget** and an **ancestor chain** (target -> root).

### 4.3 Three-Phase Dispatch

For each pointer or keyboard event:

1. **Tunnel phase (root -> target):** Walk the ancestor chain from root
   toward target. Call `widget.event(event, EventPhase::Tunnel)` at each
   node. If any handler returns `Consumed`, stop.
2. **Target phase:** Call `target.event(event, EventPhase::Target)`.
3. **Bubble phase (target -> root):** Walk the ancestor chain from target
   back toward root. Call `widget.event(event, EventPhase::Bubble)` at
   each node. If any handler returns `Consumed`, stop.

**Keyboard events** go to the focused widget (not hit-tested). The
ancestor chain is the focus target's path to root.

### 4.4 Hover Tracking

The framework tracks which widget the pointer is currently over
(`hover_widget: Option<WidgetId>`). When it changes:

1. Send `PointerLeave` to the old hover widget (bubble up through ancestors).
2. Send `PointerEnter` to the new hover widget (tunnel down through ancestors).

This enables hover states (button color change on mouse-over) without
each widget implementing its own tracking.

### 4.5 Pointer Capture

During drag interactions, a widget may **capture** the pointer:

```rust
pub enum EventResponse {
    Ignored,
    Consumed,
    CapturePointer,     // This widget receives all pointer events until release
    ReleasePointer,     // Release a previous capture
}
```

While captured:
- All `PointerMove` and `PointerUp` events go directly to the capturing
  widget, bypassing hit testing.
- `PointerLeave` is not sent (the widget logically "holds" the pointer).
- Capture is automatically released on `PointerUp` (or explicitly via
  `ReleasePointer`).

Used by: scroll thumb dragging, text selection, slider dragging.

---

## 5. Focus Management

### 5.1 Focus Chain

The framework maintains a linear list of focusable widgets (those with
`FocusPolicy::TabFocus` or `StrongFocus`), ordered by depth-first tree
traversal. This is the **tab chain**.

- **Tab:** Move to next widget in the chain.
- **Shift+Tab:** Move to previous widget in the chain.
- Wraps at both ends (last -> first, first -> last).

The currently focused widget receives all keyboard events.

### 5.2 Focus Scopes

A **focus scope** traps Tab navigation within a subtree. When focus
enters a scope, Tab cycles within it and cannot leave. Focus scopes
are used for:

- Modal dialogs (must-interact-before-dismiss)
- Dropdown menus (arrow keys navigate items, Tab is trapped)

```rust
pub struct FocusScope {
    /// The widgets in this scope, in tab order.
    chain: Vec<WidgetId>,
    /// Which widget was focused when this scope was entered.
    restore_to: Option<WidgetId>,
}
```

The focus manager maintains a **scope stack**:
- Bottom: the global scope (all focusable widgets in the window).
- Pushing a scope: saves current focus, moves focus into the new scope.
- Popping a scope: restores focus to `restore_to`.

### 5.3 Focus-Visible Heuristic

Focus rings are only rendered when the user is navigating with the
keyboard. The framework tracks the last input modality:

- Any `KeyDown` (non-modifier) or Tab sets `keyboard_active = true`.
- Any `PointerDown` sets `keyboard_active = false`.

Widgets query `ctx.is_focus_visible()` during paint to decide whether
to draw their focus ring. Text fields always show focus indicators
regardless of input modality.

### 5.4 Focus Ring Rendering

The framework provides a helper in `PaintContext`:

```rust
impl PaintContext<'_> {
    /// Draw a standard focus ring around the given rect.
    /// Uses a 2px outline offset 1px outside the rect.
    /// Color: theme accent color with guaranteed contrast.
    pub fn draw_focus_ring(&mut self, rect: Rect);
}
```

Widgets call this in their `paint()` method when they are focused and
focus is visible.

---

## 6. Text Input Pipeline

### 6.1 Keymap

The keymap translates `(scancode, modifiers) -> Key`. Initially only
US-QWERTY is supported, but the architecture must support swappable
keymap tables.

```rust
pub struct Keymap {
    /// Base layer: scancode -> Key (no modifiers)
    base: [Key; 128],
    /// Shift layer: scancode -> Key (Shift held)
    shift: [Key; 128],
    /// AltGr layer: scancode -> Key (AltGr held, for European layouts)
    altgr: [Key; 128],
}

pub enum Key {
    /// Printable character
    Char(char),
    /// Named key (non-character)
    Named(NamedKey),
    /// Dead key (starts composition)
    Dead(DeadKey),
    /// Unknown / unmapped scancode
    Unknown,
}

pub enum NamedKey {
    Backspace, Delete, Tab, Enter, Escape,
    Left, Right, Up, Down,
    Home, End, PageUp, PageDown,
    // Modifiers (not dispatched as events, tracked as state)
    Shift, Ctrl, Alt, Super,
    CapsLock,
}

pub enum DeadKey {
    Acute,      // ' -> e = e
    Grave,      // ` -> e = e
    Circumflex, // ^ -> o = o
    Tilde,      // ~ -> n = n
    Diaeresis,  // " -> u = u
}
```

**Character generation flow:**

```
Raw InputEvent (scancode + modifiers)
  -> Keymap lookup: (scancode, modifier_state) -> Key
  -> Dead key state machine:
       if Key::Dead(d): save dead key, wait for next
       if pending dead + Key::Char(c): lookup compose table -> composed char
       if Key::Char(c) with no pending dead: emit TextInput { character: c }
  -> WidgetEvent::TextInput or WidgetEvent::KeyDown (for named keys)
```

### 6.2 Dead Key Compose Table

A simple lookup table for `(DeadKey, char) -> char`:

```
(Acute, 'a') -> 'a'    (Acute, 'e') -> 'e'    (Acute, 'i') -> 'i'
(Acute, 'o') -> 'o'    (Acute, 'u') -> 'u'
(Grave, 'a') -> 'a'    (Grave, 'e') -> 'e'    (Grave, 'u') -> 'u'
(Circumflex, 'a') -> 'a' (Circumflex, 'e') -> 'e' (Circumflex, 'o') -> 'o'
(Tilde, 'n') -> 'n'    (Tilde, 'a') -> 'a'    (Tilde, 'o') -> 'o'
(Diaeresis, 'a') -> 'a' (Diaeresis, 'o') -> 'o' (Diaeresis, 'u') -> 'u'
```

If no match: emit the dead key's accent character followed by the
base character (e.g., Dead Acute + 'x' -> '\'' + 'x').

### 6.3 Key Repeat

Implemented in the framework (not the kernel), per-widget:

```rust
pub struct KeyRepeatState {
    /// The key currently being held.
    key: Option<Key>,
    /// Timestamp when key was first pressed.
    press_time_ms: u64,
    /// Timestamp of last repeat emission.
    last_repeat_ms: u64,
    /// Has the initial delay elapsed?
    repeating: bool,
}

const REPEAT_DELAY_MS: u64 = 500;
const REPEAT_INTERVAL_MS: u64 = 33;  // ~30 repeats/sec
```

**Algorithm:**
1. On `KeyDown`: Store key + current time. Clear `repeating`.
2. Each frame tick: If key held and `now - press_time > REPEAT_DELAY`
   and not yet repeating, set `repeating = true`, emit repeat event.
3. If repeating and `now - last_repeat > REPEAT_INTERVAL`, emit repeat
   event, update `last_repeat`.
4. On `KeyUp`: Clear state.

Repeat events are dispatched as `KeyDown { repeat: true }` and/or
`TextInput` (for character keys).

**Keys that repeat:** Character keys, Backspace, Delete, arrow keys,
Tab, Space. **Keys that do NOT repeat:** Modifier keys (Shift, Ctrl,
Alt, Super), Escape, Enter, CapsLock.

---

## 7. Widget Specifications

### 7.1 Label

Static text display. Not focusable.

**Properties:**
- `text: &str`
- `font_size: f32` (default: 14.0)
- `color: Color32` (default: theme foreground)
- `alignment: TextAlignment` (Start, Center, End)
- `wrap: bool` (default: false; if true, wraps at container width)
- `max_lines: Option<u32>` (truncate with ellipsis)

**Measure:** If `wrap == false`, intrinsic size is
`(measure_text_width(text, font_size), line_height)`.
If `wrap == true`, width = constraint max width, height = number of
wrapped lines * line_height.

**Paint:** Draw text at layout position using TTF renderer with clip.

**Role:** `Role::Label`

### 7.2 Button

Clickable element with text label and visual states.

**Properties:**
- `label: &str`
- `enabled: bool` (default: true)
- `style: ButtonStyle` (Primary, Secondary, Destructive)

**States:** `{ idle, hovered, pressed, disabled }`

State transitions:
- `idle` -> `hovered` on `PointerEnter`
- `hovered` -> `pressed` on `PointerDown`
- `pressed` -> `hovered` on `PointerUp` (emits click event)
- `pressed` -> `idle` on `PointerLeave` (cancel)
- Any -> `disabled` when `enabled = false`

**Measure:** `text_width + padding_h * 2` x `line_height + padding_v * 2`.
Minimum 64px wide.

**Paint:** Rounded rect background (color by state + style), text centered.
Draw focus ring when focused and focus-visible.

**Keyboard:** Enter or Space triggers click.

**Role:** `Role::Button`, name = label text.

**Focus:** `StrongFocus`

### 7.3 TextField

Single-line text input with cursor and selection.

**Properties:**
- `text: String` (mutable, owned by the widget)
- `placeholder: &str`
- `max_length: Option<usize>`
- `read_only: bool`

**Internal state:**
```rust
struct TextFieldState {
    cursor: usize,          // Character index (not byte)
    selection_anchor: Option<usize>,  // Start of selection, if any
    scroll_offset: f32,     // Horizontal scroll when text exceeds width
    blink_timer: u64,       // Cursor blink (500ms on, 500ms off)
}
```

**Text buffer:** Simple `String`. Character-indexed via `.chars()`.
This is adequate for single-line fields. Multi-line editors would
use a gap buffer or rope, but that is out of scope.

**Cursor positioning:**
- Click: measure text widths to find character index closest to x.
- Arrow Left/Right: move cursor by 1 character. With Shift: extend selection.
- Home/End: move to start/end of text. With Shift: extend selection.
- Ctrl+Left/Right: move by word boundary (scan for whitespace/punctuation).

**Selection model:**
- Selection = range `[anchor, cursor)` where anchor is where the drag started.
- Shift+click: extend selection from anchor to click position.
- Ctrl+A: select all.
- Selected text is highlighted (inverted or accent background).

**Text editing:**
- `TextInput { character }`: Insert at cursor (or replace selection).
- Backspace: Delete character before cursor (or delete selection).
- Delete: Delete character after cursor (or delete selection).
- Ctrl+C: Copy selection to clipboard (via compositor clipboard syscall).
- Ctrl+V: Paste from clipboard at cursor.
- Ctrl+X: Cut selection to clipboard.

**Horizontal scrolling:** When text is wider than the widget, the visible
portion scrolls to keep the cursor visible. `scroll_offset` is adjusted
so that the cursor is always within the visible rect, with a small margin.

**Measure:** Width = constraint max width (fills available). Height =
line_height + padding * 2. Minimum width 80px.

**Paint:** Background rect, border (thicker when focused), text with
scroll offset, cursor (blinking vertical bar), selection highlight
(filled rect behind selected text), placeholder text (when empty and
unfocused, in dimmed color).

**Role:** `Role::TextField`, name = placeholder or label.

**Focus:** `StrongFocus`. Always shows focus indicator (border change +
blinking cursor).

### 7.4 Checkbox

Toggle with label.

**Properties:**
- `checked: bool`
- `label: &str`
- `enabled: bool`

**Measure:** `check_box_size + gap + text_width` x
`max(check_box_size, line_height)`.

**Paint:** 16x16 rounded rect (checked: filled accent + checkmark glyph;
unchecked: border only). Label text to the right. Focus ring around the
check box (not the label).

**Keyboard:** Space toggles.

**Role:** `Role::Checkbox`, name = label text.

**Focus:** `StrongFocus`.

### 7.5 Separator

Visual divider. Not focusable.

**Properties:**
- `orientation: Orientation` (Horizontal, Vertical)

**Measure:** Horizontal: `(parent_width, 1.0)`.
Vertical: `(1.0, parent_height)`.

**Paint:** 1px line in theme divider color.

**Role:** `Role::Separator`

### 7.6 ScrollView

Scrollable container with a single child that may be larger than the
viewport.

**Properties:**
- `scroll_direction: ScrollDirection` (Vertical, Horizontal, Both)
- `show_scrollbar: ScrollbarVisibility` (Always, WhenNeeded, Never)

**Internal state:**
```rust
struct ScrollViewState {
    offset_x: f32,
    offset_y: f32,
    content_size: Size,       // Measured size of child
    viewport_size: Size,      // ScrollView's own size
    scrollbar_drag: Option<ScrollbarDrag>,
    thumb_hovered: bool,
}
```

**Measure:** The ScrollView measures its child with **unbounded**
constraints on the scroll axis and parent constraints on the cross axis.
The ScrollView's own size = parent constraints (it fills available space).

**Layout:** The child is positioned at `(-offset_x, -offset_y)` relative
to the ScrollView's origin. Painting is clipped to the viewport rect.

**Scrollbar rendering:**
- Track: full-height (or full-width) background strip, 8px wide.
- Thumb size: `viewport_size / content_size * track_length`, minimum 20px.
- Thumb position: `offset / max_offset * (track_length - thumb_size)`.
- `max_offset = content_size - viewport_size` (clamped >= 0).

**Scroll input:**
- Mouse wheel: Adjust offset by `delta * line_height` (typically 3 lines
  per scroll step, using the value120 model: `delta_lines = value / 120 * 3`).
- Scrollbar thumb drag: pointer capture, map drag delta to scroll offset.
- Scrollbar track click: page scroll (offset += viewport_size).

**Keyboard:** When focused, arrow keys scroll by one line, Page Up/Down
scroll by one page.

**Role:** `Role::ScrollArea`

**Focus:** `TabFocus` (so keyboard scrolling works).

### 7.7 ListView

Virtualized list with item recycling. Extends ScrollView with a data
model.

**Properties:**
- `item_count: usize`
- `item_height: f32` (fixed per list; variable-height is out of scope for v1)
- `selected_index: Option<usize>`
- `on_item_build: fn(index: usize) -> Node` (builds a widget for one item)

**Virtual scrolling algorithm:**

1. Compute visible range:
   ```
   first_visible = floor(scroll_offset / item_height)
   last_visible  = ceil((scroll_offset + viewport_height) / item_height)
   overscan = 2  // extra items above/below for smooth scrolling
   render_start = max(0, first_visible - overscan)
   render_end   = min(item_count, last_visible + overscan)
   ```

2. Only instantiate widgets for items in `[render_start, render_end)`.
   Total scroll content height = `item_count * item_height` (for
   scrollbar sizing).

3. **Item recycling:** Maintain a pool of item widgets. When an item
   scrolls out of view, its widget is returned to the pool. When a new
   item scrolls into view, a widget is taken from the pool and rebound
   to the new data index via `on_item_build`. This avoids allocation
   churn for large lists.

**Selection:**
- Click: select item.
- Arrow Up/Down: move selection (and scroll to keep selected item visible).
- Selected item is highlighted (accent background).

**Measure:** Width = parent constraint. Height = parent constraint
(fills available, scrolls internally).

**Role:** `Role::List`. Each item: `Role::ListItem`.

**Focus:** `StrongFocus`. Arrow keys navigate, Enter activates.

### 7.8 TabBar

Tab header with panel switching.

**Properties:**
- `tabs: &[&str]` (tab labels)
- `active_tab: usize`
- `on_tab_change: fn(usize)` (callback)

**Measure:** Width = parent constraint. Height = tab_height (e.g., 36px).

**Paint:** Horizontal row of tab buttons. Active tab: accent underline
(3px) + opaque background. Inactive tabs: transparent background, dimmed
text. Separator line below tabs.

**Keyboard:** Left/Right arrows switch tabs when TabBar is focused.

**Role:** `Role::Tab` for each tab. The associated content panel should
have `Role::TabPanel`.

**Focus:** `StrongFocus`. Internal navigation via arrow keys (roving
focus within the tab group).

### 7.9 Menu (Context Menu / Dropdown)

A popup list of actions that appears on right-click or button trigger.

**Properties:**
- `items: &[MenuItem]`

```rust
pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: Option<&'static str>,  // Display only (e.g., "Ctrl+C")
    pub enabled: bool,
    pub kind: MenuItemKind,
}

pub enum MenuItemKind {
    Action,             // Triggers an action on click
    Separator,          // Visual divider
    Submenu(&'static [MenuItem]),  // Nested menu
}
```

**Popup lifecycle:**
1. Trigger opens menu. Framework pushes a focus scope and adds the menu
   to the overlay layer.
2. Menu is positioned adjacent to the trigger (context menus at pointer
   position, dropdowns below the trigger button). Clamped to window bounds.
3. Arrow Up/Down navigates items. Enter activates. Escape dismisses.
4. Click outside the menu dismisses it.
5. On dismiss: pop focus scope (restores previous focus).

**Paint:** Rendered in the **overlay layer** (painted after the main
widget tree, on top of all other content). Background: opaque rounded
rect with subtle shadow. Items: text + optional shortcut label.
Hovered item: accent background.

**Hit testing:** Overlay layer is tested first. A click outside any
overlay dismisses the topmost overlay (the "light dismiss" pattern).

**Role:** `Role::Menu`, items: `Role::MenuItem`.

**Focus:** Menus create a focus scope. Arrow keys navigate, not Tab.

### 7.10 Image

Displays a pixel buffer (icon, thumbnail).

**Properties:**
- `pixels: &[u8]` (pixel data in display format)
- `width: u32, height: u32` (source dimensions)
- `scale: ImageScale` (Fit, Fill, None)

**Measure:** `None`: source size. `Fit`: scale to fit within constraints
preserving aspect ratio. `Fill`: stretch to fill constraints.

**Paint:** Blit pixel data to DrawBuffer at layout position, with clip.

**Role:** `Role::None` (decorative by default).

---

## 8. Painting Architecture

### 8.1 PaintContext

Wraps a `DrawBuffer` with clip rect, scroll offset, and theme access.

```rust
pub struct PaintContext<'a> {
    buffer: &'a mut DrawBuffer<'a>,
    /// Current clip rect in window coordinates.
    clip: Rect,
    /// Accumulated scroll offset from ancestor ScrollViews.
    scroll_offset: (f32, f32),
    /// Whether keyboard focus indicators should be rendered.
    focus_visible: bool,
    /// Theme reference for colors, fonts, spacing.
    style: &'a StyleSheet,
}
```

Widgets only paint within their clip rect. The framework sets up the
clip rect before calling each widget's `paint()`.

**Paint order within a widget:**
1. Background (fill, border)
2. Content (text, image)
3. Children (in tree order; later children paint on top)
4. Foreground (focus ring, selection overlay)

### 8.2 Dirty Tracking

Each widget has two dirty flags:

```rust
bitflags! {
    pub struct DirtyFlags: u8 {
        const NEEDS_MEASURE = 0b01;
        const NEEDS_PAINT   = 0b10;
    }
}
```

**Propagation rules:**
- `mark_needs_measure()`: Sets `NEEDS_MEASURE` on self and propagates
  up to ancestors (because a child's size change may affect parent layout).
- `mark_needs_paint()`: Sets `NEEDS_PAINT` on self only (paint is local).

**Per-frame processing:**
1. If any widget has `NEEDS_MEASURE`, re-run measure + layout from the
   highest dirty ancestor.
2. Walk tree, paint only widgets with `NEEDS_PAINT` (and their children
   if the widget's background changed and children overlap).
3. Clear all dirty flags.

**Optimization:** For the initial version, full-tree repaint on any
dirty flag is acceptable. SlopOS apps are small (dozens of widgets,
not thousands). Partial repaint is a future optimization.

### 8.3 Overlay Layer

The overlay manager maintains a z-ordered list of overlay entries:

```rust
pub struct OverlayManager {
    /// Active overlays, in z-order (last = topmost).
    overlays: Vec<OverlayEntry>,
}

struct OverlayEntry {
    /// The widget tree for this overlay (e.g., a Menu).
    root: Box<dyn Widget>,
    /// Position in window coordinates.
    position: (f32, f32),
    /// Whether clicking outside dismisses this overlay.
    light_dismiss: bool,
    /// Focus scope for this overlay.
    scope: FocusScopeId,
}
```

**Paint order:** Main tree first, then overlays in z-order. Overlays
are painted directly into the same `DrawBuffer` (no separate surface).

**Hit testing:** Overlays are tested first (reverse z-order). If a
pointer event hits no overlay and `light_dismiss` is true for the
topmost overlay, the overlay is dismissed. The event is then re-tested
against the main tree.

**Constraint:** Overlays cannot extend beyond the window boundary (no
multi-surface popups). Menus that would overflow are repositioned to
fit within the window (flip to above the trigger, or shift horizontally).

---

## 9. Style System

### 9.1 StyleSheet

A struct holding all visual parameters. Widgets reference `style.button_bg`,
`style.text_primary`, etc. instead of hardcoding colors.

```rust
pub struct StyleSheet {
    // --- Colors ---
    pub bg_primary: Color32,        // Window/panel background
    pub bg_secondary: Color32,      // Card/inset background
    pub bg_tertiary: Color32,       // Hover highlight
    pub bg_accent: Color32,         // Primary action, selection, focus
    pub bg_destructive: Color32,    // Destructive action (red)

    pub text_primary: Color32,      // Main text
    pub text_secondary: Color32,    // Placeholder, dimmed text
    pub text_on_accent: Color32,    // Text on accent background
    pub text_disabled: Color32,     // Disabled element text

    pub border_default: Color32,    // Default border
    pub border_focused: Color32,    // Focused element border
    pub border_divider: Color32,    // Separator lines

    pub shadow_color: Color32,      // Drop shadow (with alpha)
    pub focus_ring_color: Color32,  // Keyboard focus indicator

    // --- Sizing ---
    pub font_size: f32,             // Default text size (14.0)
    pub font_size_small: f32,       // Small text (12.0)
    pub font_size_heading: f32,     // Heading text (18.0)
    pub line_height: f32,           // Default line height (20.0)

    pub spacing_xs: f32,            // 4.0
    pub spacing_sm: f32,            // 8.0
    pub spacing_md: f32,            // 12.0
    pub spacing_lg: f32,            // 16.0
    pub spacing_xl: f32,            // 24.0

    pub corner_radius: f32,         // Default corner radius (6.0)
    pub border_width: f32,          // Default border width (1.0)
    pub focus_ring_width: f32,      // Focus ring width (2.0)
    pub focus_ring_offset: f32,     // Focus ring offset from border (1.0)

    pub button_padding_h: f32,      // Button horizontal padding (12.0)
    pub button_padding_v: f32,      // Button vertical padding (6.0)
    pub button_min_width: f32,      // Minimum button width (64.0)

    pub field_padding_h: f32,       // Text field horizontal padding (8.0)
    pub field_padding_v: f32,       // Text field vertical padding (6.0)
    pub field_min_width: f32,       // Minimum text field width (80.0)

    pub scrollbar_width: f32,       // Scrollbar track width (8.0)
    pub scrollbar_thumb_min: f32,   // Minimum scrollbar thumb size (20.0)

    pub tab_height: f32,            // Tab bar height (36.0)
    pub menu_item_height: f32,      // Menu item height (28.0)
    pub menu_min_width: f32,        // Minimum menu width (120.0)

    pub checkbox_size: f32,         // Checkbox box size (16.0)
    pub checkbox_gap: f32,          // Gap between box and label (8.0)
}
```

A default `StyleSheet` is derived from the existing `theme.rs` values.
The style sheet is passed by reference through `PaintContext` and
`MeasureCtx` so widgets never hardcode visual parameters.

---

## 10. Application Integration

### 10.1 App Trait

Applications implement a trait that the framework drives:

```rust
pub trait App {
    /// The message type for this application's events.
    type Message;

    /// Build the widget tree. Called when the tree needs to be
    /// (re)constructed. The returned Node is a declarative description;
    /// the framework diffs it against the previous tree and patches
    /// the retained widgets.
    fn view(&self) -> Node;

    /// Handle a widget event (button click, text change, selection, etc.).
    /// Return an Action indicating what happened.
    fn update(&mut self, msg: Self::Message) -> Action;
}

pub enum Action {
    /// Nothing changed; no rebuild needed.
    None,
    /// State changed; rebuild the widget tree on next frame.
    Rebuild,
    /// Exit the application.
    Exit,
}
```

### 10.2 Node (Declarative Tree Description)

`Node` is a lightweight enum describing a widget and its children.
Apps build a tree of Nodes in `view()`. The framework diffs this
against the previous tree to update the retained widget tree
(add/remove/update widgets).

```rust
pub enum Node {
    Label { text: &'static str, /* ... */ },
    Button { label: &'static str, on_click: MessageId, /* ... */ },
    TextField { text: String, on_change: MessageId, /* ... */ },
    Checkbox { checked: bool, label: &'static str, on_toggle: MessageId },
    Separator,
    ScrollView { child: Box<Node>, direction: ScrollDirection },
    ListView { item_count: usize, item_height: f32, builder: fn(usize) -> Node },
    TabBar { tabs: &'static [&'static str], active: usize, on_change: MessageId },
    Menu { items: &'static [MenuItem] },
    Image { pixels: &'static [u8], width: u32, height: u32 },

    // Layout containers
    VStack { children: Vec<Node>, spacing: f32, align: CrossAxisAlignment },
    HStack { children: Vec<Node>, spacing: f32, align: CrossAxisAlignment },
    ZStack { children: Vec<Node> },
    Padding { padding: EdgeInsets, child: Box<Node> },
    Spacer { size: f32 },
    Expand { weight: u16, child: Box<Node> },

    // Empty placeholder
    Empty,
}
```

### 10.3 The Event Loop (Framework-Owned)

The framework replaces `appkit::run()` with a new entry point:

```rust
pub fn run_app<A: App>(app: A, width: u32, height: u32) -> !
```

Per-frame loop:

```
1. Poll raw input events from kernel (InputEvent batch)
2. Feed through keymap -> key repeat -> character generation
3. Hit-test pointer events against widget tree (overlay first, then main)
4. Dispatch events through three-phase pipeline
5. Collect widget messages, call app.update(msg) for each
6. If any update returned Action::Rebuild, call app.view() and reconcile
7. If any widget is dirty, run measure + layout + paint
8. Present surface to compositor (damage region = window rect for v1)
9. Wait for frame callback (poll_frame_done) or next input event
```

Step 9 replaces the current `yield_now()` spin loop.

### 10.4 Incremental Adoption / Escape Hatch

For apps that need raw drawing (compositor, shell), the old
`WindowedApp` trait and `appkit::run()` remain unchanged. The new
`run_app()` and `App` trait are additive.

Apps using the widget framework can also embed raw drawing regions
via a `Canvas` widget:

```rust
Node::Canvas {
    width: u32,
    height: u32,
    painter: fn(&mut DrawBuffer),
}
```

This allows mixing widget-based UI with custom rendering (e.g., a
waveform display, a chart, a game viewport).

---

## 11. What Changes vs. Phase 6 Plan

| Phase 6 Plan                    | This Spec                                   |
|---------------------------------|---------------------------------------------|
| "Widget trait, WidgetTree"      | Full Widget trait with 7 methods + types     |
| "VStack, HStack, ZStack"       | Constraint layout algorithm specified         |
| "Hit-test -> event bubble"     | Three-phase dispatch (tunnel+target+bubble)   |
| No focus model                  | Full focus chain + scopes + focus-visible     |
| No text input model             | Keymap + dead keys + key repeat + selection   |
| No state management             | Message-based App trait (view/update pattern)  |
| No dirty tracking               | NEEDS_MEASURE / NEEDS_PAINT propagation       |
| No pointer capture              | CapturePointer / ReleasePointer               |
| No hover tracking               | PointerEnter / PointerLeave synthesis          |
| No popup architecture           | Overlay manager + light dismiss + focus scopes |
| No accessibility                | Role + accessible_name on every widget         |
| No style system                 | StyleSheet with all visual parameters          |
| No keymap                       | Keymap module (US-QWERTY, extensible)          |
| No virtual scroll detail        | Full algorithm + item recycling pool           |
| No app integration pattern      | App trait + Node tree + reconciliation         |
| Files: `userland/src/widgets/`  | Files: `userland/src/ui/` (renamed, broader)   |
| 2000-3000 lines                 | 3000-4500 lines (more complete)                |

---

## 12. Implementation Order

Build in this sequence; each phase is independently testable:

1. **Foundation** (~800 lines)
   - `constraints.rs`, `traits.rs`, `node.rs`, `style.rs`, `dirty.rs`
   - `layout.rs` (VStack, HStack, Padding, Spacer)
   - `paint.rs` (PaintContext wrapping DrawBuffer)
   - Label + Separator widgets
   - Minimal `run_app` loop (no input yet, just renders a static tree)
   - **Test:** Static layout renders correctly on boot.

2. **Input + Focus** (~600 lines)
   - `event.rs` (hit testing, three-phase dispatch)
   - `focus.rs` (focus chain, tab navigation, focus-visible)
   - `input.rs` (keymap, key repeat)
   - Button widget (hover, press, click, keyboard activation)
   - **Test:** Buttons respond to click and Enter. Tab cycles focus.

3. **Text + Scrolling** (~800 lines)
   - TextField widget (cursor, selection, clipboard, text editing)
   - ScrollView widget (scroll offset, scrollbar, wheel input)
   - Checkbox widget
   - **Test:** Type text, select, copy/paste. Scroll long content.

4. **Lists + Tabs** (~500 lines)
   - ListView widget (virtual scrolling, item recycling, selection)
   - TabBar widget (tab switching, roving focus)
   - **Test:** 500-item list scrolls without lag. Tabs switch content.

5. **Menus + Overlays** (~500 lines)
   - `overlay.rs` (overlay manager, light dismiss)
   - Menu widget (context menu, dropdown, keyboard navigation)
   - Focus scopes for menus
   - **Test:** Right-click shows context menu. Escape dismisses.

6. **Demo App** (~400 lines)
   - Enhanced file manager OR widget gallery app using all widgets.
   - **Test:** Full QA scenario from desktop-ui.md Phase 6.

---

## 13. QA Criteria

All QA criteria from `desktop-ui.md` Phase 6 remain in effect.
Additional criteria from this spec:

- **PASS: Focus ring** — Tab through all interactive widgets. Each shows a
  visible focus ring when focused via keyboard. Focus ring disappears on
  mouse click (except text fields).
- **PASS: Keyboard operability** — Every Button, TextField, Checkbox, TabBar,
  ListView, and Menu item is operable via keyboard alone (no mouse needed).
- **PASS: Key repeat** — Hold a character key in a TextField. After ~500ms
  delay, characters repeat at ~30/sec.
- **PASS: Text selection** — Click and drag in a TextField to select text.
  Shift+arrow extends selection. Ctrl+A selects all. Selection is visually
  highlighted.
- **PASS: Virtual scroll** — A ListView with 500+ items scrolls without
  visible lag or frame drops.
- **PASS: Menu dismiss** — Opening a context menu and clicking outside it
  dismisses the menu. Focus returns to the previously focused element.
- **PASS: Pointer capture** — Dragging a scrollbar thumb works even if the
  pointer moves outside the scrollbar track.
- **PASS: Existing tests** — `just test` passes. Existing apps (compositor,
  shell) are unaffected.

---

## 14. Non-Goals (v1)

- GPU-accelerated rendering
- Multi-line text editor (TextArea widget)
- Drag-and-drop between widgets
- Animations and transitions
- IME protocol (CJK input)
- Multiple keyboard layouts (only US-QWERTY in v1)
- Bidirectional text (RTL)
- Accessibility platform bridge (roles are defined but no screen reader protocol)
- Dynamic theming (style sheet is compile-time for v1)
- Multi-window widget trees (each window gets its own tree)
