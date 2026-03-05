/// Crust design-system tokens.
///
/// One place to change colours/metrics so every widget stays in sync.
/// Colour functions read a global dark/light flag so the theme can be
/// switched at runtime without restarting.
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Color32, CornerRadius, FontId, Margin, Stroke, Vec2};

// ---------------------------------------------------------------------------
// Global theme mode
// ---------------------------------------------------------------------------

/// `false` = dark (default), `true` = light.
static LIGHT_MODE: AtomicBool = AtomicBool::new(false);

/// Switch to dark mode.
pub fn set_dark() {
    LIGHT_MODE.store(false, Ordering::Relaxed);
}
/// Switch to light mode.
pub fn set_light() {
    LIGHT_MODE.store(true, Ordering::Relaxed);
}
/// Returns `true` when light mode is active.
#[inline]
pub fn is_light() -> bool {
    LIGHT_MODE.load(Ordering::Relaxed)
}
/// Apply theme from a settings string (`"light"` or `"dark"`).
pub fn apply_from_str(s: &str) {
    if s.eq_ignore_ascii_case("light") {
        set_light();
    } else {
        set_dark();
    }
}

// ---------------------------------------------------------------------------
// Palette colours - each function returns the dark or light variant.
// ---------------------------------------------------------------------------

/// Deepest background – window body, sidebar.
#[inline]
pub fn bg_base() -> Color32 {
    if is_light() {
        Color32::from_rgb(245, 245, 250)
    } else {
        Color32::from_rgb(13, 13, 18)
    }
}
/// Slightly elevated surface – top bar, input tray.
#[inline]
pub fn bg_surface() -> Color32 {
    if is_light() {
        Color32::from_rgb(235, 235, 242)
    } else {
        Color32::from_rgb(20, 20, 28)
    }
}
/// Raised surface – popup backgrounds, autocomplete.
#[inline]
pub fn bg_raised() -> Color32 {
    if is_light() {
        Color32::from_rgb(225, 225, 235)
    } else {
        Color32::from_rgb(28, 28, 40)
    }
}
/// Deeply elevated – dialog / window fill.
#[inline]
pub fn bg_dialog() -> Color32 {
    if is_light() {
        Color32::from_rgb(250, 250, 255)
    } else {
        Color32::from_rgb(22, 22, 32)
    }
}

/// Subtle/default border.
#[inline]
pub fn border_subtle() -> Color32 {
    if is_light() {
        Color32::from_rgb(200, 200, 215)
    } else {
        Color32::from_rgb(38, 38, 55)
    }
}
/// Focused / prominent border.
#[inline]
pub fn border_accent() -> Color32 {
    if is_light() {
        Color32::from_rgb(130, 110, 200)
    } else {
        Color32::from_rgb(80, 70, 120)
    }
}

/// Primary text.
#[inline]
pub fn text_primary() -> Color32 {
    if is_light() {
        Color32::from_rgb(25, 25, 35)
    } else {
        Color32::from_rgb(225, 225, 235)
    }
}
/// Secondary text – timestamps, hints, labels.
#[inline]
pub fn text_secondary() -> Color32 {
    if is_light() {
        Color32::from_rgb(85, 85, 100)
    } else {
        Color32::from_rgb(130, 130, 148)
    }
}
/// Muted – very low-contrast information.
#[inline]
pub fn text_muted() -> Color32 {
    if is_light() {
        Color32::from_rgb(145, 145, 160)
    } else {
        Color32::from_rgb(72, 72, 90)
    }
}

/// Twitch-ish purple accent – buttons, highlights, username.
#[inline]
pub fn accent() -> Color32 {
    if is_light() {
        Color32::from_rgb(120, 70, 230)
    } else {
        Color32::from_rgb(145, 95, 255)
    }
}
/// Dim accent – active press, selection fill.
#[inline]
pub fn accent_dim() -> Color32 {
    if is_light() {
        Color32::from_rgb(160, 120, 240)
    } else {
        Color32::from_rgb(100, 65, 190)
    }
}

/// Hover fill for generic buttons and interactive widgets.
#[inline]
pub fn hover_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(215, 210, 235)
    } else {
        Color32::from_rgb(45, 38, 72)
    }
}

