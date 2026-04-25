//! Hotkey registry with rebindable shortcuts.
//!
//! Mirrors the Chatterino hotkey controller: every user-visible keyboard
//! action is represented by a [`HotkeyAction`], grouped by
//! [`HotkeyCategory`]. [`HotkeyBindings`] maps every action to exactly one
//! [`KeyBinding`] and supports conflict detection + defaults.
//!
//! The module is intentionally UI-framework-agnostic: keys are stored as
//! stable string names (see [`KeyBinding::key`]). The UI crate is
//! responsible for translating those names into whatever keycode type its
//! widget toolkit uses (e.g. `egui::Key`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Logical category for a hotkey. Used to group actions in the settings UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HotkeyCategory {
    /// Top-level window actions (search, quick switch, font zoom).
    Window,
    /// Channel-tab navigation and reordering.
    Tab,
    /// Split-pane focus and movement.
    Split,
    /// Moderation-tools window (AutoMod / low-trust / unban queues).
    Moderation,
}

impl HotkeyCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Tab => "tab",
            Self::Split => "split",
            Self::Moderation => "moderation",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Window => "Window",
            Self::Tab => "Tabs",
            Self::Split => "Splits",
            Self::Moderation => "Moderation",
        }
    }

    /// Stable iteration order for settings UI.
    pub fn all() -> [HotkeyCategory; 4] {
        [Self::Window, Self::Tab, Self::Split, Self::Moderation]
    }
}

/// Every rebindable action the app supports. New variants MUST be given a
/// default binding in [`HotkeyBindings::defaults`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HotkeyAction {
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleGlobalSearch,
    OpenMessageSearch,
    OpenQuickSwitcher,
    NextTab,
    PrevTab,
    MoveTabLeft,
    MoveTabRight,
    FirstTab,
    LastTab,
    SelectTab1,
    SelectTab2,
    SelectTab3,
    SelectTab4,
    SelectTab5,
    SelectTab6,
    SelectTab7,
    SelectTab8,
    SelectTab9,
    SplitFocusPrev,
    SplitFocusNext,
    SplitMoveLeft,
    SplitMoveRight,
    /// Approve the focused queue entry in the moderation tools window.
    ModApproveFocused,
    /// Deny the focused queue entry in the moderation tools window.
    ModDenyFocused,
    /// Move focus to the next entry in the active moderation queue.
    ModFocusNext,
    /// Move focus to the previous entry in the active moderation queue.
    ModFocusPrev,
    /// Approve every entry in the active moderation queue.
    ModBulkApprove,
    /// Deny every entry in the active moderation queue.
    ModBulkDeny,
    /// Cycle to the next moderation tab (AutoMod -> Low Trust -> Unban -> AutoMod).
    ModNextTab,
    /// Cycle to the previous moderation tab.
    ModPrevTab,
}

impl HotkeyAction {
    /// Stable iteration order matching the settings UI layout.
    pub fn all() -> [HotkeyAction; 33] {
        [
            Self::ZoomIn,
            Self::ZoomOut,
            Self::ZoomReset,
            Self::ToggleGlobalSearch,
            Self::OpenMessageSearch,
            Self::OpenQuickSwitcher,
            Self::NextTab,
            Self::PrevTab,
            Self::MoveTabLeft,
            Self::MoveTabRight,
            Self::FirstTab,
            Self::LastTab,
            Self::SelectTab1,
            Self::SelectTab2,
            Self::SelectTab3,
            Self::SelectTab4,
            Self::SelectTab5,
            Self::SelectTab6,
            Self::SelectTab7,
            Self::SelectTab8,
            Self::SelectTab9,
            Self::SplitFocusPrev,
            Self::SplitFocusNext,
            Self::SplitMoveLeft,
            Self::SplitMoveRight,
            Self::ModApproveFocused,
            Self::ModDenyFocused,
            Self::ModFocusNext,
            Self::ModFocusPrev,
            Self::ModBulkApprove,
            Self::ModBulkDeny,
            Self::ModNextTab,
            Self::ModPrevTab,
        ]
    }

