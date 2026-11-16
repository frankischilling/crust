/// Crust design-system tokens.
///
/// One place to change colours/metrics so every widget stays in sync.
/// Colour functions read a global dark/light flag so the theme can be
/// switched at runtime without restarting.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use egui::{Color32, CornerRadius, FontId, Margin, Stroke, Vec2};

// Global theme mode

/// `false` = dark (default), `true` = light.
static LIGHT_MODE: AtomicBool = AtomicBool::new(false);

// Global font sizing (chat body + UI scale)

/// Default chat body font size. `small/heading/tiny` are offsets from this.
pub const DEFAULT_CHAT_FONT_SIZE: f32 = 13.5;
/// Minimum permitted chat font size.
pub const MIN_CHAT_FONT_SIZE: f32 = 8.0;
/// Maximum permitted chat font size.
pub const MAX_CHAT_FONT_SIZE: f32 = 32.0;
/// Default UI zoom multiplier, applied on top of the monitor's native scale.
pub const DEFAULT_UI_FONT_SIZE: f32 = 1.0;
/// Minimum UI scale ratio. Below this, egui hit-targets shrink below usable size.
pub const MIN_UI_FONT_SIZE: f32 = 0.75;
/// Maximum UI scale ratio. Above this, fixed-width panels (sidebar, analytics)
/// start clipping and buttons collide.
pub const MAX_UI_FONT_SIZE: f32 = 1.75;

/// Section font sizes. Storage persists the absolute pt value OR `0.0`
/// which means "auto-follow chat font" (default).
pub const DEFAULT_TOPBAR_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_TABS_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_TIMESTAMPS_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_PILLS_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_POPUPS_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_CHIPS_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_USERCARD_FONT_SIZE: f32 = 0.0;
pub const DEFAULT_DIALOG_FONT_SIZE: f32 = 0.0;
pub const MIN_SECTION_FONT_SIZE: f32 = 8.0;
pub const MAX_SECTION_FONT_SIZE: f32 = 28.0;

/// Minimum derived auto-follow section size (we never return values below this
/// even if the chat font is tiny, so UI chrome stays legible).
const AUTO_SECTION_FLOOR: f32 = 8.5;

/// Compute the effective section size from a stored slot: `0` = auto.
fn effective_section(slot: &AtomicU32, auto_offset: f32) -> f32 {
    let bits = slot.load(Ordering::Relaxed);
    if bits == 0 {
        return (chat_font_size() - auto_offset).max(AUTO_SECTION_FLOOR);
    }
    let raw = f32::from_bits(bits);
    if raw <= 0.0 || !raw.is_finite() {
        (chat_font_size() - auto_offset).max(AUTO_SECTION_FLOOR)
    } else {
        raw
    }
}

static CHAT_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static UI_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static TOPBAR_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static TABS_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static TIMESTAMPS_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static PILLS_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static POPUPS_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static CHIPS_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static USERCARD_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);
static DIALOG_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0);

fn load_f32(slot: &AtomicU32, fallback: f32) -> f32 {
    let bits = slot.load(Ordering::Relaxed);
    if bits == 0 {
        fallback
    } else {
        f32::from_bits(bits)
    }
}

/// Current chat body font size in points (clamped to valid range).
#[inline]
pub fn chat_font_size() -> f32 {
    load_f32(&CHAT_FONT_SIZE_BITS, DEFAULT_CHAT_FONT_SIZE)
}

/// Current UI zoom multiplier, independent of the monitor's native scale.
#[inline]
pub fn ui_font_size() -> f32 {
    load_f32(&UI_FONT_SIZE_BITS, DEFAULT_UI_FONT_SIZE)
}

/// Persist a new chat font size (clamped). Returns the clamped value.
pub fn set_chat_font_size(size: f32) -> f32 {
    let clamped = size.clamp(MIN_CHAT_FONT_SIZE, MAX_CHAT_FONT_SIZE);
    CHAT_FONT_SIZE_BITS.store(clamped.to_bits(), Ordering::Relaxed);
    clamped
}

