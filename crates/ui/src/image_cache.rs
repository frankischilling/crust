//! Bounded image storage shared by chat, menus, avatars, and live thumbnails.
//!
//! Encoded assets use at most 128 MiB / 4,096 entries. First-frame textures use
//! 64 MiB / 1,024 entries. Paints share the background image decoder. The decoder and texture caches
//! each have a separate 128 MiB limit. Assets used in this pass are protected;
//! admission fails when making room would discard a visible texture.

mod loaders;

#[cfg(test)]
pub(crate) use loaders::tests::{
    animated_webp as animated_webp_fixture, gif as animated_gif_fixture,
};

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crust_core::events::AppCommand;

pub(crate) type ImageBytes = (u32, u32, Arc<[u8]>);

const RAW_BYTES: usize = 128 * 1024 * 1024;
const RAW_ENTRIES: usize = 4_096;
const DERIVED_BYTES: usize = 64 * 1024 * 1024;
const DERIVED_ENTRIES: usize = 1_024;
const LOADER_BYTES: usize = 128 * 1024 * 1024;
const REQUESTS_PER_FRAME: usize = 24;
const RETRY_DELAY: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Cumulative decoder and upload work; decode time runs on background workers.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ImageWorkStats {
    pub animation_bytes: usize,
    pub pending_decodes: usize,
    pub completed_decodes: u64,
    pub decode_micros: u64,
    pub texture_uploads: u64,
    pub texture_upload_micros: u64,
}

struct Entry {
    image: ImageBytes,
    generation: u64,
    used_frame: AtomicU64,
    used_order: AtomicU64,
}

struct MissingImage {
    seen_frame: u64,
    retry_at: Instant,
}

struct DerivedEntry {
    texture: egui::TextureHandle,
    used_frame: u64,
    used_order: u64,
    byte_len: usize,
}

struct DerivedCache {
    entries: HashMap<String, DerivedEntry>,
    byte_len: usize,
    bytes_limit: usize,
    entries_limit: usize,
}

impl Default for DerivedCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            byte_len: 0,
            bytes_limit: DERIVED_BYTES,
            entries_limit: DERIVED_ENTRIES,
        }
    }
}

impl DerivedCache {
    fn remove(&mut self, url: &str) {
        if let Some(entry) = self.entries.remove(url) {
            self.byte_len -= entry.byte_len;
        }
    }

    fn make_room(&mut self, bytes: usize, entries: usize, frame: u64) -> bool {
        if bytes > self.bytes_limit || entries > self.entries_limit {
            return false;
        }
        while self.byte_len.saturating_add(bytes) > self.bytes_limit
            || self.entries.len().saturating_add(entries) > self.entries_limit
        {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.used_frame != frame)
                .min_by_key(|(_, entry)| entry.used_order)
                .map(|(url, _)| url.clone());
            let Some(oldest) = oldest else { return false };
            self.remove(&oldest);
        }
        true
    }
}

pub struct UiImageCache {
    entries: HashMap<String, Entry>,
    byte_len: usize,
    frame: u64,
    order: AtomicU64,
    missing: Mutex<HashMap<String, MissingImage>>,
    derived: Mutex<DerivedCache>,
    raw_bytes_limit: usize,
    raw_entries_limit: usize,
    loaders_installed: bool,
}

impl Default for UiImageCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            byte_len: 0,
            frame: 1,
            order: AtomicU64::new(0),
            missing: Mutex::default(),
            derived: Mutex::default(),
            raw_bytes_limit: RAW_BYTES,
            raw_entries_limit: RAW_ENTRIES,
            loaders_installed: false,
        }
    }
}

impl UiImageCache {
    pub(crate) fn work_stats(&self, ctx: &egui::Context) -> ImageWorkStats {
        loaders::work_stats(ctx)
    }

