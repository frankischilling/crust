use serde::{Deserialize, Serialize};

/// Twitch badge metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Badge {
    pub name: String,
    pub version: String,
    /// CDN image URL (1x), populated by the badge loader.
    #[serde(default)]
    pub url: Option<String>,
}

/// One gradient stop for a painted sender name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SenderNamePaintStop {
    /// Relative position in `[0.0, 1.0]`.
    pub at: f32,
    /// CSS-like hex color (`#RRGGBB`).
    pub color: String,
}

/// One drop-shadow entry for a painted sender name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SenderNamePaintShadow {
    /// Horizontal offset in logical px.
    pub x_offset: f32,
    /// Vertical offset in logical px.
    pub y_offset: f32,
    /// Blur radius hint in logical px.
    pub radius: f32,
    /// CSS-like hex color (`#RRGGBB` or `#RRGGBBAA`).
    pub color: String,
}

/// Optional rich paint metadata for a sender name (e.g. 7TV paint).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SenderNamePaint {
    /// Paint function identifier from provider metadata.
    pub function: String,
    /// Optional angle for linear gradients.
    #[serde(default)]
    pub angle: Option<f32>,
    /// Whether the gradient should repeat outside the primary stop range.
    #[serde(default)]
    pub repeat: bool,
    /// Optional URL texture source for URL paints.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Optional drop-shadows associated with this paint.
    #[serde(default)]
    pub shadows: Vec<SenderNamePaintShadow>,
    /// Ordered gradient stops.
    #[serde(default)]
    pub stops: Vec<SenderNamePaintStop>,
}

/// Chat message sender metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub user_id: super::UserId,
    /// Raw login name (lowercase).
    pub login: String,
    /// Display name as supplied by the server.
    pub display_name: String,
    /// #rrggbb color from IRC tag, or None.
    pub color: Option<String>,
    /// Optional provider paint metadata (e.g. 7TV name paint).
    #[serde(default)]
    pub name_paint: Option<SenderNamePaint>,
    pub badges: Vec<Badge>,
}