/// Persist a new UI scale ratio (clamped). Returns the clamped value.
pub fn set_ui_font_size(ratio: f32) -> f32 {
    let clamped = ratio.clamp(MIN_UI_FONT_SIZE, MAX_UI_FONT_SIZE);
    UI_FONT_SIZE_BITS.store(clamped.to_bits(), Ordering::Relaxed);
    clamped
}

/// Persist a raw slider value. `0.0` is kept as-is to mean "auto". Other
/// values are clamped into the section range.
fn set_section_font(slot: &AtomicU32, size: f32) -> f32 {
    if size <= 0.0 || !size.is_finite() {
        slot.store(0, Ordering::Relaxed);
        return 0.0;
    }
    let clamped = size.clamp(MIN_SECTION_FONT_SIZE, MAX_SECTION_FONT_SIZE);
    slot.store(clamped.to_bits(), Ordering::Relaxed);
    clamped
}

/// Raw persisted section slider value (0.0 = auto).
fn raw_section_font(slot: &AtomicU32) -> f32 {
    let bits = slot.load(Ordering::Relaxed);
    if bits == 0 {
        0.0
    } else {
        f32::from_bits(bits)
    }
}

/// Top chrome toolbar font size (effective pt).
#[inline]
pub fn topbar_font_size() -> f32 {
    effective_section(&TOPBAR_FONT_SIZE_BITS, 1.0)
}
/// Raw slider value (0.0 = auto).
#[inline]
pub fn topbar_font_size_raw() -> f32 {
    raw_section_font(&TOPBAR_FONT_SIZE_BITS)
}
#[inline]
pub fn set_topbar_font_size(size: f32) -> f32 {
    set_section_font(&TOPBAR_FONT_SIZE_BITS, size)
}

/// Channel tab chip font size (effective pt).
#[inline]
pub fn tabs_font_size() -> f32 {
    effective_section(&TABS_FONT_SIZE_BITS, 1.0)
}
#[inline]
pub fn tabs_font_size_raw() -> f32 {
    raw_section_font(&TABS_FONT_SIZE_BITS)
}
#[inline]
pub fn set_tabs_font_size(size: f32) -> f32 {
    set_section_font(&TABS_FONT_SIZE_BITS, size)
}

/// Message timestamp font size (effective pt).
#[inline]
pub fn timestamps_font_size() -> f32 {
    effective_section(&TIMESTAMPS_FONT_SIZE_BITS, 1.0)
}
#[inline]
pub fn timestamps_font_size_raw() -> f32 {
    raw_section_font(&TIMESTAMPS_FONT_SIZE_BITS)
}
#[inline]
pub fn set_timestamps_font_size(size: f32) -> f32 {
    set_section_font(&TIMESTAMPS_FONT_SIZE_BITS, size)
}

/// Room-state / viewer-count pill font size (effective pt).
#[inline]
pub fn pills_font_size() -> f32 {
    effective_section(&PILLS_FONT_SIZE_BITS, 2.0)
}
#[inline]
pub fn pills_font_size_raw() -> f32 {
    raw_section_font(&PILLS_FONT_SIZE_BITS)
}
#[inline]
pub fn set_pills_font_size(size: f32) -> f32 {
    set_section_font(&PILLS_FONT_SIZE_BITS, size)
}

/// FontId helpers returning a proportional font at the section size.
#[inline]
pub fn topbar_font() -> FontId {
    FontId::proportional(topbar_font_size())
}
#[inline]
pub fn tabs_font() -> FontId {
    FontId::proportional(tabs_font_size())
}
#[inline]
pub fn timestamps_font() -> FontId {
    FontId::proportional(timestamps_font_size())
}
#[inline]
pub fn pills_font() -> FontId {
    FontId::proportional(pills_font_size())
}