    pub(crate) fn begin_frame(&mut self, ctx: &egui::Context) {
        if !self.loaders_installed {
            // Paints can request frames before the first headless UI pass.
            // Keep the bounded wrapper outermost when those loaders already exist.
            loaders::ensure_installed(ctx);
            self.loaders_installed = true;
        }
        self.frame = self.frame.wrapping_add(1).max(1);
        // Keep in-flight requests and failure cooldowns when a tab is hidden.
        // Returning to it must not immediately send the same request again.
        let now = Instant::now();
        self.missing.lock().unwrap().retain(|_, request| {
            self.frame.wrapping_sub(request.seen_frame) <= 2 || request.retry_at > now
        });
    }

    pub(crate) fn get(&self, url: &str) -> Option<&ImageBytes> {
        if let Some(entry) = self.entries.get(url) {
            entry.used_frame.store(self.frame, Ordering::Relaxed);
            entry.used_order.store(self.next_order(), Ordering::Relaxed);
            Some(&entry.image)
        } else {
            self.note_missing(url, Instant::now());
            None
        }
    }

    /// Read layout dimensions without fetching or retaining an offscreen image.
    pub(crate) fn peek(&self, url: &str) -> Option<&ImageBytes> {
        self.entries.get(url).map(|entry| &entry.image)
    }

    /// Inspect presence without requesting an off-screen prefetch or pinning it.
    #[cfg(test)]
    pub(crate) fn contains_key(&self, url: &str) -> bool {
        self.entries.contains_key(url)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.byte_len
    }

    fn next_order(&self) -> u64 {
        self.order.fetch_add(1, Ordering::Relaxed)
    }

    fn note_missing(&self, url: &str, now: Instant) {
        if url.trim().is_empty() {
            return;
        }
        let mut missing = self.missing.lock().unwrap();
        if let Some(request) = missing.get_mut(url) {
            request.seen_frame = self.frame;
            return;
        }
        if missing.len() >= RAW_ENTRIES {
            let oldest = missing
                .iter()
                .filter(|(_, request)| request.seen_frame != self.frame)
                .max_by_key(|(_, request)| self.frame.wrapping_sub(request.seen_frame))
                .map(|(url, _)| url.clone());
            let Some(oldest) = oldest else { return };
            missing.remove(&oldest);
        }
        missing.insert(
            url.to_owned(),
            MissingImage {
                seen_frame: self.frame,
                retry_at: now,
            },
        );
    }

    /// Settle every completion, including failures and responses evicted on arrival.
    pub(crate) fn complete(
        &mut self,
        ctx: &egui::Context,
        url: String,
        width: u32,
        height: u32,
        raw: Vec<u8>,
    ) -> bool {
        self.missing.lock().unwrap().remove(&url);
        if raw.is_empty() || width == 0 || height == 0 || raw.len() > self.raw_bytes_limit {
            self.delay_retry(&url, RETRY_DELAY);
            return false;
        }
        if let Some(entry) = self.entries.get(&url) {
            if entry.image.0 == width && entry.image.1 == height && entry.image.2.as_ref() == raw {
                return true;
            }
            if entry.used_frame.load(Ordering::Relaxed) == self.frame {
                self.delay_retry(&url, Duration::from_millis(250));
                return false;
            }
        }
        let existing_bytes = self
            .entries
            .get(&url)
            .map_or(0, |entry| entry.image.2.len());
        let additional_entries = usize::from(!self.entries.contains_key(&url));
        while self
            .byte_len
            .saturating_sub(existing_bytes)
            .saturating_add(raw.len())
            > self.raw_bytes_limit
            || self.entries.len().saturating_add(additional_entries) > self.raw_entries_limit
        {
            let oldest = self
                .entries
                .iter()
                .filter(|(key, entry)| {
                    key.as_str() != url && entry.used_frame.load(Ordering::Relaxed) != self.frame
                })
                .min_by_key(|(_, entry)| entry.used_order.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                self.delay_retry(&url, Duration::from_millis(250));
                return false;
            };
            self.remove(ctx, &oldest);
        }
        self.remove(ctx, &url);
        self.byte_len += raw.len();
        let order = self.next_order();
        self.entries.insert(
            url,
            Entry {
                image: (width, height, raw.into()),
                generation: order,
                used_frame: AtomicU64::new(0),
                used_order: AtomicU64::new(order),
            },
        );
        true
    }

