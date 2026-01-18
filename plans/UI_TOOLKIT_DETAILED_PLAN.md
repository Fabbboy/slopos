# SlopOS UI Toolkit - Detailed Implementation Plan

> **Status**: Planning Phase  
> **Target**: Complete retained-mode widget system for SlopOS userland  
> **Inspiration**: Iced (Elm architecture), egui (immediate mode patterns), OrbTk (Redox reference), Slint (embedded focus)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Analysis](#2-current-state-analysis)
3. [Architecture Decision: Retained vs Immediate Mode](#3-architecture-decision)
4. [Core Design Principles](#4-core-design-principles)
5. [Module Structure](#5-module-structure)
6. [Phase 1: Core Widget System](#6-phase-1-core-widget-system)
7. [Phase 2: Layout Engine](#7-phase-2-layout-engine)
8. [Phase 3: Core Widgets](#8-phase-3-core-widgets)
9. [Phase 4: Theming System](#9-phase-4-theming-system)
10. [Phase 5: Shell Migration](#10-phase-5-shell-migration)
11. [Technical Specifications](#11-technical-specifications)
12. [File Structure](#12-file-structure)
13. [Implementation Order](#13-implementation-order)

---

## 1. Executive Summary

SlopOS needs a **retained-mode widget toolkit** for userland applications. The current shell and file manager use ad-hoc drawing code with manual state management. A proper toolkit will:

- Provide compile-time memory safety (leverage Rust's type system)
- Enable consistent UI across all applications
- Support efficient damage-based redraws (already have DamageTracker)
- Integrate with existing compositor and shared memory architecture

**Key architectural choice**: Retained mode with Elm-like update pattern, but simplified for `no_std` constraints.

---

## 2. Current State Analysis

### What We Have

| Component | Location | Purpose |
|-----------|----------|---------|
| `DrawBuffer` | `userland/src/gfx/mod.rs` | Safe pixel buffer abstraction |
| `DrawTarget` trait | `abi/src/draw.rs` | Generic drawing interface |
| `DamageTracker` | `abi/src/damage.rs` | Efficient redraw regions |
| `draw_primitives` | `abi/src/draw_primitives.rs` | Bresenham, fill_rect, circles |
| `font_render` | `abi/src/font_render.rs` | Text rendering |
| `theme.rs` | `userland/src/theme.rs` | Color/size constants |
| `ui_utils.rs` | `userland/src/ui_utils.rs` | Single `draw_button` helper |
| Compositor | `userland/src/compositor.rs` | Wayland-like surface management |

### Current Problems

1. **No widget abstraction** - Shell manually tracks cursor, scrollback, line buffers
2. **Duplicate drawing code** - Same button logic in compositor, shell, file manager
3. **Inconsistent state management** - Each app has its own state handling pattern
4. **No layout system** - All positions are hardcoded pixel values
5. **No event routing** - Input handling is interleaved with rendering

### Existing Patterns to Preserve

```rust
// Good: DrawTarget trait for abstraction
pub trait DrawTarget {
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32);
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32);
    // ...
}

// Good: DamageTracker for efficient redraws
pub struct DamageTracker { /* ... */ }

// Good: Safe DrawBuffer with no unsafe code
pub struct DrawBuffer<'a> {
    data: &'a mut [u8],
    // ...
}
```

---

## 3. Architecture Decision

### Why Retained Mode (Not Immediate Mode)

| Criterion | Retained Mode | Immediate Mode |
|-----------|--------------|----------------|
| Memory allocation | One-time widget tree | Every frame rebuilds |
| State management | Widget owns state | External state required |
| Damage tracking | Natural (widget knows bounds) | Complex (must diff frames) |
| `no_std` compatibility | Better (predictable memory) | Harder (dynamic allocations) |
| Compositor integration | Natural fit | Awkward (no persistent surfaces) |

**Decision**: Retained mode with Elm-inspired message passing.

### Simplified Elm Architecture for SlopOS

```
┌─────────────────────────────────────────────────────┐
│                    Application                       │
│  ┌─────────┐      ┌──────────┐      ┌─────────┐    │
│  │  Model  │─────▶│  View    │─────▶│ Widget  │    │
│  │ (State) │      │ (Build)  │      │  Tree   │    │
│  └────▲────┘      └──────────┘      └────┬────┘    │
│       │                                   │         │
│       │           ┌──────────┐           │         │
│       └───────────│  Update  │◀──────────┘         │
│                   │ (Handle) │   (Messages)        │
│                   └──────────┘                     │
└─────────────────────────────────────────────────────┘
```

Unlike full Elm/Iced, we:
- Keep widget state internal (no pure functional rebuild)
- Use trait objects sparingly (prefer enums for known widgets)
- Avoid dynamic allocation where possible

---

## 4. Core Design Principles

### 4.1 Zero-Cost Abstractions

```rust
// Use const generics where possible
pub struct Container<const N: usize> {
    children: [Option<WidgetSlot>; N],
}

// Prefer enums over trait objects
pub enum Widget {
    Button(Button),
    Label(Label),
    Container(Box<Container>), // Box only for recursive types
}
```

### 4.2 Compile-Time Safety

```rust
// Typed event handlers
pub enum ShellMessage {
    KeyPressed(u8),
    CommandEntered(ArrayString<256>),
    OutputLine(ArrayString<128>),
}

// No runtime type errors
impl Application for Shell {
    type Message = ShellMessage;
    fn update(&mut self, msg: Self::Message) { /* ... */ }
}
```

### 4.3 Damage-Aware Rendering

```rust
// Widgets track their own damage
pub trait Widget {
    fn bounds(&self) -> Rect;
    fn is_dirty(&self) -> bool;
    fn mark_dirty(&mut self);
    fn draw(&self, buf: &mut DrawBuffer, clip: Rect);
}
```

### 4.4 Fixed-Size Allocations

```rust
// Use ArrayString instead of String
use arrayvec::ArrayString;

pub struct Label {
    text: ArrayString<128>,
    // ...
}

// Fixed-capacity containers
pub struct Column<const N: usize> {
    children: [Option<WidgetId>; N],
    count: usize,
}
```

---

## 5. Module Structure

```
userland/src/
├── toolkit/
│   ├── mod.rs              # Public API exports
│   ├── widget.rs           # Widget trait and WidgetId
│   ├── element.rs          # Element wrapper (type-erased widget)
│   ├── application.rs      # Application trait
│   ├── event.rs            # Event types (Mouse, Key, etc)
│   ├── message.rs          # Message routing
│   │
│   ├── widgets/
│   │   ├── mod.rs          # Widget enum and registry
│   │   ├── button.rs       # Button widget
│   │   ├── label.rs        # Label widget
│   │   ├── container.rs    # Container widget
│   │   ├── scrollable.rs   # Scrollable container
│   │   ├── text_input.rs   # Text input field
│   │   └── row.rs          # Horizontal layout
│   │   └── column.rs       # Vertical layout
│   │
│   ├── layout/
│   │   ├── mod.rs          # Layout engine entry
│   │   ├── constraints.rs  # Size constraints
│   │   ├── flex.rs         # Flexbox-like layout
│   │   ├── limits.rs       # Min/max sizing
│   │   └── node.rs         # Layout node tree
│   │
│   ├── theme/
│   │   ├── mod.rs          # Theme trait and default
│   │   ├── palette.rs      # Color palette
│   │   ├── style.rs        # Widget style structures
│   │   └── dark.rs         # Dark theme (current SlopOS style)
│   │
│   └── renderer.rs         # Drawing integration
```

---

## 6. Phase 1: Core Widget System

### 6.1 Widget Trait

```rust
// userland/src/toolkit/widget.rs

/// Rectangle for bounds and clipping
#[derive(Copy, Clone, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width as i32 &&
        y >= self.y && y < self.y + self.height as i32
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        !(self.x + self.width as i32 <= other.x ||
          other.x + other.width as i32 <= self.x ||
          self.y + self.height as i32 <= other.y ||
          other.y + other.height as i32 <= self.y)
    }
}

/// Size constraints for layout
#[derive(Copy, Clone)]
pub struct Constraints {
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
}

impl Constraints {
    pub const UNBOUNDED: Self = Self {
        min_width: 0,
        min_height: 0,
        max_width: u32::MAX,
        max_height: u32::MAX,
    };

    pub fn tight(width: u32, height: u32) -> Self {
        Self {
            min_width: width,
            min_height: height,
            max_width: width,
            max_height: height,
        }
    }
}

/// Computed size after layout
#[derive(Copy, Clone, Default)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Core widget trait
pub trait Widget {
    /// Unique identifier for this widget type
    fn type_name(&self) -> &'static str;

    /// Compute desired size given constraints
    fn layout(&mut self, constraints: Constraints) -> Size;

    /// Set position after layout
    fn set_position(&mut self, x: i32, y: i32);

    /// Get current bounds
    fn bounds(&self) -> Rect;

    /// Check if widget needs redraw
    fn is_dirty(&self) -> bool;

    /// Mark widget as needing redraw
    fn mark_dirty(&mut self);

    /// Clear dirty flag after drawing
    fn clear_dirty(&mut self);

    /// Draw the widget
    fn draw(&self, buf: &mut DrawBuffer, theme: &dyn Theme);

    /// Handle an event, optionally returning a message
    fn on_event(&mut self, event: &Event) -> Option<Message>;

    /// Get children for traversal (empty for leaf widgets)
    fn children(&self) -> &[WidgetId] {
        &[]
    }
}
```

### 6.2 WidgetId and Registry

```rust
// userland/src/toolkit/widgets/mod.rs

/// Opaque widget identifier
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct WidgetId(u16);

/// Maximum widgets per application
const MAX_WIDGETS: usize = 256;

/// Widget storage with generational indices for safety
pub struct WidgetRegistry {
    widgets: [Option<WidgetSlot>; MAX_WIDGETS],
    generation: [u16; MAX_WIDGETS],
    count: usize,
}

struct WidgetSlot {
    widget: WidgetEnum,
    generation: u16,
}

/// Enum of all widget types (avoids trait object overhead)
pub enum WidgetEnum {
    Button(Button),
    Label(Label),
    Container(Container),
    Column(Column),
    Row(Row),
    TextInput(TextInput),
    Scrollable(Scrollable),
}

impl WidgetRegistry {
    pub const fn new() -> Self {
        Self {
            widgets: [const { None }; MAX_WIDGETS],
            generation: [0; MAX_WIDGETS],
            count: 0,
        }
    }

    pub fn insert(&mut self, widget: WidgetEnum) -> Option<WidgetId> {
        for i in 0..MAX_WIDGETS {
            if self.widgets[i].is_none() {
                let gen = self.generation[i];
                self.widgets[i] = Some(WidgetSlot {
                    widget,
                    generation: gen,
                });
                self.count += 1;
                return Some(WidgetId(i as u16));
            }
        }
        None
    }

    pub fn get(&self, id: WidgetId) -> Option<&WidgetEnum> {
        let slot = self.widgets.get(id.0 as usize)?.as_ref()?;
        Some(&slot.widget)
    }

    pub fn get_mut(&mut self, id: WidgetId) -> Option<&mut WidgetEnum> {
        let slot = self.widgets.get_mut(id.0 as usize)?.as_mut()?;
        Some(&mut slot.widget)
    }
}
```

### 6.3 Event System

```rust
// userland/src/toolkit/event.rs

use arrayvec::ArrayString;

/// Pointer button state
#[derive(Copy, Clone)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Keyboard key event
#[derive(Copy, Clone)]
pub struct KeyEvent {
    pub keycode: u8,
    pub pressed: bool,
    pub modifiers: Modifiers,
}

/// Modifier keys state
#[derive(Copy, Clone, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Pointer/mouse event
#[derive(Copy, Clone)]
pub struct PointerEvent {
    pub x: i32,
    pub y: i32,
    pub button: Option<u8>,
    pub state: ButtonState,
}

/// All possible events
#[derive(Clone)]
pub enum Event {
    /// Mouse/pointer moved
    PointerMoved { x: i32, y: i32 },
    /// Mouse button pressed/released
    PointerButton(PointerEvent),
    /// Pointer entered widget bounds
    PointerEnter,
    /// Pointer left widget bounds
    PointerLeave,
    /// Key pressed/released
    Key(KeyEvent),
    /// Character input (after key translation)
    Character(char),
    /// Widget gained focus
    FocusGained,
    /// Widget lost focus
    FocusLost,
}
```

### 6.4 Message System

```rust
// userland/src/toolkit/message.rs

/// Generic message wrapper
/// Applications define their own message enum and wrap it
pub enum Message {
    /// No action needed
    None,
    /// Button with given ID was clicked
    ButtonClicked(WidgetId),
    /// Text input value changed
    TextChanged(WidgetId),
    /// Custom application message (index into app's message array)
    Custom(u16),
    /// Request focus for widget
    RequestFocus(WidgetId),
}
```

---

## 7. Phase 2: Layout Engine

### 7.1 Layout Approach

Simplified flexbox-like model:
- **Flex axis**: Main direction (Column = vertical, Row = horizontal)
- **Cross axis**: Perpendicular direction
- **Flex factor**: How much extra space a child gets (0 = fixed, 1+ = flexible)

### 7.2 Layout Node

```rust
// userland/src/toolkit/layout/node.rs

/// Layout result for a widget
#[derive(Copy, Clone, Default)]
pub struct LayoutNode {
    pub bounds: Rect,
    pub padding: Padding,
    pub margin: Margin,
}

#[derive(Copy, Clone, Default)]
pub struct Padding {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

#[derive(Copy, Clone, Default)]
pub struct Margin {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl Padding {
    pub const fn all(v: u16) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }

    pub const fn symmetric(vertical: u16, horizontal: u16) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }
}
```

### 7.3 Flex Layout

```rust
// userland/src/toolkit/layout/flex.rs

/// Flex direction
#[derive(Copy, Clone)]
pub enum Direction {
    Horizontal,
    Vertical,
}

/// Alignment on cross axis
#[derive(Copy, Clone)]
pub enum Alignment {
    Start,
    Center,
    End,
    Stretch,
}

/// How to distribute extra space
#[derive(Copy, Clone)]
pub enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Layout a list of children in flex direction
pub fn flex_layout(
    direction: Direction,
    alignment: Alignment,
    justify: Justify,
    spacing: u16,
    available: Constraints,
    children: &mut [&mut dyn Widget],
) -> Size {
    // 1. Measure all children with relaxed constraints
    // 2. Calculate total fixed size and total flex
    // 3. Distribute remaining space to flex items
    // 4. Position each child
    // (implementation details omitted for brevity)
    todo!()
}
```

---

## 8. Phase 3: Core Widgets

### 8.1 Button Widget

```rust
// userland/src/toolkit/widgets/button.rs

use arrayvec::ArrayString;

pub struct Button {
    label: ArrayString<64>,
    bounds: Rect,
    dirty: bool,
    hovered: bool,
    pressed: bool,
    on_click: Option<u16>, // Index into application message array
}

impl Button {
    pub fn new(label: &str) -> Self {
        let mut s = ArrayString::new();
        let _ = s.try_push_str(label);
        Self {
            label: s,
            bounds: Rect::default(),
            dirty: true,
            hovered: false,
            pressed: false,
            on_click: None,
        }
    }

    pub fn on_click(mut self, message_id: u16) -> Self {
        self.on_click = Some(message_id);
        self
    }
}

impl Widget for Button {
    fn type_name(&self) -> &'static str { "Button" }

    fn layout(&mut self, constraints: Constraints) -> Size {
        // Calculate size based on label text
        let text_width = font_render::str_width(self.label.as_str()) as u32;
        let text_height = FONT_CHAR_HEIGHT as u32;

        let width = (text_width + 16).clamp(constraints.min_width, constraints.max_width);
        let height = (text_height + 8).clamp(constraints.min_height, constraints.max_height);

        self.bounds.width = width;
        self.bounds.height = height;

        Size { width, height }
    }

    fn set_position(&mut self, x: i32, y: i32) {
        if self.bounds.x != x || self.bounds.y != y {
            self.bounds.x = x;
            self.bounds.y = y;
            self.dirty = true;
        }
    }

    fn bounds(&self) -> Rect { self.bounds }
    fn is_dirty(&self) -> bool { self.dirty }
    fn mark_dirty(&mut self) { self.dirty = true; }
    fn clear_dirty(&mut self) { self.dirty = false; }

    fn draw(&self, buf: &mut DrawBuffer, theme: &dyn Theme) {
        let style = if self.pressed {
            theme.button_pressed()
        } else if self.hovered {
            theme.button_hovered()
        } else {
            theme.button_normal()
        };

        // Background
        gfx::fill_rect(buf,
            self.bounds.x,
            self.bounds.y,
            self.bounds.width as i32,
            self.bounds.height as i32,
            style.background,
        );

        // Border
        gfx::draw_rect(buf,
            self.bounds.x,
            self.bounds.y,
            self.bounds.width as i32,
            self.bounds.height as i32,
            style.border,
        );

        // Text (centered)
        let text_w = font_render::str_width(self.label.as_str());
        let text_h = FONT_CHAR_HEIGHT;
        let text_x = self.bounds.x + (self.bounds.width as i32 - text_w) / 2;
        let text_y = self.bounds.y + (self.bounds.height as i32 - text_h) / 2;

        font_render::draw_str(buf, text_x, text_y, self.label.as_str(), style.text, style.background);
    }

    fn on_event(&mut self, event: &Event) -> Option<Message> {
        match event {
            Event::PointerEnter => {
                self.hovered = true;
                self.dirty = true;
                None
            }
            Event::PointerLeave => {
                self.hovered = false;
                self.pressed = false;
                self.dirty = true;
                None
            }
            Event::PointerButton(e) => {
                match e.state {
                    ButtonState::Pressed => {
                        self.pressed = true;
                        self.dirty = true;
                        None
                    }
                    ButtonState::Released => {
                        let was_pressed = self.pressed;
                        self.pressed = false;
                        self.dirty = true;

                        if was_pressed && self.bounds.contains(e.x, e.y) {
                            self.on_click.map(Message::Custom)
                        } else {
                            None
                        }
                    }
                }
            }
            _ => None,
        }
    }
}
```

### 8.2 Label Widget

```rust
// userland/src/toolkit/widgets/label.rs

pub struct Label {
    text: ArrayString<256>,
    bounds: Rect,
    dirty: bool,
    alignment: TextAlignment,
}

#[derive(Copy, Clone)]
pub enum TextAlignment {
    Left,
    Center,
    Right,
}

impl Label {
    pub fn new(text: &str) -> Self {
        let mut s = ArrayString::new();
        let _ = s.try_push_str(text);
        Self {
            text: s,
            bounds: Rect::default(),
            dirty: true,
            alignment: TextAlignment::Left,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        let _ = self.text.try_push_str(text);
        self.dirty = true;
    }

    pub fn alignment(mut self, align: TextAlignment) -> Self {
        self.alignment = align;
        self
    }
}

impl Widget for Label {
    fn type_name(&self) -> &'static str { "Label" }

    fn layout(&mut self, constraints: Constraints) -> Size {
        let text_width = font_render::str_width(self.text.as_str()) as u32;
        let text_height = font_render::str_lines(self.text.as_str()) as u32 * FONT_CHAR_HEIGHT as u32;

        let width = text_width.clamp(constraints.min_width, constraints.max_width);
        let height = text_height.clamp(constraints.min_height, constraints.max_height);

        self.bounds.width = width;
        self.bounds.height = height;

        Size { width, height }
    }

    // ... other trait methods similar to Button
}
```

### 8.3 Container Widget

```rust
// userland/src/toolkit/widgets/container.rs

const MAX_CHILDREN: usize = 32;

pub struct Container {
    children: [Option<WidgetId>; MAX_CHILDREN],
    child_count: usize,
    bounds: Rect,
    dirty: bool,
    padding: Padding,
    background: Option<u32>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: [None; MAX_CHILDREN],
            child_count: 0,
            bounds: Rect::default(),
            dirty: true,
            padding: Padding::default(),
            background: None,
        }
    }

    pub fn padding(mut self, p: Padding) -> Self {
        self.padding = p;
        self
    }

    pub fn background(mut self, color: u32) -> Self {
        self.background = Some(color);
        self
    }

    pub fn push(&mut self, id: WidgetId) -> bool {
        if self.child_count < MAX_CHILDREN {
            self.children[self.child_count] = Some(id);
            self.child_count += 1;
            self.dirty = true;
            true
        } else {
            false
        }
    }
}
```

### 8.4 Column and Row

```rust
// userland/src/toolkit/widgets/column.rs

pub struct Column {
    children: [Option<WidgetId>; 32],
    child_count: usize,
    bounds: Rect,
    dirty: bool,
    spacing: u16,
    alignment: Alignment,
}

impl Column {
    pub fn new() -> Self {
        Self {
            children: [None; 32],
            child_count: 0,
            bounds: Rect::default(),
            dirty: true,
            spacing: 4,
            alignment: Alignment::Start,
        }
    }

    pub fn spacing(mut self, s: u16) -> Self {
        self.spacing = s;
        self
    }

    pub fn align(mut self, a: Alignment) -> Self {
        self.alignment = a;
        self
    }

    pub fn push(&mut self, id: WidgetId) -> bool {
        if self.child_count < 32 {
            self.children[self.child_count] = Some(id);
            self.child_count += 1;
            self.dirty = true;
            true
        } else {
            false
        }
    }
}

// Row is identical but with horizontal direction
pub struct Row {
    // Same structure as Column
}
```

---

## 9. Phase 4: Theming System

### 9.1 Theme Trait

```rust
// userland/src/toolkit/theme/mod.rs

/// Style for a button in a specific state
#[derive(Copy, Clone)]
pub struct ButtonStyle {
    pub background: u32,
    pub border: u32,
    pub text: u32,
}

/// Style for a label
#[derive(Copy, Clone)]
pub struct LabelStyle {
    pub text: u32,
}

/// Style for text input
#[derive(Copy, Clone)]
pub struct TextInputStyle {
    pub background: u32,
    pub border: u32,
    pub text: u32,
    pub placeholder: u32,
    pub cursor: u32,
}

/// Style for scrollable container
#[derive(Copy, Clone)]
pub struct ScrollableStyle {
    pub track: u32,
    pub thumb: u32,
    pub thumb_hovered: u32,
}

/// Theme trait - provides styles for all widget states
pub trait Theme {
    // Background color for the application
    fn background(&self) -> u32;

    // Button styles
    fn button_normal(&self) -> ButtonStyle;
    fn button_hovered(&self) -> ButtonStyle;
    fn button_pressed(&self) -> ButtonStyle;
    fn button_disabled(&self) -> ButtonStyle;

    // Label styles
    fn label(&self) -> LabelStyle;
    fn label_muted(&self) -> LabelStyle;

    // Text input styles
    fn text_input_normal(&self) -> TextInputStyle;
    fn text_input_focused(&self) -> TextInputStyle;

    // Scrollable styles
    fn scrollable(&self) -> ScrollableStyle;
}
```

### 9.2 Dark Theme (Default)

```rust
// userland/src/toolkit/theme/dark.rs

use crate::gfx::rgb;

/// Default dark theme matching current SlopOS style
pub struct DarkTheme;

impl Theme for DarkTheme {
    fn background(&self) -> u32 {
        rgb(0x1E, 0x1E, 0x1E)
    }

    fn button_normal(&self) -> ButtonStyle {
        ButtonStyle {
            background: rgb(0x3E, 0x3E, 0x42),
            border: rgb(0x55, 0x55, 0x58),
            text: rgb(0xE0, 0xE0, 0xE0),
        }
    }

    fn button_hovered(&self) -> ButtonStyle {
        ButtonStyle {
            background: rgb(0x50, 0x50, 0x52),
            border: rgb(0x70, 0x70, 0x74),
            text: rgb(0xE0, 0xE0, 0xE0),
        }
    }

    fn button_pressed(&self) -> ButtonStyle {
        ButtonStyle {
            background: rgb(0x28, 0x28, 0x2C),
            border: rgb(0x40, 0x40, 0x44),
            text: rgb(0xE0, 0xE0, 0xE0),
        }
    }

    fn button_disabled(&self) -> ButtonStyle {
        ButtonStyle {
            background: rgb(0x2E, 0x2E, 0x32),
            border: rgb(0x3E, 0x3E, 0x42),
            text: rgb(0x80, 0x80, 0x80),
        }
    }

    fn label(&self) -> LabelStyle {
        LabelStyle {
            text: rgb(0xE0, 0xE0, 0xE0),
        }
    }

    fn label_muted(&self) -> LabelStyle {
        LabelStyle {
            text: rgb(0x80, 0x80, 0x80),
        }
    }

    // ... other style methods
}
```

---

## 10. Phase 5: Shell Migration

### Current Shell Architecture Problems

1. `DisplayState` with 14 `Cell<T>` fields for manual state
2. `scrollback` module with unsafe pointer arithmetic
3. `surface` module with manual buffer management
4. 1500+ lines of interleaved drawing and state code

### Migration Strategy

```rust
// New shell structure using toolkit

use crate::toolkit::{Application, Column, Container, Label, Scrollable, TextInput};

pub struct Shell {
    // Application state
    history: HistoryBuffer<256, 128>, // Fixed-size ring buffer
    current_line: ArrayString<256>,
    cwd: ArrayString<128>,

    // Widget IDs (assigned during view())
    output_scroll: Option<WidgetId>,
    input_field: Option<WidgetId>,
}

pub enum ShellMessage {
    InputChanged,
    InputSubmit,
    HistoryUp,
    HistoryDown,
    OutputAppend(ArrayString<128>),
}

impl Application for Shell {
    type Message = ShellMessage;

    fn update(&mut self, msg: Self::Message) {
        match msg {
            ShellMessage::InputSubmit => {
                let cmd = self.current_line.clone();
                self.history.push_input(&cmd);
                self.current_line.clear();
                self.execute_command(&cmd);
            }
            // ...
        }
    }

    fn view(&mut self, registry: &mut WidgetRegistry) -> WidgetId {
        // Build widget tree
        let output = Scrollable::new()
            .height(400)
            // ... populate with history output

        let prompt = Label::new(&format!("{}$ ", self.cwd));
        let input = TextInput::new()
            .placeholder("Type command...")
            .on_submit(ShellMessage::InputSubmit);

        let row = Row::new()
            .push(prompt)
            .push(input);

        let root = Column::new()
            .push(output)
            .push(row);

        registry.insert(root)
    }
}
```

---

## 11. Technical Specifications

### Memory Budget

| Component | Allocation | Notes |
|-----------|------------|-------|
| Widget Registry | 256 widgets max | ~50KB for all widget state |
| Per-widget state | ~200 bytes avg | ArrayStrings, bounds, flags |
| Layout cache | 256 nodes | ~4KB |
| Event queue | 32 events | ~512 bytes |
| Message queue | 64 messages | ~256 bytes |

### No External Crates Required

The toolkit will use only:
- `core` (Rust's no_std core library)
- `arrayvec` (already available, for fixed-size strings)
- `slopos_abi` (existing ABI crate)

### Thread Safety

Not required - SlopOS userland is single-threaded per process.

---

## 12. File Structure

```
userland/src/toolkit/
├── mod.rs                 # ~50 lines - public exports
├── widget.rs              # ~150 lines - Widget trait, Rect, Constraints
├── element.rs             # ~100 lines - Element wrapper
├── application.rs         # ~80 lines - Application trait
├── event.rs               # ~80 lines - Event types
├── message.rs             # ~40 lines - Message enum
├── widgets/
│   ├── mod.rs             # ~100 lines - WidgetEnum, WidgetRegistry
│   ├── button.rs          # ~150 lines
│   ├── label.rs           # ~100 lines
│   ├── container.rs       # ~120 lines
│   ├── column.rs          # ~150 lines
│   ├── row.rs             # ~150 lines
│   ├── text_input.rs      # ~200 lines
│   └── scrollable.rs      # ~200 lines
├── layout/
│   ├── mod.rs             # ~50 lines
│   ├── constraints.rs     # ~60 lines
│   ├── flex.rs            # ~200 lines
│   └── node.rs            # ~80 lines
├── theme/
│   ├── mod.rs             # ~100 lines - Theme trait
│   ├── style.rs           # ~60 lines - Style structs
│   └── dark.rs            # ~100 lines - DarkTheme
└── renderer.rs            # ~100 lines - DrawBuffer integration

Total: ~2000 lines
```

---

## 13. Implementation Order

### Phase 1: Foundation

| Task | Deliverable |
|------|-------------|
| Core types | `widget.rs`, `event.rs`, `message.rs` |
| Widget registry | `widgets/mod.rs` with WidgetEnum |
| Renderer integration | `renderer.rs` connecting to DrawBuffer |

### Phase 2: Basic Widgets

| Task | Deliverable |
|------|-------------|
| Button widget | `widgets/button.rs` with full interaction |
| Label widget | `widgets/label.rs` with alignment |
| Theme system | `theme/mod.rs`, `theme/dark.rs` |

### Phase 3: Layout

| Task | Deliverable |
|------|-------------|
| Column layout | `widgets/column.rs`, basic vertical stacking |
| Row layout | `widgets/row.rs`, horizontal stacking |
| Container | `widgets/container.rs` with padding |

### Phase 4: Advanced Widgets

| Task | Deliverable |
|------|-------------|
| TextInput | `widgets/text_input.rs` with cursor, selection |
| Scrollable | `widgets/scrollable.rs` with viewport |

### Phase 5: Shell Migration

| Task | Deliverable |
|------|-------------|
| Shell port | New shell using toolkit |
| Testing & polish | Bug fixes, performance |

---

## Appendix A: Key Differences from Iced

| Aspect | Iced | SlopOS Toolkit |
|--------|------|----------------|
| Allocation | Dynamic (Box, Vec) | Fixed arrays, arrayvec |
| State | Pure/immutable | Mutable widgets |
| Messages | Generic <M> | u16 indices |
| Rendering | wgpu/glow | Direct DrawBuffer |
| Widgets | Trait objects | Enum dispatch |
| Styling | Style structs | Theme trait |

---

## Appendix B: Key Differences from egui

| Aspect | egui | SlopOS Toolkit |
|--------|------|----------------|
| Mode | Immediate | Retained |
| State | External | Internal to widgets |
| Layout | On-demand | Pre-computed tree |
| Damage | Full redraw | Damage tracking |
| Memory | Temporary per frame | Persistent widget tree |

---

*This plan provides a complete roadmap for implementing a `no_std`-compatible, memory-safe UI toolkit for SlopOS, leveraging Rust's type system for compile-time guarantees while remaining practical for a kernel/userland environment.*