/// Active channel row fill – translucent purple.
#[inline]
pub fn active_channel_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_premultiplied(130, 100, 210, 60)
    } else {
        Color32::from_rgba_premultiplied(55, 38, 100, 100)
    }
}
/// Hover row fill for channel list.
#[inline]
pub fn hover_row_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_premultiplied(120, 110, 170, 40)
    } else {
        Color32::from_rgba_premultiplied(40, 40, 60, 80)
    }
}

/// Success / connected green.
#[inline]
pub fn green() -> Color32 {
    if is_light() {
        Color32::from_rgb(30, 160, 70)
    } else {
        Color32::from_rgb(72, 200, 110)
    }
}
/// Warning / reconnecting yellow.
#[inline]
pub fn yellow() -> Color32 {
    if is_light() {
        Color32::from_rgb(180, 140, 10)
    } else {
        Color32::from_rgb(235, 195, 55)
    }
}
/// Error / disconnected red.
#[inline]
pub fn red() -> Color32 {
    if is_light() {
        Color32::from_rgb(200, 45, 45)
    } else {
        Color32::from_rgb(220, 65, 65)
    }
}

/// Clickable URL / hyperlink.
#[inline]
pub fn link() -> Color32 {
    if is_light() {
        Color32::from_rgb(30, 90, 200)
    } else {
        Color32::from_rgb(100, 180, 255)
    }
}
/// @mention highlight.
#[inline]
pub fn mention() -> Color32 {
    if is_light() {
        Color32::from_rgb(20, 110, 200)
    } else {
        Color32::from_rgb(100, 200, 255)
    }
}
/// Timestamp / very-low-contrast inline text.
#[inline]
pub fn timestamp() -> Color32 {
    if is_light() {
        Color32::from_rgb(140, 140, 155)
    } else {
        Color32::from_rgb(100, 100, 115)
    }
}
/// Separator dot between timestamp and badge/name.
#[inline]
pub fn separator() -> Color32 {
    if is_light() {
        Color32::from_rgb(180, 180, 195)
    } else {
        Color32::from_rgb(75, 75, 85)
    }
}
/// Twitch brand purple (used for Twitch-specific accents).
#[inline]
pub fn twitch_purple() -> Color32 {
    Color32::from_rgb(145, 70, 235)
}
/// Gold for sub events and highlights.
#[inline]
pub fn gold() -> Color32 {
    Color32::from_rgb(255, 215, 0)
}
/// Cyan-ish for raid events.
#[inline]
pub fn raid_cyan() -> Color32 {
    if is_light() {
        Color32::from_rgb(20, 140, 210)
    } else {
        Color32::from_rgb(100, 200, 255)
    }
}
/// Orange for bits events.
#[inline]
pub fn bits_orange() -> Color32 {
    Color32::from_rgb(255, 160, 50)
}

// Stroke styles

#[inline]
pub fn stroke_subtle() -> Stroke {
    Stroke {
        width: 1.0,
        color: border_subtle(),
    }
}
#[inline]
pub fn stroke_accent() -> Stroke {
    Stroke {
        width: 1.0,
        color: border_accent(),
    }
}

// Extra semantic colours for widgets that were using hardcoded values.

/// Tooltip / autocomplete popup background.
#[inline]
pub fn tooltip_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(255, 255, 255)
    } else {
        Color32::from_rgb(40, 40, 48)
    }
}
/// Section header background (emote picker, etc.).
#[inline]
pub fn section_header_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(230, 230, 238)
    } else {
        Color32::from_rgb(45, 45, 55)
    }
}
/// Placeholder / hint text.
#[inline]
pub fn placeholder_text() -> Color32 {
    if is_light() {
        Color32::from_rgb(160, 160, 175)
    } else {
        Color32::from_rgb(120, 120, 130)
    }
}
/// Live-channel red tint for sidebar.
#[inline]
pub fn live_tint_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(255, 235, 235)
    } else {
        Color32::from_rgb(24, 14, 14)
    }
}
/// Overlay fill used behind animated borders / dialogs.
#[inline]
pub fn overlay_fill() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(240, 238, 248, 200)
    } else {
        Color32::from_rgba_unmultiplied(18, 16, 26, 200)
    }
}

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