    /// Stable serialized identifier used in the TOML settings file. Keep
    /// these strings backward-compatible when adding/removing actions.
    pub fn as_key(self) -> &'static str {
        match self {
            Self::ZoomIn => "zoom_in",
            Self::ZoomOut => "zoom_out",
            Self::ZoomReset => "zoom_reset",
            Self::ToggleGlobalSearch => "toggle_global_search",
            Self::OpenMessageSearch => "open_message_search",
            Self::OpenQuickSwitcher => "open_quick_switcher",
            Self::NextTab => "next_tab",
            Self::PrevTab => "prev_tab",
            Self::MoveTabLeft => "move_tab_left",
            Self::MoveTabRight => "move_tab_right",
            Self::FirstTab => "first_tab",
            Self::LastTab => "last_tab",
            Self::SelectTab1 => "select_tab_1",
            Self::SelectTab2 => "select_tab_2",
            Self::SelectTab3 => "select_tab_3",
            Self::SelectTab4 => "select_tab_4",
            Self::SelectTab5 => "select_tab_5",
            Self::SelectTab6 => "select_tab_6",
            Self::SelectTab7 => "select_tab_7",
            Self::SelectTab8 => "select_tab_8",
            Self::SelectTab9 => "select_tab_9",
            Self::SplitFocusPrev => "split_focus_prev",
            Self::SplitFocusNext => "split_focus_next",
            Self::SplitMoveLeft => "split_move_left",
            Self::SplitMoveRight => "split_move_right",
            Self::ModApproveFocused => "mod_approve_focused",
            Self::ModDenyFocused => "mod_deny_focused",
            Self::ModFocusNext => "mod_focus_next",
            Self::ModFocusPrev => "mod_focus_prev",
            Self::ModBulkApprove => "mod_bulk_approve",
            Self::ModBulkDeny => "mod_bulk_deny",
            Self::ModNextTab => "mod_next_tab",
            Self::ModPrevTab => "mod_prev_tab",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::all().into_iter().find(|a| a.as_key() == key)
    }

    pub fn category(self) -> HotkeyCategory {
        match self {
            Self::ZoomIn
            | Self::ZoomOut
            | Self::ZoomReset
            | Self::ToggleGlobalSearch
            | Self::OpenMessageSearch
            | Self::OpenQuickSwitcher => HotkeyCategory::Window,
            Self::NextTab
            | Self::PrevTab
            | Self::MoveTabLeft
            | Self::MoveTabRight
            | Self::FirstTab
            | Self::LastTab
            | Self::SelectTab1
            | Self::SelectTab2
            | Self::SelectTab3
            | Self::SelectTab4
            | Self::SelectTab5
            | Self::SelectTab6
            | Self::SelectTab7
            | Self::SelectTab8
            | Self::SelectTab9 => HotkeyCategory::Tab,
            Self::SplitFocusPrev
            | Self::SplitFocusNext
            | Self::SplitMoveLeft
            | Self::SplitMoveRight => HotkeyCategory::Split,
            Self::ModApproveFocused
            | Self::ModDenyFocused
            | Self::ModFocusNext
            | Self::ModFocusPrev
            | Self::ModBulkApprove
            | Self::ModBulkDeny
            | Self::ModNextTab
            | Self::ModPrevTab => HotkeyCategory::Moderation,
        }
    }

    /// Human-readable label for the settings UI.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ZoomIn => "Increase chat font size",
            Self::ZoomOut => "Decrease chat font size",
            Self::ZoomReset => "Reset chat font size",
            Self::ToggleGlobalSearch => "Toggle global message search",
            Self::OpenMessageSearch => "Search in current channel",
            Self::OpenQuickSwitcher => "Open quick switcher",
            Self::NextTab => "Next channel tab",
            Self::PrevTab => "Previous channel tab",
            Self::MoveTabLeft => "Move tab left",
            Self::MoveTabRight => "Move tab right",
            Self::FirstTab => "Jump to first tab",
            Self::LastTab => "Jump to last tab",
            Self::SelectTab1 => "Select tab 1",
            Self::SelectTab2 => "Select tab 2",
            Self::SelectTab3 => "Select tab 3",
            Self::SelectTab4 => "Select tab 4",
            Self::SelectTab5 => "Select tab 5",
            Self::SelectTab6 => "Select tab 6",
            Self::SelectTab7 => "Select tab 7",
            Self::SelectTab8 => "Select tab 8",
            Self::SelectTab9 => "Select tab 9",
            Self::SplitFocusPrev => "Focus previous split",
            Self::SplitFocusNext => "Focus next split",
            Self::SplitMoveLeft => "Move focused split left",
            Self::SplitMoveRight => "Move focused split right",
            Self::ModApproveFocused => "Approve focused moderation entry",
            Self::ModDenyFocused => "Deny focused moderation entry",
            Self::ModFocusNext => "Focus next moderation entry",
            Self::ModFocusPrev => "Focus previous moderation entry",
            Self::ModBulkApprove => "Approve all in current moderation queue",
            Self::ModBulkDeny => "Deny all in current moderation queue",
            Self::ModNextTab => "Next moderation tab",
            Self::ModPrevTab => "Previous moderation tab",
        }
    }
}