/// Tooltip / popover label size (effective pt). Auto-follows chat at -1.5.
#[inline]
pub fn popups_font_size() -> f32 {
    effective_section(&POPUPS_FONT_SIZE_BITS, 1.5)
}
#[inline]
pub fn popups_font_size_raw() -> f32 {
    raw_section_font(&POPUPS_FONT_SIZE_BITS)
}
#[inline]
pub fn set_popups_font_size(size: f32) -> f32 {
    set_section_font(&POPUPS_FONT_SIZE_BITS, size)
}
#[inline]
pub fn popups_font() -> FontId {
    FontId::proportional(popups_font_size())
}

/// Inline chip / inline badge size (effective pt). Auto-follows chat at +4.5
/// so default lands near the original 18pt for viewer-count chips.
#[inline]
pub fn chips_font_size() -> f32 {
    effective_section(&CHIPS_FONT_SIZE_BITS, -4.5)
}
#[inline]
pub fn chips_font_size_raw() -> f32 {
    raw_section_font(&CHIPS_FONT_SIZE_BITS)
}
#[inline]
pub fn set_chips_font_size(size: f32) -> f32 {
    set_section_font(&CHIPS_FONT_SIZE_BITS, size)
}
#[inline]
pub fn chips_font() -> FontId {
    FontId::proportional(chips_font_size())
}

/// User-card heading size (effective pt). Auto-follows chat at +14.5
/// so default lands near the original 28pt username heading.
#[inline]
pub fn usercard_font_size() -> f32 {
    effective_section(&USERCARD_FONT_SIZE_BITS, -14.5)
}
#[inline]
pub fn usercard_font_size_raw() -> f32 {
    raw_section_font(&USERCARD_FONT_SIZE_BITS)
}
#[inline]
pub fn set_usercard_font_size(size: f32) -> f32 {
    set_section_font(&USERCARD_FONT_SIZE_BITS, size)
}
#[inline]
pub fn usercard_font() -> FontId {
    FontId::proportional(usercard_font_size())
}

/// Login / setup dialog helper-text size (effective pt).
#[inline]
pub fn dialog_font_size() -> f32 {
    effective_section(&DIALOG_FONT_SIZE_BITS, 3.5)
}
#[inline]
pub fn dialog_font_size_raw() -> f32 {
    raw_section_font(&DIALOG_FONT_SIZE_BITS)
}
#[inline]
pub fn set_dialog_font_size(size: f32) -> f32 {
    set_section_font(&DIALOG_FONT_SIZE_BITS, size)
}
#[inline]
pub fn dialog_font() -> FontId {
    FontId::proportional(dialog_font_size())
}

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

// Palette colours - each function returns the dark or light variant.

/// Deepest background - window body, sidebar.
#[inline]
pub fn bg_base() -> Color32 {
    if is_light() {
        Color32::from_rgb(245, 245, 250)
    } else {
        Color32::from_rgb(13, 13, 18)
    }
}
/// Slightly elevated surface - top bar, input tray.
#[inline]
pub fn bg_surface() -> Color32 {
    if is_light() {
        Color32::from_rgb(235, 235, 242)
    } else {
        Color32::from_rgb(20, 20, 28)
    }
}
/// Raised surface - popup backgrounds, autocomplete.
#[inline]
pub fn bg_raised() -> Color32 {
    if is_light() {
        Color32::from_rgb(225, 225, 235)
    } else {
        Color32::from_rgb(28, 28, 40)
    }
}
/// Deeply elevated - dialog / window fill.
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
/// Secondary text - timestamps, hints, labels.
#[inline]
pub fn text_secondary() -> Color32 {
    if is_light() {
        Color32::from_rgb(85, 85, 100)
    } else {
        Color32::from_rgb(130, 130, 148)
    }
}
/// Muted - very low-contrast information.
#[inline]
pub fn text_muted() -> Color32 {
    if is_light() {
        Color32::from_rgb(145, 145, 160)
    } else {
        Color32::from_rgb(72, 72, 90)
    }
}
/// Helper text, section labels, and dialog subtitles.
#[inline]
pub fn text_helper() -> Color32 {
    text_secondary()
}
/// High-contrast text for colored pills/buttons.
#[inline]
pub fn text_on_accent() -> Color32 {
    if is_light() {
        Color32::from_rgb(255, 255, 255)
    } else {
        Color32::from_rgb(246, 244, 255)
    }
}

