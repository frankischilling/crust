use std::collections::HashMap;

use crust_core::model::{Badge, Sender, SenderNamePaint, SenderNamePaintShadow, SenderNamePaintStop};
use crust_emotes::{
    providers::{read_bounded_json, SevenTvProvider},
    EmoteInfo,
};
use tracing::{debug, info, warn};

const SEVENTV_GQL_URL: &str = "https://api.7tv.app/v3/gql";

#[derive(Debug, Clone, Default)]
pub(crate) struct SevenTvUserStyleRaw {
    pub(crate) color: Option<i32>,
    pub(crate) badge_id: Option<String>,
    pub(crate) paint_id: Option<String>,
    pub(crate) avatar_url: Option<String>,
}

pub(crate) struct SevenTvUserProfile {
    pub(crate) style: SevenTvUserStyleRaw,
    pub(crate) personal_emotes: Option<Vec<EmoteInfo>>,
}

#[derive(Debug, Clone)]
pub(crate) struct SevenTvBadgeMeta {
    pub(crate) tooltip: Option<String>,
    pub(crate) url: String,
}

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
    pub(crate) name_paint: Option<SenderNamePaint>,
    pub(crate) avatar_url: Option<String>,
}

pub(crate) enum SevenTvCosmeticUpdate {
    Catalog {
        badges: HashMap<String, SevenTvBadgeMeta>,
        paints: HashMap<String, SevenTvPaintMeta>,
    },
    UserStyle {
        twitch_user_id: String,
        generation: u64,
        profile: Option<SevenTvUserProfile>,
    },
    /// Batch of Twitch user-ids discovered in history messages that need
    /// their 7TV styles resolved.
    BatchUserLookup {
        channel: crust_core::ChannelId,
        user_ids: Vec<String>,
    },
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

impl<T> SevenTvGraphQlResponse<T> {
    fn successful_data(self) -> Option<T> {
        if !self.errors.is_empty() {
            let messages = self.errors.iter().map(|error| error.message.as_str())
                .collect::<Vec<_>>().join(" | ");
            warn!("7TV cosmetics GraphQL errors: {messages}");
            return None;
        }
        self.data
    }
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvBadgeFile {
    name: String,
}

pub(crate) fn resolve_7tv_user_style(
    style: &SevenTvUserStyleRaw,
    badges: &HashMap<String, SevenTvBadgeMeta>,
    paints: &HashMap<String, SevenTvPaintMeta>,
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

    let name_paint = style.paint_id.as_ref().and_then(|id| paints.get(id))
        .map(|paint| SenderNamePaint {
            function: paint.function.clone(),
            angle: paint.angle,
            repeat: paint.repeat,
            image_url: paint.image_url.clone(),
            shadows: paint.shadows.clone(),
            stops: paint.stops.clone(),
        });

    SevenTvResolvedStyle {
        color_hex,
        badge,
        name_paint,
        avatar_url: style.avatar_url.clone(),
    }
}

pub(crate) fn apply_7tv_cosmetics_to_sender(sender: &mut Sender, style: &SevenTvResolvedStyle) {
    if let Some(ref color) = style.color_hex {
        sender.color = Some(color.clone());
    }
    sender.name_paint = style.name_paint.clone();

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
    } else {
        sender.badges.retain(|badge| !badge.name.eq_ignore_ascii_case("7tv"));
    }
}

/// Only a current profile response can replace a sender's cosmetics.
pub(crate) fn complete_7tv_user_profile(
    index: &crate::EmoteIndex,
    twitch_user_id: &str,
    generation: u64,
    profile: Option<SevenTvUserProfile>,
) -> Option<(crate::runtime::emote_registry::UserEmoteUpdate, Option<SevenTvUserStyleRaw>)> {
    let (style, emotes) = profile.map(|profile| (Some(profile.style), profile.personal_emotes))
        .unwrap_or_default();
    let update = index.write().unwrap()
        .complete_personal_load(twitch_user_id, generation, emotes)?;
    Some((update, style))
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
        badges: Vec<BadgeNode>,
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
        .and_then(reqwest::Response::error_for_status)
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

    let Some(data) = payload.successful_data() else {
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

pub(crate) async fn load_7tv_user_for_twitch(
    client: &reqwest::Client,
    twitch_user_id: &str,
) -> Option<SevenTvUserProfile> {
    if twitch_user_id.is_empty() {
        return None;
    }
    let query = r#"
        query($id: String!) {
            userByConnection(platform: TWITCH, id: $id) {
                avatar_url
                style { color badge_id paint_id }
                emote_sets { id flags }
            }
        }
    "#;
    let response = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({"query": query, "variables": {"id": twitch_user_id}}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(response) => response,
        Err(error) => {
            debug!("7TV user lookup failed for {twitch_user_id}: {error}");
            return None;
        }
    };
    let response = read_bounded_json::<serde_json::Value>(response, 2 * 1024 * 1024).await?;
    if response
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        debug!("7TV user lookup returned GraphQL errors for {twitch_user_id}");
        return None;
    }
    let user = response.get("data")?.get("userByConnection")?;
    if user.is_null() {
        return Some(SevenTvUserProfile {
            style: SevenTvUserStyleRaw::default(),
            personal_emotes: Some(Vec::new()),
        });
    }
    let style = parse_7tv_user_style(user)?;
    let personal_emotes = SevenTvProvider::with_client(client.clone())
        .load_personal_from_user(user)
        .await;
    Some(SevenTvUserProfile {
        style,
        personal_emotes,
    })
}

fn parse_7tv_user_style(response: &serde_json::Value) -> Option<SevenTvUserStyleRaw> {
    let user = response.get("user").unwrap_or(response).as_object()?;
    let style = match user.get("style")? {
        serde_json::Value::Object(style) => Some(style),
        serde_json::Value::Null => None,
        _ => return None,
    };
    let color = style
        .and_then(|style| style.get("color"))
        .and_then(serde_json::Value::as_i64)
        .map(|color| color as i32)
        .filter(|color| *color != 0);
    let badge_id = style
        .and_then(|style| {
            style
                .get("badge_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| style.get("badge")?.get("id")?.as_str())
        })
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let paint = style.and_then(|style| style.get("paint_id")
        .or_else(|| style.get("paint")?.get("id")));
    if style.is_some() && paint.is_none() {
        return None;
    }
    let paint_id = match paint {
        Some(serde_json::Value::String(id)) => (!id.trim().is_empty()).then(|| id.trim().to_owned()),
        Some(serde_json::Value::Null) | None => None,
        _ => return None,
    };
    let avatar_url = user
        .get("avatar_url")
        .and_then(serde_json::Value::as_str)
        .and_then(normalize_external_url);
    Some(SevenTvUserStyleRaw {
        color,
        badge_id,
        paint_id,
        avatar_url,
    })
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
    use super::*;

    #[test]
    fn user_response_supplies_cosmetics_alongside_personal_set_metadata() {
        let profile = serde_json::json!({
            "style": {"color": 0x11223300u32, "badge_id": "supporter", "paint_id": "selected"},
            "avatar_url": "//cdn.7tv.app/avatar/example/1x.webp",
            "emote_sets": [{"id": "personal", "flags": 4}]
        });
        let style = parse_7tv_user_style(&profile).unwrap();
        assert_eq!(style.color, Some(0x11223300));
        assert_eq!(style.badge_id.as_deref(), Some("supporter"));
        assert_eq!(style.paint_id.as_deref(), Some("selected"));
        assert_eq!(
            style.avatar_url.as_deref(),
            Some("https://cdn.7tv.app/avatar/example/1x.webp")
        );
        let fallback = parse_7tv_user_style(&serde_json::json!({"user": {
            "style": {"color": 0, "badge": {"id": "fallback"}, "paint": {"id": "nested"}}
        }}))
        .unwrap();
        assert!(fallback.color.is_none());
        assert_eq!(fallback.badge_id.as_deref(), Some("fallback"));
        assert_eq!(fallback.paint_id.as_deref(), Some("nested"));
        assert!(parse_7tv_user_style(&serde_json::json!({"user": null})).is_none());
    }

    #[test]
    fn selected_paint_resolves_and_successful_removal_clears_it() {
        let paints = HashMap::from([("selected".to_owned(), SevenTvPaintMeta {
            function: "url".to_owned(),
            angle: Some(45.0),
            repeat: true,
            image_url: Some("https://example.test/paint.webp".to_owned()),
            shadows: vec![SenderNamePaintShadow {
                x_offset: 1.0, y_offset: 2.0, radius: 3.0, color: "#112233FF".to_owned(),
            }],
            stops: vec![SenderNamePaintStop { at: 0.0, color: "#123456".to_owned() }],
        })]);
        let badges = HashMap::from([("selected-badge".to_owned(), SevenTvBadgeMeta {
            tooltip: Some("Selected badge".to_owned()),
            url: "https://example.test/badge.webp".to_owned(),
        })]);
        let selected = parse_7tv_user_style(&serde_json::json!({
            "style": {"paint_id": "selected", "badge_id": "selected-badge"}
        })).unwrap();
        let mut sender = Sender {
            user_id: crust_core::UserId("42".to_owned()), login: "sender".to_owned(),
            display_name: "Sender".to_owned(), original_display_name: None,
            color: Some("#ABCDEF".to_owned()), name_paint: None,
            badges: vec![
                Badge { name: "moderator".to_owned(), version: "1".to_owned(), url: None },
                Badge { name: "bttv".to_owned(), version: "1".to_owned(), url: None },
            ],
        };
        apply_7tv_cosmetics_to_sender(&mut sender, &resolve_7tv_user_style(&selected, &badges, &paints));
        let paint = sender.name_paint.as_ref().unwrap();
        assert_eq!(paint.function, "url");
        assert_eq!(paint.image_url.as_deref(), Some("https://example.test/paint.webp"));
        assert_eq!(paint.angle, Some(45.0));
        assert!(paint.repeat);
        assert_eq!(paint.shadows[0].radius, 3.0);
        assert_eq!(paint.stops[0].color, "#123456");
        assert_eq!(sender.badges[0].name, "7tv");
        let removed = parse_7tv_user_style(&serde_json::json!({"style": {"paint_id": null}})).unwrap();
        apply_7tv_cosmetics_to_sender(&mut sender, &resolve_7tv_user_style(&removed, &badges, &paints));
        assert!(sender.name_paint.is_none());
        assert_eq!(sender.color.as_deref(), Some("#ABCDEF"));
        assert_eq!(sender.badges.iter().map(|badge| badge.name.as_str()).collect::<Vec<_>>(),
            ["moderator", "bttv"]);
        assert!(parse_7tv_user_style(&serde_json::json!({"style": {"paint_id": 123}})).is_none());
        assert!(parse_7tv_user_style(&serde_json::json!({"style": {"badge_id": null}})).is_none());
        assert!(parse_7tv_user_style(&serde_json::json!({"style": "invalid"})).is_none());
    }

    #[test]
    fn stale_and_failed_profiles_cannot_replace_current_paint_metadata() {
        let index: crate::EmoteIndex = std::sync::Arc::new(std::sync::RwLock::new(
            crate::runtime::emote_registry::EmoteRegistry::default()));
        let (stale, current) = {
            let mut registry = index.write().unwrap();
            registry.set_account(Some("42"));
            let stale = registry.begin_personal_load("42", None).unwrap();
            registry.set_account(None);
            registry.set_account(Some("42"));
            (stale, registry.begin_personal_load("42", None).unwrap())
        };
        let profile = |paint: &str| Some(SevenTvUserProfile {
            style: SevenTvUserStyleRaw { paint_id: Some(paint.to_owned()), ..Default::default() },
            personal_emotes: Some(Vec::new()),
        });
        assert!(complete_7tv_user_profile(&index, "42", stale, profile("old")).is_none());
        let (_, style) = complete_7tv_user_profile(&index, "42", current, profile("current")).unwrap();
        assert_eq!(style.unwrap().paint_id.as_deref(), Some("current"));
        assert!(complete_7tv_user_profile(&index, "42", stale, profile("old")).is_none());

        let retry = {
            let mut registry = index.write().unwrap();
            registry.set_account(Some("99"));
            registry.begin_personal_load("99", None).unwrap()
        };
        let (_, failed_style) = complete_7tv_user_profile(&index, "99", retry, None).unwrap();
        assert!(failed_style.is_none(), "failed requests must not emit a cosmetic replacement");
    }

    #[test]
    fn partial_catalog_errors_do_not_replace_the_last_catalog() {
        let response: SevenTvGraphQlResponse<serde_json::Value> = serde_json::from_value(
            serde_json::json!({"data": {"cosmetics": {"paints": []}},
                "errors": [{"message": "paint lookup unavailable"}]})).unwrap();
        assert!(response.successful_data().is_none());
        let empty: SevenTvGraphQlResponse<serde_json::Value> = serde_json::from_value(
            serde_json::json!({"data": {"cosmetics": {"paints": []}}})).unwrap();
        assert!(empty.successful_data().is_some());
    }

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
