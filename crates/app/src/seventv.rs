use std::collections::HashMap;

use crust_core::model::{Badge, Sender, SenderNamePaintShadow, SenderNamePaintStop};
use tracing::{debug, info, warn};

const SEVENTV_GQL_URL: &str = "https://api.7tv.app/v3/gql";

#[derive(Debug, Clone, Default)]
pub(crate) struct SevenTvUserStyleRaw {
    pub(crate) color: Option<i32>,
    pub(crate) badge_id: Option<String>,
    pub(crate) avatar_url: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SevenTvBadgeMeta {
    pub(crate) tooltip: Option<String>,
    pub(crate) url: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct SevenTvPaintMeta {
    pub(crate) function: String,
    pub(crate) angle: Option<f32>,
    pub(crate) repeat: bool,
    pub(crate) image_url: Option<String>,
    pub(crate) shadows: Vec<SenderNamePaintShadow>,
    pub(crate) stops: Vec<SenderNamePaintStop>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SevenTvResolvedStyle {
    pub(crate) color_hex: Option<String>,
    pub(crate) badge: Option<Badge>,
    pub(crate) avatar_url: Option<String>,
}

pub(crate) enum SevenTvCosmeticUpdate {
    Catalog {
        badges: HashMap<String, SevenTvBadgeMeta>,
        paints: HashMap<String, SevenTvPaintMeta>,
    },
    UserStyle {
        twitch_user_id: String,
        style: Option<SevenTvUserStyleRaw>,
    },
    /// Batch of Twitch user-ids discovered in history messages that need
    /// their 7TV styles resolved.
    BatchUserLookup { user_ids: Vec<String> },
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<SevenTvGraphQlError>,
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlError {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvBadgeFile {
    name: String,
}

pub(crate) fn resolve_7tv_user_style(
    style: &SevenTvUserStyleRaw,
    badges: &HashMap<String, SevenTvBadgeMeta>,
    _paints: &HashMap<String, SevenTvPaintMeta>,
) -> SevenTvResolvedStyle {
    let color_hex = style.color.and_then(seven_tv_color_to_hex);

    let badge = style
        .badge_id
        .as_ref()
        .and_then(|id| badges.get(id))
        .map(|b| Badge {
            name: "7tv".to_owned(),
            version: b.tooltip.clone().unwrap_or_else(|| "1".to_owned()),
            url: Some(b.url.clone()),
        });

    SevenTvResolvedStyle {
        color_hex,
        badge,
        avatar_url: style.avatar_url.clone(),
    }
}

pub(crate) fn apply_7tv_cosmetics_to_sender(sender: &mut Sender, style: &SevenTvResolvedStyle) {
    if let Some(ref color) = style.color_hex {
        sender.color = Some(color.clone());
    }
    sender.name_paint = None;

    if let Some(ref badge) = style.badge {
        if let Some(existing) = sender
            .badges
            .iter_mut()
            .find(|b| b.name.eq_ignore_ascii_case("7tv"))
        {
            *existing = badge.clone();
        } else {
            sender.badges.insert(0, badge.clone());
        }
    }
}

pub(crate) async fn load_7tv_cosmetics_catalog(
    client: &reqwest::Client,
) -> Option<(
    HashMap<String, SevenTvBadgeMeta>,
    HashMap<String, SevenTvPaintMeta>,
)> {
    #[derive(serde::Deserialize)]
    struct RespData {
        cosmetics: Cosmetics,
    }

    #[derive(serde::Deserialize)]
    struct Cosmetics {
        #[serde(default)]
        badges: Vec<BadgeNode>,
        #[serde(default)]
        paints: Vec<PaintNode>,
    }

    #[derive(serde::Deserialize)]
    struct BadgeNode {
        id: String,
        tooltip: Option<String>,
        host: BadgeHost,
    }

    #[derive(serde::Deserialize)]
    struct BadgeHost {
        url: String,
        files: Vec<SevenTvBadgeFile>,
    }

    #[derive(serde::Deserialize)]
    struct PaintNode {
        id: String,
        #[serde(default)]
        function: Option<String>,
        #[serde(default)]
        angle: Option<f32>,
        #[serde(default)]
        repeat: bool,
        #[serde(default)]
        image_url: Option<String>,
        #[serde(default)]
        color: Option<i32>,
        #[serde(default)]
        stops: Vec<PaintStopNode>,
        #[serde(default)]
        shadows: Vec<PaintShadowNode>,
    }

    #[derive(serde::Deserialize)]
    struct PaintStopNode {
        at: f32,
        color: i32,
    }

    #[derive(serde::Deserialize)]
    struct PaintShadowNode {
        #[serde(default)]
        x_offset: f32,
        #[serde(default)]
        y_offset: f32,
        #[serde(default)]
        radius: f32,
        color: i32,
    }

    let query = r#"
        query {
            cosmetics {
                badges {
                    id
                    tooltip
                    host {
                        url
                        files {
                            name
                        }
                    }
                }
                paints {
                    id
                    function
                    angle
                    repeat
                    image_url
                    color
                    stops {
                        at
                        color
                    }
                    shadows {
                        x_offset
                        y_offset
                        radius
                        color
                    }
                }
            }
        }
    "#;

    let resp = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("7TV cosmetics fetch failed: {e}");
            return None;
        }
    };

    let payload = match resp.json::<SevenTvGraphQlResponse<RespData>>().await {
        Ok(p) => p,
        Err(e) => {
            warn!("7TV cosmetics parse failed: {e}");
            return None;
        }
    };

    if !payload.errors.is_empty() {
        let messages = payload
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        warn!("7TV cosmetics GraphQL errors: {messages}");
    }

    let Some(data) = payload.data else {
        return None;
    };

    let badges: HashMap<String, SevenTvBadgeMeta> = data
        .cosmetics
        .badges
        .into_iter()
        .filter_map(|b| {
            let file = choose_7tv_badge_file(&b.host.files)?;
            Some((
                b.id,
                SevenTvBadgeMeta {
                    tooltip: b.tooltip,
                    url: seven_tv_badge_url(&b.host.url, &file.name),
                },
            ))
        })
        .collect();

    let paints: HashMap<String, SevenTvPaintMeta> = data
        .cosmetics
        .paints
        .into_iter()
        .filter_map(|p| {
            let fallback = p.color.and_then(seven_tv_color_to_hex);
            let stops = p
                .stops
                .into_iter()
                .filter_map(|s| {
                    seven_tv_color_to_hex_with_alpha(s.color)
                        .map(|color| SenderNamePaintStop { at: s.at, color })
                })
                .collect::<Vec<_>>();
            let normalized = normalize_7tv_paint_stops(stops, fallback);
            let image_url = p.image_url.as_deref().and_then(normalize_external_url);
            if normalized.is_empty() && image_url.is_none() {
                return None;
            }
            let shadows = p
                .shadows
                .into_iter()
                .filter_map(|s| {
                    seven_tv_color_to_hex_with_alpha(s.color).map(|color| SenderNamePaintShadow {
                        x_offset: s.x_offset,
                        y_offset: s.y_offset,
                        radius: s.radius,
                        color,
                    })
                })
                .collect::<Vec<_>>();
            Some((
                p.id,
                SevenTvPaintMeta {
                    function: p
                        .function
                        .unwrap_or_else(|| "linear-gradient".to_owned())
                        .to_ascii_lowercase(),
                    angle: p.angle,
                    repeat: p.repeat,
                    image_url,
                    shadows,
                    stops: normalized,
                },
            ))
        })
        .collect();

    info!(
        "Loaded 7TV cosmetics catalog (badges={}, paints={})",
        badges.len(),
        paints.len()
    );

    Some((badges, paints))
}

pub(crate) async fn load_7tv_user_style_for_twitch(
    client: &reqwest::Client,
    twitch_user_id: &str,
) -> Option<SevenTvUserStyleRaw> {
    #[derive(serde::Deserialize)]
    struct RespData {
        #[serde(rename = "userByConnection")]
        user_by_connection: Option<UserNode>,
    }

    #[derive(serde::Deserialize)]
    struct UserNode {
        style: Option<StyleNode>,
        avatar_url: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct StyleNode {
        color: i32,
        badge_id: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct StyleNodeV2 {
        color: i32,
        badge: Option<BadgeIdNode>,
    }

    #[derive(serde::Deserialize)]
    struct BadgeIdNode {
        id: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct RespDataV2 {
        #[serde(rename = "userByConnection")]
        user_by_connection: Option<UserNodeV2>,
    }

    #[derive(serde::Deserialize)]
    struct UserNodeV2 {
        style: Option<StyleNodeV2>,
        avatar_url: Option<String>,
    }

    if twitch_user_id.trim().is_empty() {
        return None;
    }

    let query = r#"
        query($id: String!) {
            userByConnection(platform: TWITCH, id: $id) {
                avatar_url
                style {
                    color
                    badge_id
                }
            }
        }
    "#;

    let resp = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({
            "query": query,
            "variables": { "id": twitch_user_id }
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!("7TV user style fetch failed for {twitch_user_id}: {e}");
            return None;
        }
    };

    let payload = match resp.json::<SevenTvGraphQlResponse<RespData>>().await {
        Ok(p) => p,
        Err(e) => {
            debug!("7TV user style parse failed for {twitch_user_id}: {e}");
            return None;
        }
    };

    if !payload.errors.is_empty() {
        let messages = payload
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        debug!("7TV user style GraphQL errors for {twitch_user_id}: {messages}");
    }

    let user_node = payload.data.and_then(|d| d.user_by_connection);

    let avatar_url = user_node
        .as_ref()
        .and_then(|u| u.avatar_url.clone())
        .filter(|s| !s.is_empty())
        .map(|u| {
            // 7TV sometimes returns protocol-relative URLs (//cdn.7tv.app/...)
            if u.starts_with("//") {
                format!("https:{u}")
            } else {
                u
            }
        });

    let style = user_node.and_then(|u| u.style).unwrap_or(StyleNode {
        color: 0,
        badge_id: None,
    });

    let mut out = SevenTvUserStyleRaw {
        color: if style.color == 0 {
            None
        } else {
            Some(style.color)
        },
        badge_id: style.badge_id.filter(|s| !s.is_empty()),
        avatar_url,
    };

    // Some 7TV schemas expose badge id under style.badge.id instead of
    // style.badge_id. When missing, try a compatible fallback query.
    if out.badge_id.is_none() {
        let query_v2 = r#"
            query($id: String!) {
                userByConnection(platform: TWITCH, id: $id) {
                    avatar_url
                    style {
                        color
                        badge {
                            id
                        }
                    }
                }
            }
        "#;

        if let Ok(resp2) = client
            .post(SEVENTV_GQL_URL)
            .json(&serde_json::json!({
                "query": query_v2,
                "variables": { "id": twitch_user_id }
            }))
            .send()
            .await
        {
            if let Ok(payload2) = resp2.json::<SevenTvGraphQlResponse<RespDataV2>>().await {
                if let Some(user2) = payload2.data.and_then(|d| d.user_by_connection) {
                    if out.avatar_url.is_none() {
                        out.avatar_url = user2.avatar_url.filter(|s| !s.is_empty()).map(|u| {
                            if u.starts_with("//") {
                                format!("https:{u}")
                            } else {
                                u
                            }
                        });
                    }
                    if out.color.is_none() {
                        out.color = user2.style.as_ref().and_then(|s| {
                            if s.color == 0 {
                                None
                            } else {
                                Some(s.color)
                            }
                        });
                    }
                    out.badge_id = user2
                        .style
                        .and_then(|s| s.badge)
                        .and_then(|b| b.id)
                        .filter(|s| !s.is_empty());
                }
            }
        }
    }

    Some(out)
}

fn seven_tv_color_to_rgba(color: i32) -> (u8, u8, u8, u8) {
    let raw = color as u32;
    let r = ((raw >> 24) & 0xFF) as u8;
    let g = ((raw >> 16) & 0xFF) as u8;
    let b = ((raw >> 8) & 0xFF) as u8;
    let a = (raw & 0xFF) as u8;
    // 7TV user colors are expected to be opaque in chat rendering.
    // Keep alpha=0 from turning names fully invisible.
    let a = if a == 0 { 255 } else { a };
    (r, g, b, a)
}

fn seven_tv_color_to_hex(color: i32) -> Option<String> {
    if color == 0 {
        return None;
    }
    let (r, g, b, a) = seven_tv_color_to_rgba(color);
    if a == 255 {
        Some(format!("#{r:02X}{g:02X}{b:02X}"))
    } else {
        Some(format!("#{r:02X}{g:02X}{b:02X}{a:02X}"))
    }
}

fn seven_tv_color_to_hex_with_alpha(color: i32) -> Option<String> {
    if color == 0 {
        return None;
    }
    let (r, g, b, a) = seven_tv_color_to_rgba(color);
    if a == 255 {
        Some(format!("#{r:02X}{g:02X}{b:02X}"))
    } else {
        Some(format!("#{r:02X}{g:02X}{b:02X}{a:02X}"))
    }
}

fn seven_tv_badge_url(host_url: &str, file_name: &str) -> String {
    let base = if host_url.starts_with("//") {
        format!("https:{host_url}")
    } else {
        host_url.to_owned()
    };
    format!("{}/{}", base.trim_end_matches('/'), file_name)
}

fn normalize_external_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{trimmed}"));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_owned());
    }
    None
}

fn choose_7tv_badge_file(files: &[SevenTvBadgeFile]) -> Option<&SevenTvBadgeFile> {
    files
        .iter()
        .find(|f| f.name.starts_with("2x."))
        .or_else(|| files.iter().find(|f| f.name.starts_with("1x.")))
        .or_else(|| files.first())
}

fn normalize_7tv_paint_stops(
    mut stops: Vec<SenderNamePaintStop>,
    fallback_color: Option<String>,
) -> Vec<SenderNamePaintStop> {
    stops.retain(|s| !s.color.is_empty() && s.at.is_finite());
    stops.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));

    if stops.is_empty() {
        if let Some(color) = fallback_color {
            return vec![
                SenderNamePaintStop {
                    at: 0.0,
                    color: color.clone(),
                },
                SenderNamePaintStop { at: 1.0, color },
            ];
        }
        return Vec::new();
    }

    // Preserve hard gradient edges by nudging duplicate or reversed
    // stop positions forward slightly.
    let mut last_stop = f32::NEG_INFINITY;
    for stop in &mut stops {
        if stop.at <= last_stop {
            stop.at = last_stop + 0.0000001;
        }
        last_stop = stop.at;
    }

    stops
}

#[cfg(test)]
mod tests {
    use super::{seven_tv_color_to_hex, seven_tv_color_to_hex_with_alpha};

    #[test]
    fn seven_tv_color_zero_is_treated_as_absent() {
        assert_eq!(seven_tv_color_to_hex(0), None);
        assert_eq!(seven_tv_color_to_hex_with_alpha(0), None);
    }

    #[test]
    fn seven_tv_color_alpha_zero_is_normalized_to_opaque() {
        // 0x11223300 should become opaque #112233.
        let c = 0x11223300u32 as i32;
        assert_eq!(seven_tv_color_to_hex(c), Some("#112233".to_owned()));
        assert_eq!(
            seven_tv_color_to_hex_with_alpha(c),
            Some("#112233".to_owned())
        );
    }
}
