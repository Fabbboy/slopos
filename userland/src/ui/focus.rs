use super::traits::{Widget, WidgetId};

/// Unique identifier for a focus scope.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FocusScopeId(pub u32);

/// A focus scope traps Tab navigation within a subtree.
pub struct FocusScope {
    pub id: FocusScopeId,
    /// Widgets in this scope, in tab order.
    pub chain: Vec<WidgetId>,
    /// Which widget was focused when this scope was entered.
    pub restore_to: Option<WidgetId>,
}

/// Manages keyboard focus, tab chains, and focus scopes.
pub struct FocusManager {
    /// Currently focused widget.
    focused: Option<WidgetId>,
    /// Global tab chain (depth-first order of all focusable widgets).
    tab_chain: Vec<WidgetId>,
    /// Scope stack. Bottom = global scope.
    scope_stack: Vec<FocusScope>,
    /// Whether the user is navigating via keyboard (for focus-visible).
    keyboard_active: bool,
    /// Counter for generating scope IDs.
    next_scope_id: u32,
}

impl FocusManager {
    pub fn new() -> Self {
        Self {
            focused: None,
            tab_chain: Vec::new(),
            scope_stack: Vec::new(),
            keyboard_active: false,
            next_scope_id: 1,
        }
    }

    /// The currently focused widget.
    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    /// Whether a widget is currently focused.
    pub fn is_focused(&self, id: WidgetId) -> bool {
        self.focused == Some(id)
    }

    /// Whether focus rings should be rendered (keyboard navigation active).
    pub fn is_focus_visible(&self) -> bool {
        self.keyboard_active
    }

    /// Set focus to a specific widget.
    pub fn set_focused(&mut self, id: Option<WidgetId>) {
        self.focused = id;
    }

    /// Record that a keyboard key was pressed (non-modifier).
    pub fn note_keyboard_input(&mut self) {
        self.keyboard_active = true;
    }

    /// Record that a pointer button was pressed.
    pub fn note_pointer_input(&mut self) {
        self.keyboard_active = false;
    }

    /// Rebuild the tab chain by walking the widget tree in DFS order.
    pub fn rebuild_tab_chain(&mut self, root: &dyn Widget) {
        self.tab_chain.clear();
        Self::collect_focusable(root, &mut self.tab_chain);
    }

    fn collect_focusable(widget: &dyn Widget, chain: &mut Vec<WidgetId>) {
        if widget.focus_policy().is_tab_focusable() {
            chain.push(widget.id());
        }
        for child in widget.children() {
            Self::collect_focusable(child.as_ref(), chain);
        }
    }

    /// Active tab chain (topmost scope, or global).
    fn active_chain(&self) -> &[WidgetId] {
        if let Some(scope) = self.scope_stack.last() {
            &scope.chain
        } else {
            &self.tab_chain
        }
    }

    /// Move focus to the next widget in the tab chain.
    pub fn move_focus_next(&mut self) {
        self.keyboard_active = true;
        let chain = self.active_chain();
        if chain.is_empty() {
            return;
        }
        let next = match self.focused {
            Some(current) => {
                if let Some(pos) = chain.iter().position(|&id| id == current) {
                    chain[(pos + 1) % chain.len()]
                } else {
                    chain[0]
                }
            }
            None => chain[0],
        };
        self.focused = Some(next);
    }

    /// Move focus to the previous widget in the tab chain.
    pub fn move_focus_prev(&mut self) {
        self.keyboard_active = true;
        let chain = self.active_chain();
        if chain.is_empty() {
            return;
        }
        let prev = match self.focused {
            Some(current) => {
                if let Some(pos) = chain.iter().position(|&id| id == current) {
                    if pos == 0 {
                        chain[chain.len() - 1]
                    } else {
                        chain[pos - 1]
                    }
                } else {
                    chain[chain.len() - 1]
                }
            }
            None => chain[chain.len() - 1],
        };
        self.focused = Some(prev);
    }

    /// Push a focus scope. Tab navigation is trapped within it.
    pub fn push_scope(&mut self, focusable_ids: Vec<WidgetId>) -> FocusScopeId {
        let id = FocusScopeId(self.next_scope_id);
        self.next_scope_id += 1;
        let scope = FocusScope {
            id,
            chain: focusable_ids,
            restore_to: self.focused,
        };
        self.scope_stack.push(scope);
        // Focus first widget in the new scope.
        if let Some(scope) = self.scope_stack.last() {
            if let Some(&first) = scope.chain.first() {
                self.focused = Some(first);
            }
        }
        id
    }

    /// Pop the topmost focus scope. Restores previous focus.
    pub fn pop_scope(&mut self) -> Option<FocusScopeId> {
        if let Some(scope) = self.scope_stack.pop() {
            self.focused = scope.restore_to;
            Some(scope.id)
        } else {
            None
        }
    }
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new()
    }
}