/// Twitch-ish purple accent - buttons, highlights, username.
#[inline]
pub fn accent() -> Color32 {
    if is_light() {
        Color32::from_rgb(120, 70, 230)
    } else {
        Color32::from_rgb(145, 95, 255)
    }
}
/// Dim accent - active press, selection fill.
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

/// Active channel row fill - translucent purple.
#[inline]
pub fn active_channel_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(130, 100, 210, 60)
    } else {
        Color32::from_rgba_unmultiplied(55, 38, 100, 100)
    }
}
/// Hover row fill for channel list.
#[inline]
pub fn hover_row_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(120, 110, 170, 40)
    } else {
        Color32::from_rgba_unmultiplied(40, 40, 60, 80)
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

/// Kick platform green.
#[inline]
pub fn kick_green() -> Color32 {
    if is_light() {
        Color32::from_rgb(56, 182, 36)
    } else {
        Color32::from_rgb(83, 252, 24)
    }
}

/// Apply alpha to an RGB color while preserving channels.
#[inline]
pub fn alpha(color: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

fn relative_luminance(color: Color32) -> f32 {
    let linear = |channel: u8| {
        let channel = channel as f32 / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
}

/// Keep informational text readable on an opaque surface without changing its fill.
pub(crate) fn readable_text_color(color: Color32, background: Color32) -> Color32 {
    let color = Color32::from_rgb(color.r(), color.g(), color.b());
    let background = relative_luminance(background);
    let contrast = |color| {
        let foreground = relative_luminance(color);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    };
    if contrast(color) >= 4.5 {
        return color;
    }

    let target = if contrast(Color32::BLACK) >= contrast(Color32::WHITE) {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    let mut failing = 0.0;
    let mut passing = 1.0;
    let mut result = target;
    for _ in 0..8 {
        let middle = (failing + passing) * 0.5;
        let candidate = color.lerp_to_gamma(target, middle);
        if contrast(candidate) >= 4.5 {
            passing = middle;
            result = candidate;
        } else {
            failing = middle;
        }
    }
    result
}

/// Adjust plain username text for the current theme, leaving stored colors intact.
pub(crate) fn readable_username_color(color: Color32) -> Color32 {
    let light_theme = is_light();
    let [r, g, b] = [color.r(), color.g(), color.b()].map(|channel| channel as f32 / 255.0);
    let minimum = r.min(g).min(b);
    let maximum = r.max(g).max(b);
    let chroma = maximum - minimum;
    let lightness = (minimum + maximum) * 0.5;
    let saturation = if chroma > 0.0 {
        chroma / (1.0 - (2.0 * lightness - 1.0).abs())
    } else {
        0.0
    };
    let hue = if chroma == 0.0 {
        0.0
    } else if maximum == r {
        60.0 * ((g - b) / chroma).rem_euclid(6.0)
    } else if maximum == g {
        60.0 * ((b - r) / chroma + 2.0)
    } else {
        60.0 * ((r - g) / chroma + 4.0)
    };

    // Preserve hue and saturation while compensating for the apparent brightness
    // of yellow/green on light backgrounds and blue/violet on dark backgrounds.
    let adjusted = if light_theme {
        let lightness = lightness.min(0.5);
        if lightness > 0.4 && (36.0..120.0).contains(&hue) {
            lightness - ((hue - 36.0) / 84.0 * std::f32::consts::PI).sin() * saturation * 0.4
        } else {
            lightness
        }
    } else {
        let lightness = lightness.max(0.5);
        if lightness < 0.6 && (196.0..300.0).contains(&hue) {
            lightness + ((hue - 196.0) / 104.0 * std::f32::consts::PI).sin() * saturation * 0.4
        } else {
            lightness
        }
    };
    let color_at_lightness = |adjusted: f32| {
        if adjusted == lightness {
            return Color32::from_rgb(color.r(), color.g(), color.b());
        }
        let adjusted_chroma = (1.0 - (2.0 * adjusted - 1.0).abs()) * saturation;
        let scale = if chroma > 0.0 {
            adjusted_chroma / chroma
        } else {
            0.0
        };
        let offset = adjusted - adjusted_chroma * 0.5;
        let channel = |value: f32| (((value - minimum) * scale + offset) * 255.0).round() as u8;
        Color32::from_rgb(channel(r), channel(g), channel(b))
    };
    let background = relative_luminance(bg_raised());
    let target = if light_theme {
        (background + 0.05) / 4.5 - 0.05
    } else {
        (background + 0.05) * 4.5 - 0.05
    };
    let readable = |color| {
        if light_theme {
            relative_luminance(color) <= target
        } else {
            relative_luminance(color) >= target
        }
    };
    let display_color = color_at_lightness(adjusted);
    if readable(display_color) {
        return display_color;
    }

    // A bounded lightness search covers bright greens and other colors that
    // still blend into a chat surface after the hue-specific adjustment.
    let mut failing = adjusted;
    let mut passing = if light_theme { 0.0 } else { 1.0 };
    let mut display_color = color_at_lightness(passing);
    for _ in 0..8 {
        let middle = (passing + failing) * 0.5;
        let candidate = color_at_lightness(middle);
        if readable(candidate) {
            passing = middle;
            display_color = candidate;
        } else {
            failing = middle;
        }
    }
    display_color
}

/// Parse six hexadecimal digits, with an optional leading hash.
pub(crate) fn parse_hex_rgb(value: &str) -> Option<Color32> {
    if value.trim_start_matches('#').len() != 6 {
        return None;
    }
    parse_hex_rgb_opaque(value)
}

/// Parse RGB or RGBA while preserving the supplied alpha.
pub(crate) fn parse_hex_rgba(value: &str) -> Option<Color32> {
    let [r, g, b, a] = parse_hex_channels(value)?;
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

/// Parse RGB or RGBA for text that must remain opaque.
pub(crate) fn parse_hex_rgb_opaque(value: &str) -> Option<Color32> {
    let [r, g, b, _] = parse_hex_channels(value)?;
    Some(Color32::from_rgb(r, g, b))
}

fn parse_hex_channels(value: &str) -> Option<[u8; 4]> {
    let bytes = value.trim_start_matches('#').as_bytes();
    if !matches!(bytes.len(), 6 | 8) {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let mut channels = [0, 0, 0, 255];
    for (channel, pair) in channels.iter_mut().zip(bytes.chunks_exact(2)) {
        *channel = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(channels)
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

/// Header / toolbar group fill.
#[inline]
pub fn bg_header() -> Color32 {
    if is_light() {
        Color32::from_rgb(228, 228, 236)
    } else {
        Color32::from_rgb(24, 24, 34)
    }
}

/// Card/dialog section fill.
#[inline]
pub fn bg_card() -> Color32 {
    if is_light() {
        Color32::from_rgb(240, 240, 247)
    } else {
        Color32::from_rgb(26, 26, 36)
    }
}

/// Selected tab fill.
#[inline]
pub fn tab_selected_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(120, 90, 210, 52)
    } else {
        Color32::from_rgba_unmultiplied(96, 76, 170, 160)
    }
}

/// Tab hover fill.
#[inline]
pub fn tab_hover_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(120, 120, 150, 26)
    } else {
        Color32::from_rgba_unmultiplied(80, 80, 110, 72)
    }
}

/// Soft fill for badges and pills.
#[inline]
pub fn pill_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(110, 110, 140, 28)
    } else {
        Color32::from_rgba_unmultiplied(100, 100, 130, 70)
    }
}

/// Mention badge fill.
#[inline]
pub fn mention_pill_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(215, 175, 40, 44)
    } else {
        Color32::from_rgba_unmultiplied(220, 175, 45, 84)
    }
}

