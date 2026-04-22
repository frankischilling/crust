pub mod account_switcher;
pub mod analytics;
pub mod channel_list;
pub mod chat_input;
pub mod chrome;
pub mod emoji_list;
pub mod emote_picker;
pub mod global_search;
pub mod info_bars;
pub mod irc_status;
pub mod join_dialog;
pub mod loading_screen;
pub mod login_dialog;
pub mod message_list;
pub mod message_search;
pub mod plugin_ui;
pub mod settings_page;
pub mod split_header;
pub mod user_profile_popup;

/// Build a `bytes://` URI for egui image loading.
///
/// egui's `ImageCrateLoader` determines the image format from the URI's file
/// extension.  Many CDN URLs (Twitch, Twemoji, …) either lack an extension or
/// lie about the real format (e.g. the Twitch CDN may serve WebP even from a
/// path that has no extension at all).  We therefore sniff the actual format
/// from the first few magic bytes and append the correct extension hint so
/// `image::load_from_memory_with_format` succeeds.
pub(crate) fn bytes_uri(url: &str, raw: &[u8]) -> String {
    let ext = sniff_image_ext(raw);
    let ext = if ext.is_empty() {
        // If the bytes are not identifiable, fall back to URL-based hints.
        // Twitch CDN often serves WebP for newer emotes.
        if url.contains("static-cdn.jtvnw.net/emoticons/v2/") {
            ".webp"
        } else {
            ".png"
        }
    } else {
        ext
    };
    format!("bytes://{url}{ext}")
}

/// Detect the image format from leading magic bytes.
fn sniff_image_ext(raw: &[u8]) -> &'static str {
    if raw.starts_with(b"\x89PNG") {
        ".png"
    } else if raw.len() >= 12 && raw.starts_with(b"RIFF") && &raw[8..12] == b"WEBP" {
        ".webp"
    } else if raw.starts_with(b"GIF8") {
        ".gif"
    } else if raw.starts_with(b"\xff\xd8\xff") {
        ".jpg"
    } else {
        // No recognised magic → omit extension so egui falls back to
        // `image::load_from_memory` which does its own auto-detection.
        ""
    }
}