/// A key combination. `key` is the stable name of the key (matches
/// `egui::Key` variant names for the UI crate's mapping helper).
///
/// `key.is_empty()` represents "unbound" - the action will never fire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeyBinding {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    /// macOS ⌘ modifier. Treated as an alias of `ctrl` on non-mac platforms
    /// so users don't have to rebind per OS.
    #[serde(default)]
    pub command: bool,
    /// Stable key identifier. Uses egui's `Key` variant names
    /// (`"K"`, `"Tab"`, `"PageUp"`, `"ArrowLeft"`, `"Num1"`, `"F1"`, ...).
    /// Empty string means unbound.
    #[serde(default)]
    pub key: String,
}

impl KeyBinding {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..Default::default()
        }
    }

    pub fn with_ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }

    pub fn with_shift(mut self) -> Self {
        self.shift = true;
        self
    }

    pub fn with_alt(mut self) -> Self {
        self.alt = true;
        self
    }

    pub fn is_unbound(&self) -> bool {
        self.key.trim().is_empty()
    }

    /// Formatted label like `"Ctrl+Shift+F"` / `"Escape"`.
    pub fn display_label(&self) -> String {
        if self.is_unbound() {
            return "(unbound)".to_owned();
        }
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.command {
            parts.push("Cmd");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.push(&self.key);
        parts.join("+")
    }

    /// Canonical form used for conflict detection. Case-insensitive on
    /// the key name so `"k"` and `"K"` collide.
    pub fn canonical(&self) -> Option<(bool, bool, bool, bool, String)> {
        if self.is_unbound() {
            return None;
        }
        Some((
            self.ctrl,
            self.shift,
            self.alt,
            self.command,
            self.key.to_ascii_lowercase(),
        ))
    }
}

/// Map of action -> binding. Lookup, conflict detection, defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HotkeyBindings {
    map: BTreeMap<HotkeyAction, KeyBinding>,
}