/// Destructive action fill.
#[inline]
pub fn danger_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(210, 60, 60, 28)
    } else {
        Color32::from_rgba_unmultiplied(200, 55, 55, 54)
    }
}

/// Soft info stripe fill.
#[inline]
pub fn info_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(90, 130, 220, 24)
    } else {
        Color32::from_rgba_unmultiplied(85, 115, 210, 48)
    }
}

/// Subtle red-tinted background for destructive notices.
#[inline]
pub fn danger_soft_bg() -> Color32 {
    alpha(red(), if is_light() { 24 } else { 36 })
}

/// Strong destructive fill for critical action buttons.
#[inline]
pub fn danger_strong_bg() -> Color32 {
    alpha(red(), if is_light() { 208 } else { 224 })
}

/// Soft amber background for warnings/highlights.
#[inline]
pub fn warning_soft_bg() -> Color32 {
    alpha(yellow(), if is_light() { 28 } else { 44 })
}

/// Strong success fill for positive action buttons.
#[inline]
pub fn success_strong_bg() -> Color32 {
    alpha(green(), if is_light() { 198 } else { 212 })
}

/// Neutral dark scrim used behind dense log rows/cards.
#[inline]
pub fn scrim_bg() -> Color32 {
    if is_light() {
        Color32::from_rgba_unmultiplied(0, 0, 0, 20)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 60)
    }
}

