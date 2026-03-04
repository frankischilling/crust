/// Crust design-system constants.
///
/// One place to change colours/metrics so every widget stays in sync.
use egui::{Color32, CornerRadius, FontId, Margin, Stroke, Vec2};

// Palette colors

/// Deepest background - window body, sidebar.
pub const BG_BASE: Color32 = Color32::from_rgb(13, 13, 18);
/// Slightly elevated surface - top bar, input tray.
pub const BG_SURFACE: Color32 = Color32::from_rgb(20, 20, 28);
/// Raised surface - popup backgrounds, autocomplete.
pub const BG_RAISED: Color32 = Color32::from_rgb(28, 28, 40);
/// Deeply elevated - dialog / window fill.
pub const BG_DIALOG: Color32 = Color32::from_rgb(22, 22, 32);

/// Subtle/default border.
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(38, 38, 55);
/// Focused / prominent border.
pub const BORDER_ACCENT: Color32 = Color32::from_rgb(80, 70, 120);

/// Primary text.
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(225, 225, 235);
/// Secondary text - timestamps, hints, labels.
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(130, 130, 148);
/// Muted - very low-contrast information.
pub const TEXT_MUTED: Color32 = Color32::from_rgb(72, 72, 90);

/// Twitch-ish purple accent - buttons, highlights, username.
pub const ACCENT: Color32 = Color32::from_rgb(145, 95, 255);
/// Hover tint for accent buttons.
pub const ACCENT_DIM: Color32 = Color32::from_rgb(100, 65, 190);

/// Hover fill for generic buttons and interactive widgets.
/// Clearly distinct from BG_SURFACE/BG_RAISED with a subtle purple tint.
pub const HOVER_BG: Color32 = Color32::from_rgb(45, 38, 72);

/// Active channel row fill - translucent purple.
pub const ACTIVE_CHANNEL_BG: Color32 = Color32::from_rgba_premultiplied(55, 38, 100, 100);
/// Hover row fill for channel list.
pub const HOVER_ROW_BG: Color32 = Color32::from_rgba_premultiplied(40, 40, 60, 80);

/// Success / connected green.
pub const GREEN: Color32 = Color32::from_rgb(72, 200, 110);
/// Warning / reconnecting yellow.
pub const YELLOW: Color32 = Color32::from_rgb(235, 195, 55);
/// Error / disconnected red.
pub const RED: Color32 = Color32::from_rgb(220, 65, 65);

/// Clickable URL / hyperlink blue.
pub const LINK: Color32 = Color32::from_rgb(100, 180, 255);
/// @mention highlight blue.
pub const MENTION: Color32 = Color32::from_rgb(100, 200, 255);
/// Timestamp / very-low-contrast inline text.
pub const TIMESTAMP: Color32 = Color32::from_rgb(100, 100, 115);
/// Separator dot between timestamp and badge/name.
pub const SEPARATOR: Color32 = Color32::from_rgb(75, 75, 85);
/// Twitch brand purple (used for Twitch-specific accents).
pub const TWITCH_PURPLE: Color32 = Color32::from_rgb(145, 70, 235);
/// Gold for sub events and highlights.
pub const GOLD: Color32 = Color32::from_rgb(255, 215, 0);
/// Cyan-ish for raid events.
pub const RAID_CYAN: Color32 = Color32::from_rgb(100, 200, 255);
/// Orange for bits events.
pub const BITS_ORANGE: Color32 = Color32::from_rgb(255, 160, 50);

// Stroke styles

pub const STROKE_SUBTLE: Stroke = Stroke {
    width: 1.0,
    color: BORDER_SUBTLE,
};
pub const STROKE_ACCENT: Stroke = Stroke {
    width: 1.0,
    color: BORDER_ACCENT,
};

// Layout metrics

/// Corner radius shared across most interactive elements.
pub const RADIUS: CornerRadius = CornerRadius::same(5);
/// Tighter radius for inline pills / badges.
pub const RADIUS_SM: CornerRadius = CornerRadius::same(3);

/// Standard toolbar row height.
pub const BAR_H: f32 = 28.0;
/// Minimum sidebar width.
pub const SIDEBAR_MIN_W: f32 = 80.0;
/// Maximum sidebar width.
pub const SIDEBAR_MAX_W: f32 = 300.0;
/// Default sidebar width.
pub const SIDEBAR_W: f32 = 160.0;

/// Inner margin for toolbar panels.
pub const BAR_MARGIN: Margin = Margin {
    left: 10,
    right: 10,
    top: 4,
    bottom: 4,
};
/// Inner margin for sidebar panels.
pub const SIDEBAR_MARGIN: Margin = Margin {
    left: 8,
    right: 6,
    top: 8,
    bottom: 8,
};
/// Inner margin for the chat input tray.
pub const INPUT_MARGIN: Margin = Margin {
    left: 10,
    right: 10,
    top: 4,
    bottom: 4,
};

/// Horizontal spacing inside toolbars.
pub const TOOLBAR_SPACING: Vec2 = Vec2::new(6.0, 0.0);
/// Default item spacing for most panels.
pub const ITEM_SPACING: Vec2 = Vec2::new(4.0, 3.0);
/// Channel row vertical gap.
pub const CHANNEL_ROW_GAP: f32 = 2.0;

// Typography styles

/// Body text - chat messages, general labels.
pub fn body() -> FontId {
    FontId::proportional(13.5)
}

/// Small label - timestamps, system messages, secondary info.
pub fn small() -> FontId {
    FontId::proportional(12.5)
}

/// Heading / section label (all-caps sidebar header etc).
pub fn heading() -> FontId {
    FontId::proportional(11.5)
}

/// Tiny label - badges, room-state pills, character count.
pub fn tiny() -> FontId {
    FontId::proportional(10.5)
}