impl HotkeyBindings {
    /// Factory for default Crust keybindings. Kept close to Chatterino
    /// defaults where sensible.
    pub fn defaults() -> Self {
        let mut map = BTreeMap::new();
        // Window
        map.insert(HotkeyAction::ZoomIn, KeyBinding::new("Equals").with_ctrl());
        map.insert(HotkeyAction::ZoomOut, KeyBinding::new("Minus").with_ctrl());
        map.insert(HotkeyAction::ZoomReset, KeyBinding::new("Num0").with_ctrl());
        map.insert(
            HotkeyAction::ToggleGlobalSearch,
            KeyBinding::new("F").with_ctrl().with_shift(),
        );
        map.insert(
            HotkeyAction::OpenMessageSearch,
            KeyBinding::new("F").with_ctrl(),
        );
        map.insert(
            HotkeyAction::OpenQuickSwitcher,
            KeyBinding::new("K").with_ctrl(),
        );
        // Tab navigation
        map.insert(HotkeyAction::NextTab, KeyBinding::new("Tab").with_ctrl());
        map.insert(
            HotkeyAction::PrevTab,
            KeyBinding::new("Tab").with_ctrl().with_shift(),
        );
        map.insert(
            HotkeyAction::MoveTabLeft,
            KeyBinding::new("ArrowLeft").with_alt().with_shift(),
        );
        map.insert(
            HotkeyAction::MoveTabRight,
            KeyBinding::new("ArrowRight").with_alt().with_shift(),
        );
        map.insert(HotkeyAction::FirstTab, KeyBinding::new("Home").with_ctrl());
        map.insert(HotkeyAction::LastTab, KeyBinding::new("End").with_ctrl());
        map.insert(HotkeyAction::SelectTab1, KeyBinding::new("Num1").with_ctrl());
        map.insert(HotkeyAction::SelectTab2, KeyBinding::new("Num2").with_ctrl());
        map.insert(HotkeyAction::SelectTab3, KeyBinding::new("Num3").with_ctrl());
        map.insert(HotkeyAction::SelectTab4, KeyBinding::new("Num4").with_ctrl());
        map.insert(HotkeyAction::SelectTab5, KeyBinding::new("Num5").with_ctrl());
        map.insert(HotkeyAction::SelectTab6, KeyBinding::new("Num6").with_ctrl());
        map.insert(HotkeyAction::SelectTab7, KeyBinding::new("Num7").with_ctrl());
        map.insert(HotkeyAction::SelectTab8, KeyBinding::new("Num8").with_ctrl());
        map.insert(HotkeyAction::SelectTab9, KeyBinding::new("Num9").with_ctrl());
        // Splits
        map.insert(
            HotkeyAction::SplitFocusPrev,
            KeyBinding::new("PageUp").with_ctrl().with_alt(),
        );
        map.insert(
            HotkeyAction::SplitFocusNext,
            KeyBinding::new("PageDown").with_ctrl().with_alt(),
        );
        map.insert(
            HotkeyAction::SplitMoveLeft,
            {
                let mut b = KeyBinding::new("ArrowLeft").with_ctrl().with_alt();
                b.shift = true;
                b
            },
        );
        map.insert(
            HotkeyAction::SplitMoveRight,
            {
                let mut b = KeyBinding::new("ArrowRight").with_ctrl().with_alt();
                b.shift = true;
                b
            },
        );
        // Moderation: gated to mod-tools window focus, so single-letter binds
        // don't conflict with global typing.
        map.insert(HotkeyAction::ModApproveFocused, KeyBinding::new("A"));
        map.insert(HotkeyAction::ModDenyFocused, KeyBinding::new("D"));
        map.insert(HotkeyAction::ModFocusNext, KeyBinding::new("J"));
        map.insert(HotkeyAction::ModFocusPrev, KeyBinding::new("K"));
        map.insert(
            HotkeyAction::ModBulkApprove,
            KeyBinding::new("A").with_shift(),
        );
        map.insert(
            HotkeyAction::ModBulkDeny,
            KeyBinding::new("D").with_shift(),
        );
        map.insert(HotkeyAction::ModNextTab, KeyBinding::new("Tab"));
        map.insert(
            HotkeyAction::ModPrevTab,
            KeyBinding::new("Tab").with_shift(),
        );
        Self { map }
    }

    /// Reads an existing binding or the default if the action wasn't
    /// explicitly set. Guarantees every known action resolves to *some*
    /// binding, so callers don't have to handle the `None` case.
    pub fn get(&self, action: HotkeyAction) -> KeyBinding {
        if let Some(b) = self.map.get(&action) {
            return b.clone();
        }
        HotkeyBindings::defaults()
            .map
            .remove(&action)
            .unwrap_or_default()
    }

    pub fn set(&mut self, action: HotkeyAction, binding: KeyBinding) {
        self.map.insert(action, binding);
    }

    pub fn iter(&self) -> impl Iterator<Item = (HotkeyAction, &KeyBinding)> {
        HotkeyAction::all()
            .into_iter()
            .filter_map(move |a| self.map.get(&a).map(|b| (a, b)))
    }