/// Drag ghost tint when dropping to split view.
#[inline]
pub fn split_success_bg() -> Color32 {
    alpha(green(), if is_light() { 210 } else { 220 })
}

/// Split-view helper label color.
#[inline]
pub fn split_success_text() -> Color32 {
    alpha(
        green().gamma_multiply(1.6),
        if is_light() { 210 } else { 190 },
    )
}

/// Whisper thread selected row background.
#[inline]
pub fn whisper_selected_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(225, 218, 246)
    } else {
        Color32::from_rgb(49, 39, 73)
    }
}

/// Outgoing whisper bubble colors.
#[inline]
pub fn whisper_self_fill() -> Color32 {
    if is_light() {
        Color32::from_rgb(227, 219, 248)
    } else {
        Color32::from_rgb(63, 49, 96)
    }
}
#[inline]
pub fn whisper_self_stroke() -> Color32 {
    if is_light() {
        Color32::from_rgb(153, 128, 214)
    } else {
        Color32::from_rgb(120, 98, 182)
    }
}
#[inline]
pub fn whisper_self_text() -> Color32 {
    if is_light() {
        Color32::from_rgb(31, 24, 52)
    } else {
        Color32::from_rgb(242, 236, 255)
    }
}
#[inline]
pub fn whisper_self_meta() -> Color32 {
    if is_light() {
        Color32::from_rgb(92, 74, 142)
    } else {
        Color32::from_rgb(186, 168, 232)
    }
}

/// Incoming whisper bubble colors.
#[inline]
pub fn whisper_other_fill() -> Color32 {
    if is_light() {
        Color32::from_rgb(245, 246, 252)
    } else {
        Color32::from_rgb(34, 36, 48)
    }
}
#[inline]
pub fn whisper_other_stroke() -> Color32 {
    if is_light() {
        Color32::from_rgb(203, 206, 224)
    } else {
        Color32::from_rgb(62, 66, 88)
    }
}

/// Background for perf sparklines.
#[inline]
pub fn sparkline_bg() -> Color32 {
    if is_light() {
        Color32::from_rgb(240, 243, 247)
    } else {
        Color32::from_rgb(28, 31, 38)
    }
}

/// Accent line color in perf sparkline.
#[inline]
pub fn sparkline_stroke() -> Color32 {
    if is_light() {
        Color32::from_rgb(50, 150, 98)
    } else {
        Color32::from_rgb(90, 200, 140)
    }
}

// Layout metrics