    fn delay_retry(&self, url: &str, delay: Duration) {
        let mut missing = self.missing.lock().unwrap();
        if missing.len() < RAW_ENTRIES {
            missing.insert(
                url.to_owned(),
                MissingImage {
                    seen_frame: self.frame,
                    retry_at: Instant::now() + delay,
                },
            );
        }
    }

    fn remove(&mut self, ctx: &egui::Context, url: &str) {
        if let Some(entry) = self.entries.remove(url) {
            self.byte_len -= entry.image.2.len();
            // The bounded texture loader also forgets every animation-frame URI.
            let uri = crate::widgets::bytes_uri(url, &entry.image.2);
            ctx.forget_image(&uri);
            ctx.forget_image(&loaders::first_frame_uri(&uri));
            ctx.data_mut(|data| data.remove::<egui::FrameDurations>(egui::Id::new(&uri)));
        }
        self.derived.lock().unwrap().remove(url);
    }

    /// Requests only assets read by a visible widget. A full command queue leaves
    /// the request eligible; an accepted request can retry after a lost completion.
    pub(crate) fn end_frame(&self, ctx: &egui::Context, send: impl FnMut(AppCommand) -> bool) {
        self.flush_requests(ctx, Instant::now(), send);
    }

    fn flush_requests(
        &self,
        ctx: &egui::Context,
        now: Instant,
        mut send: impl FnMut(AppCommand) -> bool,
    ) {
        let mut missing = self.missing.lock().unwrap();
        let mut sent = 0;
        for (url, request) in missing
            .iter_mut()
            .filter(|(_, request)| request.seen_frame == self.frame)
        {
            if request.retry_at <= now {
                if sent == REQUESTS_PER_FRAME || !send(AppCommand::FetchImage { url: url.clone() })
                {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    break;
                }
                request.retry_at = now + REQUEST_TIMEOUT;
                sent += 1;
            }
            ctx.request_repaint_after(request.retry_at.saturating_duration_since(now));
        }
    }

    /// Share a decoded frame without decoding or allocating pixels on the UI thread.
    /// The identity changes when the source bytes or the animation frame change.
    pub(crate) fn color_frame(
        &self,
        url: &str,
        ctx: &egui::Context,
        animate: bool,
    ) -> Option<(Arc<egui::ColorImage>, u64)> {
        let (_, _, raw) = self.get(url)?;
        let uri = crate::widgets::bytes_uri(url, raw);
        let (image, frame) = loaders::color_frame(ctx, &uri, raw.clone(), animate)?;
        let mut identity = std::collections::hash_map::DefaultHasher::new();
        self.entries.get(url)?.generation.hash(&mut identity);
        frame.hash(&mut identity);
        Some((image, identity.finish()))
    }