    /// Load from the TOML-friendly `(String, KeyBinding)` representation
    /// used in settings storage and the event bus.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (String, KeyBinding)>,
    {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            if let Some(action) = HotkeyAction::from_key(&k) {
                map.insert(action, v);
            }
        }
        // Fill in any missing actions with defaults so new releases don't
        // silently lose bindings for brand-new actions.
        let defaults = HotkeyBindings::defaults();
        for action in HotkeyAction::all() {
            map.entry(action).or_insert_with(|| defaults.get(action));
        }
        Self { map }
    }

    pub fn to_pairs(&self) -> Vec<(String, KeyBinding)> {
        self.iter()
            .map(|(a, b)| (a.as_key().to_owned(), b.clone()))
            .collect()
    }

    /// Returns every action whose current binding collides with another.
    /// Two bindings collide when their canonical forms (modifier set +
    /// case-insensitive key name) are equal.
    ///
    /// Actions with `key.is_empty()` (i.e. unbound) never collide.
    pub fn conflicts(&self) -> Vec<HotkeyAction> {
        let mut buckets: BTreeMap<(bool, bool, bool, bool, String), Vec<HotkeyAction>> =
            BTreeMap::new();
        for (action, binding) in self.iter() {
            if let Some(key) = binding.canonical() {
                buckets.entry(key).or_default().push(action);
            }
        }
        let mut out: Vec<HotkeyAction> = Vec::new();
        for (_, actions) in buckets {
            if actions.len() > 1 {
                out.extend(actions);
            }
        }
        out.sort_by_key(|a| a.as_key());
        out
    }

    /// Find the first other action whose binding collides with `needle`
    /// (excluding `needle` itself). Useful for inline conflict messages.
    pub fn find_conflict(&self, action: HotkeyAction, needle: &KeyBinding) -> Option<HotkeyAction> {
        let Some(target) = needle.canonical() else {
            return None;
        };
        for (other, binding) in self.iter() {
            if other == action {
                continue;
            }
            if binding.canonical() == Some(target.clone()) {
                return Some(other);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_action() {
        let b = HotkeyBindings::defaults();
        for a in HotkeyAction::all() {
            assert!(
                !b.get(a).is_unbound(),
                "default missing for action {:?}",
                a
            );
        }
    }

    #[test]
    fn defaults_have_no_conflicts() {
        assert!(HotkeyBindings::defaults().conflicts().is_empty());
    }

    #[test]
    fn conflict_detected_when_two_actions_share_binding() {
        let mut b = HotkeyBindings::defaults();
        b.set(
            HotkeyAction::ZoomIn,
            KeyBinding::new("K").with_ctrl(),
        );
        let conflicts = b.conflicts();
        assert!(conflicts.contains(&HotkeyAction::ZoomIn));
        assert!(conflicts.contains(&HotkeyAction::OpenQuickSwitcher));
        assert_eq!(
            b.find_conflict(
                HotkeyAction::ZoomIn,
                &KeyBinding::new("K").with_ctrl(),
            ),
            Some(HotkeyAction::OpenQuickSwitcher),
        );
    }

    #[test]
    fn unbound_never_conflicts() {
        let mut b = HotkeyBindings::defaults();
        b.set(HotkeyAction::ZoomIn, KeyBinding::default());
        b.set(HotkeyAction::ZoomOut, KeyBinding::default());
        assert!(b.conflicts().is_empty());
    }

    #[test]
    fn round_trip_pairs_preserves_changes() {
        let mut b = HotkeyBindings::defaults();
        b.set(
            HotkeyAction::OpenQuickSwitcher,
            KeyBinding::new("P").with_ctrl(),
        );
        let pairs = b.to_pairs();
        let reloaded = HotkeyBindings::from_pairs(pairs);
        assert_eq!(
            reloaded.get(HotkeyAction::OpenQuickSwitcher),
            KeyBinding::new("P").with_ctrl(),
        );
    }

    #[test]
    fn case_insensitive_key_conflict() {
        let mut b = HotkeyBindings::defaults();
        // Set tab1 to lowercase "k" with Ctrl (OpenQuickSwitcher is "K" + Ctrl)
        b.set(HotkeyAction::SelectTab1, KeyBinding::new("k").with_ctrl());
        let conflicts = b.conflicts();
        assert!(conflicts.contains(&HotkeyAction::SelectTab1));
        assert!(conflicts.contains(&HotkeyAction::OpenQuickSwitcher));
    }

    #[test]
    fn display_label_formats_modifiers_in_order() {
        let b = KeyBinding::new("K").with_ctrl().with_shift();
        assert_eq!(b.display_label(), "Ctrl+Shift+K");
        assert_eq!(KeyBinding::default().display_label(), "(unbound)");
    }
}