/// Corner radius shared across most interactive elements.
pub const RADIUS: CornerRadius = CornerRadius::same(5);
/// Tighter radius for inline pills / badges.
pub const RADIUS_SM: CornerRadius = CornerRadius::same(3);

/// Ratio of the current chat font size to the default (used to scale
/// bar/button heights so the layout keeps up with the font).
#[inline]
pub fn font_scale() -> f32 {
    (chat_font_size() / DEFAULT_CHAT_FONT_SIZE).clamp(0.6, 2.4)
}

/// Standard toolbar row height. Grows with chat font so labels don't clip.
#[inline]
pub fn bar_h() -> f32 {
    28.0 * font_scale()
}
/// Compact square icon button size.
#[inline]
pub fn icon_btn_sm() -> f32 {
    18.0 * font_scale()
}
/// Default square icon button size.
#[inline]
pub fn icon_btn() -> f32 {
    24.0 * font_scale()
}
/// Dialog header padding.
pub const DIALOG_MARGIN: Margin = Margin {
    left: 12,
    right: 12,
    top: 10,
    bottom: 10,
};
/// Card/section padding for redesigned secondary surfaces.
pub const CARD_MARGIN: Margin = Margin {
    left: 10,
    right: 10,
    top: 9,
    bottom: 9,
};
/// Inner margin for the top tab strip.
pub const TAB_STRIP_MARGIN: Margin = Margin {
    left: 6,
    right: 6,
    top: 2,
    bottom: 2,
};
/// Minimum sidebar width.
pub const SIDEBAR_MIN_W: f32 = 80.0;
/// Minimum sidebar width once the window enters a narrow layout.
pub const SIDEBAR_COMPACT_MIN_W: f32 = 64.0;
/// Maximum sidebar width.
pub const SIDEBAR_MAX_W: f32 = 300.0;
/// Default sidebar width.
pub const SIDEBAR_W: f32 = 160.0;
/// Default sidebar width in narrow mode.
pub const SIDEBAR_COMPACT_W: f32 = 124.0;

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
/// Default gap between major UI sections.
pub const SECTION_GAP: f32 = 8.0;

// Typography styles

/// Body text - chat messages, general labels.
pub fn body() -> FontId {
    FontId::proportional(chat_font_size())
}

/// Small label - timestamps, system messages, secondary info.
pub fn small() -> FontId {
    FontId::proportional((chat_font_size() - 1.0).max(MIN_CHAT_FONT_SIZE))
}

/// Heading / section label (all-caps sidebar header etc).
pub fn heading() -> FontId {
    FontId::proportional((chat_font_size() - 2.0).max(MIN_CHAT_FONT_SIZE))
}

/// Tiny label - badges, room-state pills, character count.
pub fn tiny() -> FontId {
    FontId::proportional((chat_font_size() - 3.0).max(MIN_CHAT_FONT_SIZE))
}

#[cfg(test)]
mod color_tests {
    use super::*;