    pub(crate) fn static_texture(
        &self,
        ctx: &egui::Context,
        url: &str,
    ) -> Option<egui::TextureHandle> {
        self.get(url)?;
        {
            let mut derived = self.derived.lock().unwrap();
            if let Some(entry) = derived.entries.get_mut(url) {
                entry.used_frame = self.frame;
                entry.used_order = self.next_order();
                return Some(entry.texture.clone());
            }
        }
        let (image, _) = self.color_frame(url, ctx, false)?;
        let byte_len = image
            .pixels
            .len()
            .saturating_mul(std::mem::size_of::<egui::Color32>());
        let mut derived = self.derived.lock().unwrap();
        if !derived.make_room(byte_len, 1, self.frame) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return None;
        }
        let texture = ctx.load_texture(
            format!("crust-static://{url}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        derived.entries.insert(
            url.to_owned(),
            DerivedEntry {
                texture: texture.clone(),
                used_frame: self.frame,
                used_order: self.next_order(),
                byte_len,
            },
        );
        derived.byte_len += byte_len;
        Some(texture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(bytes: usize, entries: usize) -> UiImageCache {
        UiImageCache {
            raw_bytes_limit: bytes,
            raw_entries_limit: entries,
            ..Default::default()
        }
    }

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn ready_static_texture(
        cache: &UiImageCache,
        ctx: &egui::Context,
        url: &str,
    ) -> egui::TextureHandle {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(texture) = cache.static_texture(ctx, url) {
                return texture;
            }
            assert!(
                Instant::now() < deadline,
                "first frame did not finish decoding"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn ready_color_frame(
        cache: &UiImageCache,
        ctx: &egui::Context,
        url: &str,
        animate: bool,
    ) -> (Arc<egui::ColorImage>, u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = cache.color_frame(url, ctx, animate) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "paint frame did not finish decoding"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn raw_cache_respects_bytes_and_entry_limits() {
        let ctx = egui::Context::default();
        let mut cache = cache(8, 2);
        for index in 0..1_000 {
            assert!(cache.complete(&ctx, index.to_string(), 1, 1, vec![1; 4]));
            assert!(cache.byte_len() <= 8);
            assert!(cache.len() <= 2);
        }
        assert!(!cache.contains_key("0"));
        assert!(cache.contains_key("999"));
        assert!(!cache.complete(&ctx, "oversized".to_owned(), 1, 1, vec![0; 9]));
    }

    #[test]
    fn visible_entries_are_pinned_and_evicted_assets_can_be_requested_again() {
        let ctx = egui::Context::default();
        let mut cache = cache(4, 1);
        cache.complete(&ctx, "first".to_owned(), 1, 1, vec![1; 4]);
        assert!(cache.get("first").is_some());
        assert!(!cache.complete(&ctx, "second".to_owned(), 1, 1, vec![2; 4]));
        assert!(cache.contains_key("first"));
        cache.begin_frame(&ctx);
        assert!(cache.complete(&ctx, "second".to_owned(), 1, 1, vec![2; 4]));
        assert!(cache.get("first").is_none());
        let mut requested = Vec::new();
        cache.end_frame(&ctx, |command| {
            requested.push(command);
            true
        });
        assert!(matches!(&requested[..], [AppCommand::FetchImage { url }] if url == "first"));
        cache.begin_frame(&ctx);
        assert!(cache.complete(&ctx, "first".to_owned(), 1, 1, vec![1; 4]));
        assert!(cache.get("first").is_some());
    }

    #[test]
    fn failed_queue_and_lost_completion_leave_requests_retryable() {
        let ctx = egui::Context::default();
        let cache = UiImageCache::default();
        assert!(cache.get("image").is_none());
        let now = Instant::now();
        cache.flush_requests(&ctx, now + Duration::from_millis(1), |_| false);
        let mut sent = 0;
        cache.flush_requests(&ctx, now + Duration::from_millis(2), |_| {
            sent += 1;
            true
        });
        cache.flush_requests(&ctx, now + Duration::from_millis(3), |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 1);
        cache.flush_requests(&ctx, now + REQUEST_TIMEOUT + Duration::from_secs(1), |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 2);
    }

    #[test]
    fn failed_completion_retries_after_cooldown() {
        let ctx = egui::Context::default();
        let mut cache = UiImageCache::default();
        cache.get("image");
        cache.end_frame(&ctx, |_| true);
        assert!(!cache.complete(&ctx, "image".to_owned(), 0, 0, Vec::new()));
        let mut sent = 0;
        cache.flush_requests(&ctx, Instant::now(), |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 0);
        cache.flush_requests(&ctx, Instant::now() + RETRY_DELAY, |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 1);
    }

    #[test]
    fn raw_eviction_releases_shared_first_frame_and_included_bytes() {
        use egui::load::{BytesLoader, BytesPoll};
        let ctx = egui::Context::default();
        let raw = png();
        let mut cache = cache(raw.len(), 1);
        cache.begin_frame(&ctx);
        cache.complete(&ctx, "first".to_owned(), 2, 1, raw.clone());
        let texture = ready_static_texture(&cache, &ctx, "first");
        assert_eq!(
            cache.static_texture(&ctx, "first").unwrap().id(),
            texture.id()
        );
        let uri = crate::widgets::bytes_uri("first", &raw);
        ctx.include_bytes(uri.clone(), raw.clone());
        assert!(matches!(
            ctx.try_load_bytes(&uri),
            Ok(BytesPoll::Ready { .. })
        ));
        assert_eq!(cache.derived.lock().unwrap().byte_len, 8);
        cache.begin_frame(&ctx);
        cache.complete(&ctx, "second".to_owned(), 2, 1, raw);
        assert!(cache.derived.lock().unwrap().entries.is_empty());
        assert_eq!(ctx.loaders().include.byte_size(), 0);
    }

    #[test]
    fn frozen_textures_share_a_budget_and_reload_after_eviction() {
        let ctx = egui::Context::default();
        let mut cache = UiImageCache::default();
        cache.derived.get_mut().unwrap().bytes_limit = 8;
        cache.derived.get_mut().unwrap().entries_limit = 1;
        cache.complete(&ctx, "first".to_owned(), 2, 1, png());
        cache.complete(&ctx, "second".to_owned(), 2, 1, png());
        let first = ready_static_texture(&cache, &ctx, "first").id();
        assert_eq!(cache.derived.lock().unwrap().byte_len, 8);
        assert!(cache.static_texture(&ctx, "second").is_none());
        assert_eq!(cache.static_texture(&ctx, "first").unwrap().id(), first);
        cache.begin_frame(&ctx);
        let _ = ready_static_texture(&cache, &ctx, "second");
        assert_eq!(cache.derived.lock().unwrap().entries.len(), 1);
        assert_eq!(cache.derived.lock().unwrap().byte_len, 8);
        cache.begin_frame(&ctx);
        assert_ne!(ready_static_texture(&cache, &ctx, "first").id(), first);
    }

    #[test]
    fn paint_frames_share_decoded_pixels_and_identity_tracks_animation_and_replacement() {
        let ctx = egui::Context::default();
        let mut cache = UiImageCache::default();
        cache.begin_frame(&ctx);
        cache.complete(&ctx, "paint".to_owned(), 2, 1, animated_gif_fixture());
        let mut first_identity = 0;
        let _ = ctx.run(
            egui::RawInput {
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                let (first, identity) = ready_color_frame(&cache, ctx, "paint", true);
                assert!(first
                    .pixels
                    .iter()
                    .all(|pixel| *pixel == egui::Color32::RED));
                first_identity = identity;
                let (again, again_identity) = ready_color_frame(&cache, ctx, "paint", true);
                assert_eq!(identity, again_identity);
                assert!(Arc::ptr_eq(&first, &again));
            },
        );
        let _ = ctx.run(
            egui::RawInput {
                time: Some(0.15),
                ..Default::default()
            },
            |ctx| {
                let (next, identity) = ready_color_frame(&cache, ctx, "paint", true);
                assert!(next
                    .pixels
                    .iter()
                    .all(|pixel| *pixel == egui::Color32::GREEN));
                assert_ne!(identity, first_identity);
                let (still, still_identity) = ready_color_frame(&cache, ctx, "paint", false);
                assert!(still
                    .pixels
                    .iter()
                    .all(|pixel| *pixel == egui::Color32::RED));
                assert_eq!(still_identity, first_identity);
                assert_eq!(ready_color_frame(&cache, ctx, "paint", true).1, identity);
            },
        );
        cache.begin_frame(&ctx);
        assert!(cache.complete(&ctx, "paint".to_owned(), 2, 1, png()));
        let (_, replacement_identity) = ready_color_frame(&cache, &ctx, "paint", false);
        assert_ne!(replacement_identity, first_identity);
    }

    #[test]
    fn first_frame_color_sampling_accepts_rgb16_and_rgba16_pngs() {
        let rgb = image::DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgb([u16::MAX, 32_768, 0]),
        ));
        let rgba = image::DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgba([u16::MAX, 0, 32_768, 32_768]),
        ));
        for (name, source, native_bytes, expected) in [
            ("rgb16", rgb, 12, [255u8, 128, 0, 255]),
            ("rgba16", rgba, 16, [255u8, 0, 128, 128]),
        ] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            source
                .write_to(&mut encoded, image::ImageFormat::Png)
                .unwrap();
            let raw = encoded.into_inner();
            assert_eq!(
                loaders::preflight_size(&raw, native_bytes, true).unwrap(),
                native_bytes
            );
            assert!(loaders::preflight_size(&raw, native_bytes - 1, true).is_err());

            let ctx = egui::Context::default();
            let mut cache = UiImageCache::default();
            cache.begin_frame(&ctx);
            assert!(cache.complete(&ctx, name.to_owned(), 2, 1, raw));
            let (decoded, _) = ready_color_frame(&cache, &ctx, name, false);
            assert_eq!(decoded.size, [2, 1]);
            for pixel in &decoded.pixels {
                for (actual, expected) in pixel.to_srgba_unmultiplied().into_iter().zip(expected) {
                    assert!(
                        (i16::from(actual) - i16::from(expected)).abs() <= 1,
                        "{name}: unexpected converted pixel {pixel:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hidden_tabs_preserve_inflight_requests_and_failure_cooldowns() {
        let ctx = egui::Context::default();
        let mut cache = UiImageCache::default();
        cache.get("image");
        let mut sent = 0;
        cache.end_frame(&ctx, |_| {
            sent += 1;
            true
        });
        for _ in 0..10 {
            cache.begin_frame(&ctx);
        }
        cache.get("image");
        cache.end_frame(&ctx, |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 1, "switching tabs must retain an in-flight request");
        cache.complete(&ctx, "image".to_owned(), 0, 0, Vec::new());
        for _ in 0..10 {
            cache.begin_frame(&ctx);
        }
        cache.get("image");
        cache.end_frame(&ctx, |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 1, "switching tabs must retain a failure cooldown");
        cache.flush_requests(&ctx, Instant::now() + RETRY_DELAY, |_| {
            sent += 1;
            true
        });
        assert_eq!(sent, 2);
    }

    #[test]
    fn missing_metadata_stays_bounded_and_hidden_widgets_do_not_retry() {
        let ctx = egui::Context::default();
        let mut cache = UiImageCache::default();
        for index in 0..RAW_ENTRIES + 100 {
            cache.get(&index.to_string());
        }
        assert_eq!(cache.missing.lock().unwrap().len(), RAW_ENTRIES);
        cache.begin_frame(&ctx);
        cache.end_frame(&ctx, |_| panic!("hidden asset requested"));
        cache.begin_frame(&ctx);
        cache.begin_frame(&ctx);
        assert!(cache.missing.lock().unwrap().is_empty());
    }

    #[test]
    #[ignore = "isolated cold still-image decode timing"]
    fn perf_first_frame_cold_load() {
        let pixels = image::RgbaImage::from_fn(1024, 1024, |x, y| {
            image::Rgba([
                ((x * 31 + y * 17) % 256) as u8,
                ((x * 11 + y * 47) % 256) as u8,
                ((x * 23 + y * 7) % 256) as u8,
                255,
            ])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let raw = encoded.into_inner();
        let mut synchronous = Vec::new();
        let mut first_polls = Vec::new();
        let mut completions = Vec::new();
        for _ in 0..7 {
            let started = Instant::now();
            let decoded = image::load_from_memory(&raw).unwrap().into_rgba8();
            std::hint::black_box(egui::ColorImage::from_rgba_unmultiplied(
                [1024, 1024],
                &decoded,
            ));
            synchronous.push(started.elapsed().as_secs_f64() * 1000.0);

            let ctx = egui::Context::default();
            let mut cache = UiImageCache::default();
            cache.begin_frame(&ctx);
            cache.complete(&ctx, "cold".to_owned(), 1024, 1024, raw.clone());
            let started = Instant::now();
            std::hint::black_box(cache.color_frame("cold", &ctx, false));
            first_polls.push(started.elapsed().as_secs_f64() * 1000.0);
            let (image, _) = ready_color_frame(&cache, &ctx, "cold", false);
            assert_eq!(image.size, [1024, 1024]);
            completions.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        synchronous.sort_by(f64::total_cmp);
        first_polls.sort_by(f64::total_cmp);
        completions.sort_by(f64::total_cmp);
        eprintln!(
            "cold PNG 1024x1024, encoded_bytes={}, synchronous_decode_ms={synchronous:?}, first_poll_ms={first_polls:?}, complete_ms={completions:?}",
            raw.len(),
        );
    }
}