    fn contrast_ratio(foreground: Color32, background: Color32) -> f64 {
        let luminance = |color: Color32| {
            [color.r(), color.g(), color.b()]
                .into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(channel, weight)| {
                    let value = f64::from(channel) / 255.0;
                    weight
                        * if value <= 0.04045 {
                            value / 12.92
                        } else {
                            ((value + 0.055) / 1.055).powf(2.4)
                        }
                })
                .sum::<f64>()
        };
        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn informational_text_has_contrast_on_light_and_dark_tinted_surfaces() {
        let surfaces = [
            Color32::from_rgb(13, 13, 18),
            Color32::from_rgb(20, 20, 28),
            Color32::from_rgb(28, 28, 40),
            Color32::from_rgb(24, 14, 14),
            Color32::from_rgb(245, 245, 250),
            Color32::from_rgb(235, 235, 242),
            Color32::from_rgb(225, 225, 235),
            Color32::from_rgb(255, 235, 235),
        ];
        let accents = [
            Color32::from_rgb(255, 215, 0),
            Color32::from_rgb(180, 140, 10),
            Color32::from_rgb(235, 195, 55),
            Color32::from_rgb(200, 45, 45),
            Color32::from_rgb(220, 65, 65),
            Color32::from_rgb(20, 140, 210),
            Color32::from_rgb(100, 200, 255),
            Color32::from_rgb(155, 102, 214),
            Color32::from_rgb(168, 85, 247),
            Color32::from_rgb(139, 92, 246),
            Color32::from_rgb(99, 102, 241),
            Color32::from_rgb(236, 72, 153),
            Color32::from_rgb(244, 114, 182),
            Color32::from_rgb(145, 145, 160),
            Color32::from_rgb(72, 72, 90),
            Color32::from_rgb(85, 85, 100),
            Color32::from_rgb(130, 130, 148),
        ];
        for surface in surfaces {
            for accent in accents {
                for opacity in [0, 18, 20, 22, 24, 28, 32, 44] {
                    let background = surface.blend(alpha(accent, opacity));
                    let foreground = readable_text_color(accent, background);
                    let ratio = contrast_ratio(foreground, background);
                    assert_eq!(foreground.a(), 255);
                    assert!(ratio >= 4.5, "{accent:?} on {background:?}: {ratio}");
                    if contrast_ratio(accent, background) >= 4.5 {
                        assert_eq!(
                            foreground, accent,
                            "readable colors must stay unchanged"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn informational_text_remains_readable_on_mid_tone_surfaces() {
        for value in 0..=255 {
            let background = Color32::from_gray(value);
            let foreground = readable_text_color(Color32::from_gray(128), background);
            assert!(contrast_ratio(foreground, background) >= 4.5);
        }
    }

    #[test]
    fn translucent_palette_fills_do_not_add_light_beyond_their_opacity() {
        for fill in [
            active_channel_bg(),
            hover_row_bg(),
            tab_selected_bg(),
            tab_hover_bg(),
            pill_bg(),
            mention_pill_bg(),
            danger_bg(),
            info_bg(),
        ] {
            let premultiplied = egui::Rgba::from(fill);
            assert!(premultiplied.r() <= premultiplied.a() + 0.005, "{fill:?}");
            assert!(premultiplied.g() <= premultiplied.a() + 0.005, "{fill:?}");
            assert!(premultiplied.b() <= premultiplied.a() + 0.005, "{fill:?}");
        }
    }

    #[test]
    fn hex_colors_preserve_rgb_and_alpha_contracts() {
        let rgb = Color32::from_rgb(0x12, 0xab, 0x34);
        assert_eq!(parse_hex_rgb("#12Ab34"), Some(rgb));
        assert_eq!(parse_hex_rgba("12Ab34"), Some(rgb));
        assert_eq!(parse_hex_rgb("12ab3480"), None);
        assert_eq!(
            parse_hex_rgba("#12ab3480"),
            Some(Color32::from_rgba_unmultiplied(0x12, 0xab, 0x34, 0x80))
        );
        assert_eq!(parse_hex_rgb_opaque("#12ab3400"), Some(rgb));
        assert_eq!(parse_hex_rgba("#12ab3400"), Some(Color32::TRANSPARENT));
    }

    #[test]
    fn hex_colors_reject_malformed_unicode_and_non_hex_bytes() {
        for value in [
            "",
            "#fff",
            "1234567",
            "123456789",
            "zzzzzz",
            "+1abcd",
            "12345+",
            "12ab34gg",
            "€abc",
            "a€bc",
            "ab€c",
            "abc€",
            "€abcde",
            "12€abc",
            "1234€a",
            "12345€",
            "ééé",
            "🦀ab",
            "12🦀ab",
        ] {
            assert_eq!(parse_hex_rgb(value), None, "RGB: {value:?}");
            assert_eq!(parse_hex_rgba(value), None, "RGBA: {value:?}");
            assert_eq!(parse_hex_rgb_opaque(value), None, "opaque: {value:?}");
        }
    }
}
