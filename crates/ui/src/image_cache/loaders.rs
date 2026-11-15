//! Keep egui's decoded images and uploaded textures within separate budgets.
//!
//! Animation decoders own a source URI, while textures own individual frame
//! URIs. Normalizing that relationship makes forgetting a source release every
//! uploaded frame, rather than leaving old animation textures resident.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod animation;

use egui::load::{
    ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint, SizedTexture, TextureLoadResult,
    TextureLoader, TexturePoll,
};
use image::ImageDecoder;

const IMAGE_LOADER_ID: &str = "crust::bounded_image_loader";
const TEXTURE_LOADER_ID: &str = "crust::bounded_texture_loader";
const METRICS_ID: &str = "crust::image_work_metrics";
const FIRST_FRAME_PREFIX: &str = "bytes://crust-first-frame/";

#[derive(Default)]
struct LoaderMetrics {
    animation_bytes: AtomicUsize,
    pending_decodes: AtomicUsize,
    completed_decodes: AtomicU64,
    decode_micros: AtomicU64,
    texture_uploads: AtomicU64,
    texture_upload_micros: AtomicU64,
}

pub(super) fn work_stats(ctx: &egui::Context) -> super::ImageWorkStats {
    let Some(metrics) =
        ctx.data(|data| data.get_temp::<Arc<LoaderMetrics>>(egui::Id::new(METRICS_ID)))
    else {
        return super::ImageWorkStats::default();
    };
    super::ImageWorkStats {
        animation_bytes: metrics.animation_bytes.load(Ordering::Relaxed),
        pending_decodes: metrics.pending_decodes.load(Ordering::Relaxed),
        completed_decodes: metrics.completed_decodes.load(Ordering::Relaxed),
        decode_micros: metrics.decode_micros.load(Ordering::Relaxed),
        texture_uploads: metrics.texture_uploads.load(Ordering::Relaxed),
        texture_upload_micros: metrics.texture_upload_micros.load(Ordering::Relaxed),
    }
}

type Decoder = Arc<dyn ImageLoader + Send + Sync>;

fn source_uri(uri: &str) -> &str {
    egui::decode_animated_image_uri(uri).map_or(uri, |(source, _)| source)
}

fn frame(ctx: &egui::Context) -> u64 {
    // All viewports share the loaders; protect textures used by the root pass
    // while immediate child viewports are being painted too.
    ctx.cumulative_pass_nr_for(egui::ViewportId::ROOT)
}

fn full_cache_error() -> LoadError {
    LoadError::Loading("Image cache is full; this image will retry on the next pass".to_owned())
}

pub(super) fn ensure_installed(ctx: &egui::Context) {
    if ctx
        .loaders()
        .image
        .lock()
        .iter()
        .any(|loader| loader.id() == IMAGE_LOADER_ID)
    {
        return;
    }
    egui_extras::install_image_loaders(ctx);
    install(ctx, super::LOADER_BYTES, super::RAW_ENTRIES);
}

pub(super) fn install(ctx: &egui::Context, bytes: usize, entries: usize) {
    let loaders = ctx.loaders();
    let mut images = loaders.image.lock();
    if images.iter().any(|loader| loader.id() == IMAGE_LOADER_ID) {
        return;
    }
    let metrics = Arc::new(LoaderMetrics::default());
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(METRICS_ID), metrics.clone()));
    let mut inner = std::mem::take(&mut *images);
    inner.push(Arc::new(animation::AnimationLoader::new(
        bytes,
        entries,
        metrics.clone(),
    )));
    images.push(Arc::new(BoundedImageLoader {
        inner,
        cache: Mutex::default(),
        bytes_limit: bytes,
        entries_limit: entries,
    }));
    drop(images);
    // The default loader stays available for unsupported custom sources. It has
    // no cached data at installation; our loader handles normal image uploads.
    ctx.add_texture_loader(Arc::new(BoundedTextureLoader {
        cache: Mutex::default(),
        bytes_limit: bytes,
        entries_limit: entries,
        metrics,
    }));
}

pub(super) fn color_frame(
    ctx: &egui::Context,
    source: &str,
    raw: Arc<[u8]>,
    animate: bool,
) -> Option<(Arc<egui::ColorImage>, usize)> {
    ensure_installed(ctx);
    let animation_format = matches!(
        image::guess_format(&raw),
        Ok(image::ImageFormat::Gif | image::ImageFormat::WebP)
    );
    let animated = animate && animation_format;
    let source = if animation_format && !animate {
        first_frame_uri(source)
    } else {
        source.to_owned()
    };
    ctx.include_bytes(source.clone(), raw);
    let mut frame = if animated {
        animation::frame_index(ctx, &source)
    } else {
        0
    };
    let uri = if animation_format {
        format!("{source}#{frame}")
    } else {
        source.clone()
    };
    let ImagePoll::Ready { mut image } = ctx.try_load_image(&uri, SizeHint::default()).ok()? else {
        return None;
    };
    if animated {
        let current = animation::frame_index(ctx, &source);
        if current != frame {
            let ImagePoll::Ready {
                image: current_image,
            } = ctx
                .try_load_image(&format!("{source}#{current}"), SizeHint::default())
                .ok()?
            else {
                return None;
            };
            image = current_image;
            frame = current;
        }
    }
    Some((image, frame))
}

pub(super) fn first_frame_uri(source: &str) -> String {
    format!("{FIRST_FRAME_PREFIX}{source}")
}

struct DecodedEntry {
    decoder: usize,
    pending: bool,
    rejected: Option<String>,
    used_frame: u64,
    used_order: u64,
}

#[derive(Default)]
struct DecodedCache {
    entries: HashMap<String, DecodedEntry>,
    bytes: usize,
    order: u64,
}

struct BoundedImageLoader {
    inner: Vec<Decoder>,
    cache: Mutex<DecodedCache>,
    bytes_limit: usize,
    entries_limit: usize,
}

impl BoundedImageLoader {
    fn inner_bytes(&self) -> usize {
        self.inner.iter().map(|decoder| decoder.byte_size()).sum()
    }

    fn forget_source(&self, uri: &str, cache: &mut DecodedCache) {
        for decoder in &self.inner {
            decoder.forget(uri);
        }
        cache.entries.remove(uri);
        cache.bytes = self.inner_bytes();
    }
}

impl ImageLoader for BoundedImageLoader {
    fn id(&self) -> &str {
        IMAGE_LOADER_ID
    }

    fn load(&self, ctx: &egui::Context, uri: &str, size_hint: SizeHint) -> ImageLoadResult {
        let source = source_uri(uri);
        let pass = frame(ctx);
        let mut cache = self.cache.lock().unwrap();
        cache.order = cache.order.wrapping_add(1);
        let order = cache.order;
        let mut selected = None;
        if let Some(entry) = cache.entries.get_mut(source) {
            entry.used_frame = pass;
            entry.used_order = order;
            if let Some(reason) = &entry.rejected {
                return Err(LoadError::Loading(reason.clone()));
            }
            let decoder = &self.inner[entry.decoder];
            if !entry.pending || decoder.id() == animation::LOADER_ID {
                let result = decoder.load(ctx, uri, size_hint);
                if !entry.pending && matches!(result, Ok(ImagePoll::Ready { .. })) {
                    // Decoder byte_size walks its entire cache. Avoid that work for
                    // ordinary repaint hits, including later frames of an animation.
                    return result;
                }
                // The background decoder preflights its own queued work. Other
                // pending loaders need the wrapper check when their bytes arrive.
                if !matches!(
                    result,
                    Err(LoadError::NotSupported | LoadError::FormatNotSupported { .. })
                ) {
                    selected = Some((entry.decoder, result));
                }
            }
        }
        let mut detected_format = None;
        if selected.is_none() {
            // GIF/WebP loaders collect an entire animation before returning. Check
            // canvas size and frame count before they can allocate those buffers.
            if let Ok(egui::load::BytesPoll::Ready { bytes, .. }) = ctx.try_load_bytes(source) {
                if let Err(reason) = preflight_size(
                    &bytes,
                    self.bytes_limit,
                    source.starts_with(FIRST_FRAME_PREFIX),
                ) {
                    if cache.entries.len() < self.entries_limit {
                        cache.entries.insert(
                            source.to_owned(),
                            DecodedEntry {
                                decoder: 0,
                                pending: false,
                                rejected: Some(reason.clone()),
                                used_frame: pass,
                                used_order: order,
                            },
                        );
                    }
                    return Err(LoadError::Loading(reason));
                }
            }
            for (index, decoder) in self.inner.iter().enumerate().rev() {
                match decoder.load(ctx, uri, size_hint) {
                    Err(LoadError::NotSupported) => continue,
                    Err(LoadError::FormatNotSupported {
                        detected_format: format,
                    }) => {
                        detected_format = detected_format.or(format);
                    }
                    result => {
                        selected = Some((index, result));
                        break;
                    }
                }
            }
        }
        let Some((decoder, result)) = selected else {
            return Err(LoadError::FormatNotSupported { detected_format });
        };
        cache.entries.insert(
            source.to_owned(),
            DecodedEntry {
                decoder,
                pending: matches!(result, Ok(ImagePoll::Pending { .. })),
                rejected: None,
                used_frame: pass,
                used_order: order,
            },
        );
        cache.bytes = self.inner_bytes();
        while cache.bytes > self.bytes_limit || cache.entries.len() > self.entries_limit {
            let oldest = cache
                .entries
                .iter()
                .filter(|(_, entry)| entry.used_frame != pass)
                .min_by_key(|(_, entry)| entry.used_order)
                .map(|(uri, _)| uri.clone());
            let Some(oldest) = oldest else {
                self.forget_source(source, &mut cache);
                ctx.data_mut(|data| data.remove::<egui::FrameDurations>(egui::Id::new(source)));
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return Err(full_cache_error());
            };
            self.forget_source(&oldest, &mut cache);
            ctx.data_mut(|data| data.remove::<egui::FrameDurations>(egui::Id::new(&oldest)));
        }
        result
    }

    fn forget(&self, uri: &str) {
        self.forget_source(source_uri(uri), &mut self.cache.lock().unwrap());
    }

    fn forget_all(&self) {
        let mut cache = self.cache.lock().unwrap();
        for decoder in &self.inner {
            decoder.forget_all();
        }
        *cache = DecodedCache::default();
    }

    fn byte_size(&self) -> usize {
        self.cache.lock().unwrap().bytes
    }
}

fn preflight_decoded_size(raw: &[u8], limit: usize) -> Result<usize, String> {
    preflight_size(raw, limit, false)
}

pub(super) fn preflight_size(raw: &[u8], limit: usize, first_frame: bool) -> Result<usize, String> {
    let Ok(format) = image::guess_format(raw) else {
        return Ok(0);
    };
    let decoder = image::ImageReader::with_format(std::io::Cursor::new(raw), format)
        .into_decoder()
        .map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    let frame_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "Image dimensions exceed the decoded byte limit".to_owned())?;
    if frame_bytes > limit {
        return Err("Image dimensions exceed the decoded byte limit".to_owned());
    }
    if first_frame {
        // PNG can decode to 16-bit RGB/RGBA before conversion to display pixels.
        // Reserve that native output too, then release the extra bytes on completion.
        let native_bytes = usize::try_from(decoder.total_bytes())
            .map_err(|_| "Image dimensions exceed the decoded byte limit".to_owned())?;
        let decode_bytes = frame_bytes.max(native_bytes);
        if decode_bytes > limit {
            return Err("Image dimensions exceed the decoded byte limit".to_owned());
        }
        match format {
            image::ImageFormat::Gif => {
                gif_frame_count(raw, 0)?;
            }
            image::ImageFormat::WebP => {
                webp_frame_count(raw, 0, (width, height))?;
            }
            _ => {}
        }
        return Ok(decode_bytes);
    }
    let max_frames = limit.checked_div(frame_bytes.max(1)).unwrap_or(0);
    let frames = match format {
        image::ImageFormat::Gif => gif_frame_count(raw, max_frames)?,
        image::ImageFormat::WebP => webp_frame_count(raw, max_frames, (width, height))?,
        _ => 1,
    };
    if frames > max_frames {
        return Err("Animation exceeds the decoded byte limit".to_owned());
    }
    Ok(frame_bytes * frames)
}

fn gif_frame_count(raw: &[u8], max_frames: usize) -> Result<usize, String> {
    let invalid = || "Invalid GIF container".to_owned();
    let screen = raw.get(6..10).ok_or_else(invalid)?;
    let width = u16::from_le_bytes(screen[..2].try_into().unwrap()) as u32;
    let height = u16::from_le_bytes(screen[2..].try_into().unwrap()) as u32;
    let packed = *raw.get(10).ok_or_else(invalid)?;
    let mut cursor = 13usize;
    if packed & 0x80 != 0 {
        cursor += 3usize << ((packed & 7) + 1);
    }
    let mut frames = 0;
    loop {
        let marker = *raw.get(cursor).ok_or_else(invalid)?;
        cursor += 1;
        match marker {
            0x3b => return Ok(frames.max(1)),
            0x21 => {
                cursor += 1; // extension label; payload is a sequence of sub-blocks
                skip_gif_blocks(raw, &mut cursor)?;
            }
            0x2c => {
                let descriptor = raw.get(cursor..cursor + 9).ok_or_else(invalid)?;
                let left = u16::from_le_bytes(descriptor[..2].try_into().unwrap()) as u32;
                let top = u16::from_le_bytes(descriptor[2..4].try_into().unwrap()) as u32;
                let frame_width = u16::from_le_bytes(descriptor[4..6].try_into().unwrap()) as u32;
                let frame_height = u16::from_le_bytes(descriptor[6..8].try_into().unwrap()) as u32;
                if left + frame_width > width || top + frame_height > height {
                    return Err("GIF frame exceeds its canvas bounds".to_owned());
                }
                let packed = descriptor[8];
                cursor += 9;
                if packed & 0x80 != 0 {
                    cursor += 3usize << ((packed & 7) + 1);
                }
                cursor += 1; // LZW minimum code size
                skip_gif_blocks(raw, &mut cursor)?;
                frames += 1;
                if frames > max_frames {
                    return Ok(frames);
                }
            }
            _ => return Err(invalid()),
        }
    }
}

fn skip_gif_blocks(raw: &[u8], cursor: &mut usize) -> Result<(), String> {
    loop {
        let size = *raw
            .get(*cursor)
            .ok_or_else(|| "Truncated GIF block".to_owned())? as usize;
        *cursor += 1;
        if size == 0 {
            return Ok(());
        }
        *cursor = cursor
            .checked_add(size)
            .filter(|end| *end <= raw.len())
            .ok_or_else(|| "Truncated GIF block".to_owned())?;
    }
}

fn webp_frame_count(raw: &[u8], max_frames: usize, canvas: (u32, u32)) -> Result<usize, String> {
    let mut cursor = 12usize;
    let mut frames = 0;
    while cursor < raw.len() {
        let header = raw
            .get(cursor..cursor + 8)
            .ok_or_else(|| "Truncated WebP chunk".to_owned())?;
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if &header[..4] == b"ANMF" {
            let descriptor = raw
                .get(cursor + 8..cursor + 24)
                .filter(|_| size >= 16)
                .ok_or_else(|| "Truncated WebP frame".to_owned())?;
            let le24 = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
            let left = le24(&descriptor[..3]) * 2;
            let top = le24(&descriptor[3..6]) * 2;
            let width = le24(&descriptor[6..9]) + 1;
            let height = le24(&descriptor[9..12]) + 1;
            if left + width > canvas.0 || top + height > canvas.1 {
                return Err("WebP frame exceeds its canvas bounds".to_owned());
            }
            frames += 1;
            if frames > max_frames {
                return Ok(frames);
            }
        }
        cursor = cursor
            .checked_add(8)
            .and_then(|cursor| cursor.checked_add(size))
            .and_then(|cursor| cursor.checked_add(size & 1))
            .filter(|end| *end <= raw.len())
            .ok_or_else(|| "Truncated WebP chunk".to_owned())?;
    }
    Ok(frames.max(1))
}

struct TextureEntry {
    texture: egui::TextureHandle,
    used_frame: u64,
    used_order: u64,
}

#[derive(Default)]
struct TextureCache {
    entries: HashMap<String, HashMap<egui::TextureOptions, TextureEntry>>,
    bytes: usize,
    count: usize,
    order: u64,
}

impl TextureCache {
    fn remove(&mut self, uri: &str, options: egui::TextureOptions) {
        if let Some(variants) = self.entries.get_mut(uri) {
            if let Some(entry) = variants.remove(&options) {
                self.bytes -= entry.texture.byte_size();
                self.count -= 1;
            }
            if variants.is_empty() {
                self.entries.remove(uri);
            }
        }
    }

    fn remove_source(&mut self, source: &str) {
        self.entries.retain(|uri, variants| {
            if source_uri(uri) == source {
                for entry in variants.values() {
                    self.bytes -= entry.texture.byte_size();
                    self.count -= 1;
                }
                false
            } else {
                true
            }
        });
    }
}

struct BoundedTextureLoader {
    cache: Mutex<TextureCache>,
    bytes_limit: usize,
    entries_limit: usize,
    metrics: Arc<LoaderMetrics>,
}

impl TextureLoader for BoundedTextureLoader {
    fn id(&self) -> &str {
        TEXTURE_LOADER_ID
    }

    fn load(
        &self,
        ctx: &egui::Context,
        uri: &str,
        options: egui::TextureOptions,
        size_hint: SizeHint,
    ) -> TextureLoadResult {
        let pass = frame(ctx);
        let mut cache = self.cache.lock().unwrap();
        cache.order = cache.order.wrapping_add(1);
        let order = cache.order;
        if let Some(entry) = cache
            .entries
            .get_mut(uri)
            .and_then(|variants| variants.get_mut(&options))
        {
            entry.used_frame = pass;
            entry.used_order = order;
            let texture = SizedTexture::from_handle(&entry.texture);
            drop(cache);
            let source = source_uri(uri);
            if source != uri
                && ctx
                    .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(source)))
                    .is_none()
            {
                // A decoder can be evicted while one of its frame textures
                // survives. Restore timing metadata before egui gets stuck on
                // frame zero. Image loading must run without our texture lock.
                let _ = ctx.try_load_image(uri, size_hint);
                if ctx
                    .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(source)))
                    .is_some()
                {
                    ctx.request_repaint();
                }
            }
            return Ok(TexturePoll::Ready { texture });
        }
        // No texture exists yet, so refusing admission cannot invalidate shapes
        // already queued for painting in this pass.
        let image = match ctx.try_load_image(uri, size_hint)? {
            ImagePoll::Pending { size } => return Ok(TexturePoll::Pending { size }),
            ImagePoll::Ready { image } => image,
        };
        let bytes = image
            .pixels
            .len()
            .saturating_mul(std::mem::size_of::<egui::Color32>());
        if bytes > self.bytes_limit || self.entries_limit == 0 {
            return Err(full_cache_error());
        }
        while cache.bytes.saturating_add(bytes) > self.bytes_limit
            || cache.count >= self.entries_limit
        {
            let oldest = cache
                .entries
                .iter()
                .flat_map(|(uri, variants)| {
                    variants
                        .iter()
                        .map(move |(options, entry)| (uri, options, entry))
                })
                .filter(|(_, _, entry)| entry.used_frame != pass)
                .min_by_key(|(_, _, entry)| entry.used_order)
                .map(|(uri, options, _)| (uri.clone(), *options));
            let Some((oldest, options)) = oldest else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return Err(full_cache_error());
            };
            cache.remove(&oldest, options);
        }
        let upload_started = Instant::now();
        let handle = ctx.load_texture(uri, image, options);
        self.metrics.texture_uploads.fetch_add(1, Ordering::Relaxed);
        self.metrics.texture_upload_micros.fetch_add(
            upload_started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        let texture = SizedTexture::from_handle(&handle);
        cache.bytes += handle.byte_size();
        cache.count += 1;
        cache.entries.entry(uri.to_owned()).or_default().insert(
            options,
            TextureEntry {
                texture: handle,
                used_frame: pass,
                used_order: order,
            },
        );
        Ok(TexturePoll::Ready { texture })
    }

    fn forget(&self, uri: &str) {
        self.cache.lock().unwrap().remove_source(source_uri(uri));
    }

    fn forget_all(&self) {
        *self.cache.lock().unwrap() = TextureCache::default();
    }

    fn byte_size(&self) -> usize {
        self.cache.lock().unwrap().bytes
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ProbeDecoder {
        calls: Arc<AtomicUsize>,
        unsupported_format: bool,
    }

    impl ImageLoader for ProbeDecoder {
        fn id(&self) -> &str {
            "probe_decoder"
        }
        fn load(&self, _: &egui::Context, _: &str, _: SizeHint) -> ImageLoadResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.unsupported_format {
                Err(LoadError::FormatNotSupported {
                    detected_format: Some("png".to_owned()),
                })
            } else {
                Ok(ImagePoll::Ready {
                    image: Arc::new(egui::ColorImage::new([2, 1], egui::Color32::WHITE)),
                })
            }
        }
        fn forget(&self, _: &str) {}
        fn forget_all(&self) {}
        fn byte_size(&self) -> usize {
            0
        }
    }

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    pub(crate) fn gif() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
        encoder
            .encode_frame(image::Frame::from_parts(
                image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        encoder
            .encode_frame(image::Frame::from_parts(
                image::RgbaImage::from_pixel(2, 1, image::Rgba([0, 255, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        drop(encoder);
        out
    }

    fn context(bytes: usize, entries: usize) -> egui::Context {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        install(&ctx, bytes, entries);
        ctx
    }

    pub(crate) fn animated_webp() -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]) {
            out.extend_from_slice(tag);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
            if payload.len() % 2 != 0 {
                out.push(0);
            }
        }
        let mut body = b"WEBP".to_vec();
        // Animation flag, 2x1 canvas (dimensions are stored minus one).
        chunk(&mut body, b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        chunk(&mut body, b"ANIM", &[0; 6]);
        for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 1, image::Rgba(color)))
                .write_to(&mut encoded, image::ImageFormat::WebP)
                .unwrap();
            let mut frame = vec![0; 16];
            frame[6] = 1; // frame width minus one
            frame[12] = 100; // duration in milliseconds
            frame[15] = 2; // replace the frame rather than blending
            frame.extend_from_slice(&encoded.into_inner()[12..]);
            chunk(&mut body, b"ANMF", &frame);
        }
        let mut raw = b"RIFF".to_vec();
        raw.extend_from_slice(&(body.len() as u32).to_le_bytes());
        raw.extend_from_slice(&body);
        raw
    }

    fn texture(ctx: &egui::Context, uri: &str) -> TextureLoadResult {
        ctx.try_load_texture(uri, egui::TextureOptions::LINEAR, SizeHint::default())
    }

    fn ready_image(ctx: &egui::Context, uri: &str) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match ctx.try_load_image(uri, SizeHint::default()) {
                Ok(ImagePoll::Ready { .. }) => return,
                Ok(ImagePoll::Pending { .. }) => {
                    assert!(Instant::now() < deadline, "image did not finish decoding");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("image decode failed: {error:?}"),
            }
        }
    }

    fn ready_texture(ctx: &egui::Context, uri: &str) -> TextureLoadResult {
        ready_image(ctx, uri);
        texture(ctx, uri)
    }

    #[test]
    fn animation_budget_is_checked_before_the_decoder_runs() {
        let ctx = egui::Context::default();
        let calls = Arc::new(AtomicUsize::new(0));
        ctx.add_image_loader(Arc::new(ProbeDecoder {
            calls: calls.clone(),
            unsupported_format: false,
        }));
        install(&ctx, 8, 10);
        let uri = "bytes://animation.gif";
        ctx.include_bytes(uri, gif());
        let frame = format!("{uri}#0");
        assert!(ctx.try_load_image(&frame, SizeHint::default()).is_err());
        assert!(ctx.try_load_image(&frame, SizeHint::default()).is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn format_rejection_falls_through_and_preserves_detection() {
        let ctx = egui::Context::default();
        let supported = Arc::new(AtomicUsize::new(0));
        let unsupported = Arc::new(AtomicUsize::new(0));
        ctx.add_image_loader(Arc::new(ProbeDecoder {
            calls: supported.clone(),
            unsupported_format: false,
        }));
        ctx.add_image_loader(Arc::new(ProbeDecoder {
            calls: unsupported.clone(),
            unsupported_format: true,
        }));
        install(&ctx, 1024, 10);
        assert!(matches!(
            ctx.try_load_image("custom://test.png", SizeHint::default()),
            Ok(ImagePoll::Ready { .. })
        ));
        assert_eq!(unsupported.load(Ordering::Relaxed), 1);
        assert_eq!(supported.load(Ordering::Relaxed), 1);

        let ctx = egui::Context::default();
        ctx.add_image_loader(Arc::new(ProbeDecoder {
            calls: unsupported,
            unsupported_format: true,
        }));
        install(&ctx, 1024, 10);
        assert!(
            matches!(ctx.try_load_image("custom://test.png", SizeHint::default()),
            Err(LoadError::NoMatchingImageLoader { detected_format: Some(format) }) if format == "png")
        );
    }

    #[test]
    fn a_pending_byte_source_can_fall_through_to_a_custom_decoder() {
        use egui::load::{BytesLoadResult, BytesLoader, BytesPoll};
        use std::sync::atomic::AtomicBool;

        #[derive(Default)]
        struct DelayedBytes(AtomicBool);

        impl BytesLoader for DelayedBytes {
            fn id(&self) -> &str {
                "delayed_bytes_fixture"
            }
            fn load(&self, _: &egui::Context, _: &str) -> BytesLoadResult {
                if self.0.load(Ordering::Relaxed) {
                    Ok(BytesPoll::Ready {
                        size: None,
                        bytes: vec![0].into(),
                        mime: None,
                    })
                } else {
                    Ok(BytesPoll::Pending { size: None })
                }
            }
            fn forget(&self, _: &str) {}
            fn forget_all(&self) {}
            fn byte_size(&self) -> usize {
                0
            }
        }

        let ctx = egui::Context::default();
        let bytes = Arc::new(DelayedBytes::default());
        let calls = Arc::new(AtomicUsize::new(0));
        ctx.add_bytes_loader(bytes.clone());
        ctx.add_image_loader(Arc::new(ProbeDecoder {
            calls: calls.clone(),
            unsupported_format: false,
        }));
        install(&ctx, 1024, 10);
        assert!(matches!(
            ctx.try_load_image("custom://pending-image", SizeHint::default()),
            Ok(ImagePoll::Pending { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        bytes.0.store(true, Ordering::Relaxed);
        assert!(matches!(
            ctx.try_load_image("custom://pending-image", SizeHint::default()),
            Ok(ImagePoll::Ready { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn static_color_sampling_reuses_the_regular_decoded_image() {
        let ctx = context(8, 1);
        let source = "bytes://shared.png";
        let raw: Arc<[u8]> = png().into();
        ctx.include_bytes(source, raw.clone());
        ready_image(&ctx, source);
        let ImagePoll::Ready { image } = ctx.try_load_image(source, SizeHint::default()).unwrap()
        else {
            panic!("the ordinary static image must be ready");
        };
        for animate in [true, false] {
            let (sample, index) = color_frame(&ctx, source, raw.clone(), animate).unwrap();
            assert_eq!(index, 0);
            assert!(Arc::ptr_eq(&image, &sample));
        }
        assert_eq!(work_stats(&ctx).completed_decodes, 1);
        assert_eq!(work_stats(&ctx).animation_bytes, 8);
    }

    #[test]
    fn pending_custom_decoders_recheck_size_when_image_bytes_arrive() {
        struct WaitingDecoder(Arc<AtomicUsize>);

        impl ImageLoader for WaitingDecoder {
            fn id(&self) -> &str {
                "waiting_decoder_fixture"
            }
            fn load(&self, _: &egui::Context, _: &str, _: SizeHint) -> ImageLoadResult {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(ImagePoll::Pending { size: None })
            }
            fn forget(&self, _: &str) {}
            fn forget_all(&self) {}
            fn byte_size(&self) -> usize {
                0
            }
        }

        let ctx = egui::Context::default();
        let calls = Arc::new(AtomicUsize::new(0));
        ctx.add_image_loader(Arc::new(WaitingDecoder(calls.clone())));
        install(&ctx, 4, 10);
        let source = "bytes://pending-custom";
        ctx.include_bytes(source, vec![0]);
        assert!(matches!(
            ctx.try_load_image(source, SizeHint::default()),
            Ok(ImagePoll::Pending { .. })
        ));
        ctx.include_bytes(source, png());
        assert!(matches!(
            ctx.try_load_image(source, SizeHint::default()),
            Err(LoadError::Loading(_))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn gif_and_webp_container_walks_reject_truncated_chunks() {
        let mut raw = gif();
        assert_eq!(gif_frame_count(&raw, 10).unwrap(), 2);
        raw.pop();
        assert!(gif_frame_count(&raw, 10).is_err());
        let mut webp = b"RIFF\x1c\x00\x00\x00WEBPANMF\x10\x00\x00\x00\x00".to_vec();
        assert!(webp_frame_count(&webp, 10, (1, 1)).is_err());
        webp.extend_from_slice(&[0; 15]);
        assert_eq!(webp_frame_count(&webp, 10, (1, 1)).unwrap(), 1);
        webp[26] = 255;
        assert!(webp_frame_count(&webp, 10, (1, 1)).is_err());
    }

    #[test]
    fn gif_descriptor_cannot_allocate_beyond_a_small_canvas() {
        let mut raw = gif();
        let descriptor = raw.iter().position(|byte| *byte == 0x2c).unwrap();
        raw[descriptor + 5..descriptor + 7].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(preflight_decoded_size(&raw, 1024)
            .unwrap_err()
            .contains("canvas"));
        assert!(preflight_size(&raw, 1024, true)
            .unwrap_err()
            .contains("canvas"));
    }

    #[test]
    fn paused_animation_needs_only_one_frame_of_decode_budget() {
        let ctx = context(8, 1);
        let raw = gif();
        assert!(preflight_decoded_size(&raw, 8).is_err());
        assert_eq!(preflight_size(&raw, 8, true).unwrap(), 8);
        let source = first_frame_uri("bytes://paused.gif");
        ctx.include_bytes(source.clone(), raw);
        ready_image(&ctx, &format!("{source}#0"));
        let ImagePoll::Ready { image } = ctx
            .try_load_image(&format!("{source}#0"), SizeHint::default())
            .unwrap()
        else {
            panic!("the paused first frame must be available");
        };
        assert!(image
            .pixels
            .iter()
            .all(|pixel| *pixel == egui::Color32::RED));
    }

    #[test]
    fn static_texture_eviction_respects_current_pass_and_reloads() {
        let ctx = context(1024, 1);
        ctx.include_bytes("bytes://first.png", png());
        ctx.include_bytes("bytes://second.png", png());
        let mut first_id = None;
        let _ = ctx.run(Default::default(), |ctx| {
            first_id = ready_texture(ctx, "bytes://first.png")
                .unwrap()
                .texture_id();
            assert!(texture(ctx, "bytes://second.png").is_err());
            assert_eq!(
                texture(ctx, "bytes://first.png").unwrap().texture_id(),
                first_id
            );
        });
        let _ = ctx.run(Default::default(), |ctx| {
            assert!(matches!(
                ready_texture(ctx, "bytes://second.png"),
                Ok(TexturePoll::Ready { .. })
            ));
        });
        let _ = ctx.run(Default::default(), |ctx| {
            let reloaded = ready_texture(ctx, "bytes://first.png")
                .unwrap()
                .texture_id();
            assert_ne!(first_id, reloaded);
        });
    }

    #[test]
    fn forgetting_an_animation_source_frees_all_frames_and_reloads() {
        let ctx = context(1024, 10);
        let raw = gif();
        let uri = "bytes://animation.gif";
        ctx.include_bytes(uri, raw.clone());
        let first = format!("{uri}#0");
        let second = format!("{uri}#1");
        let first_id = ready_texture(&ctx, &first).unwrap().texture_id();
        assert!(matches!(
            texture(&ctx, &second),
            Ok(TexturePoll::Ready { .. })
        ));
        let loaders = ctx.loaders();
        assert_eq!(
            loaders
                .texture
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>(),
            16
        );
        assert!(
            loaders
                .image
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>()
                >= 16
        );
        ctx.forget_image(uri);
        assert_eq!(
            loaders
                .texture
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>(),
            0
        );
        assert_eq!(
            loaders
                .image
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>(),
            0
        );
        ctx.include_bytes(uri, raw);
        assert_ne!(ready_texture(&ctx, &first).unwrap().texture_id(), first_id);
    }

    #[test]
    fn surviving_animation_textures_restore_timing_after_decoder_eviction() {
        for (uri, raw) in [
            ("bytes://returning.gif", gif()),
            ("bytes://returning.webp", animated_webp()),
        ] {
            let ctx = context(1024, 1);
            ctx.include_bytes(uri, raw.clone());
            let image = egui::Image::from_bytes(uri, raw);
            let mut first_id = None;
            let at = |time| egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let _ = ctx.run(at(0.0), |ctx| {
                ready_image(ctx, &format!("{uri}#0"));
                first_id = image
                    .load_for_size(ctx, egui::vec2(2.0, 1.0))
                    .unwrap()
                    .texture_id();
            });
            assert!(first_id.is_some());
            ctx.include_bytes("bytes://decoder-pressure.png", png());
            let _ = ctx.run(at(0.05), |ctx| {
                ready_image(ctx, "bytes://decoder-pressure.png");
                assert!(matches!(
                    ctx.try_load_image("bytes://decoder-pressure.png", SizeHint::default()),
                    Ok(ImagePoll::Ready { .. })
                ));
                assert!(ctx
                    .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(uri)))
                    .is_none());
                // The image budget evicted the animation, but its first frame
                // still occupies the independent texture cache.
                assert_eq!(
                    ctx.loaders()
                        .texture
                        .lock()
                        .iter()
                        .map(|loader| loader.byte_size())
                        .sum::<usize>(),
                    8
                );
            });
            let _ = ctx.run(at(0.05), |ctx| {
                assert_eq!(
                    image
                        .load_for_size(ctx, egui::vec2(2.0, 1.0))
                        .unwrap()
                        .texture_id(),
                    first_id
                );
                ready_image(ctx, &format!("{uri}#0"));
                assert!(ctx
                    .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(uri)))
                    .is_some());
            });
            let _ = ctx.run(at(0.15), |ctx| {
                let next = image
                    .load_for_size(ctx, egui::vec2(2.0, 1.0))
                    .unwrap()
                    .texture_id();
                assert!(next.is_some());
                assert_ne!(
                    next, first_id,
                    "returning animation must advance after decoder eviction"
                );
            });
        }
    }

    #[test]
    fn ordinary_image_widgets_cycle_extensionless_animations_without_static_duplicates() {
        for raw in [gif(), animated_webp()] {
            let ctx = context(16, 4);
            let image = egui::Image::from_bytes("bytes://extensionless-animation", raw);
            let at = |time| egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let mut first = None;
            let _ = ctx.run(at(0.0), |ctx| {
                let mut result = image.load_for_size(ctx, egui::vec2(2.0, 1.0));
                assert!(matches!(result, Ok(TexturePoll::Pending { .. })));
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                while matches!(result, Ok(TexturePoll::Pending { .. })) {
                    assert!(
                        Instant::now() < deadline,
                        "animation did not finish decoding"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    result = image.load_for_size(ctx, egui::vec2(2.0, 1.0));
                }
                first = result.unwrap().texture_id();
            });
            assert!(first.is_some());
            let _ = ctx.run(at(0.15), |ctx| {
                let second = image
                    .load_for_size(ctx, egui::vec2(2.0, 1.0))
                    .unwrap()
                    .texture_id();
                assert!(second.is_some());
                assert_ne!(first, second, "the image widget must advance to frame one");
            });
            let _ = ctx.run(at(0.25), |ctx| {
                assert_eq!(
                    image
                        .load_for_size(ctx, egui::vec2(2.0, 1.0))
                        .unwrap()
                        .texture_id(),
                    first,
                    "the image widget must loop to its retained first texture"
                );
            });
            assert_eq!(work_stats(&ctx).completed_decodes, 1);
            assert_eq!(work_stats(&ctx).animation_bytes, 16);
        }
    }

    #[test]
    fn animated_webp_passes_preflight_and_uploads_distinct_frames() {
        let raw = animated_webp();
        assert!(preflight_decoded_size(&raw, 16).is_ok());
        assert!(preflight_decoded_size(&raw, 8).is_err());
        let ctx = context(1024, 10);
        ctx.include_bytes("bytes://animation.webp", raw);
        let first = ready_texture(&ctx, "bytes://animation.webp#0")
            .unwrap()
            .texture_id();
        let second = texture(&ctx, "bytes://animation.webp#1")
            .unwrap()
            .texture_id();
        assert!(first.is_some());
        assert!(second.is_some());
        assert_ne!(first, second);
    }

    #[test]
    fn decoded_and_texture_byte_budgets_reject_oversized_images() {
        let ctx = context(4, 10);
        ctx.include_bytes("bytes://oversized.png", png());
        assert!(texture(&ctx, "bytes://oversized.png").is_err());
        let loaders = ctx.loaders();
        assert!(
            loaders
                .image
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>()
                <= 4
        );
        assert_eq!(
            loaders
                .texture
                .lock()
                .iter()
                .map(|loader| loader.byte_size())
                .sum::<usize>(),
            0
        );
    }

    #[test]
    #[ignore = "isolated cold static-image decode timing"]
    fn perf_static_image_cold_load() {
        use std::time::{Duration, Instant};

        let pixels = image::RgbaImage::from_fn(1024, 1024, |x, y| {
            image::Rgba([
                ((x * 17 + y * 31) % 256) as u8,
                ((x * x + y * 7) % 256) as u8,
                ((x * 3 + y * y) % 256) as u8,
                255,
            ])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let raw = encoded.into_inner();
        let mut first_calls = [Vec::new(), Vec::new()];
        let mut totals = [Vec::new(), Vec::new()];
        for _ in 0..7 {
            for bounded in [false, true] {
                let ctx = if bounded {
                    context(128 * 1024 * 1024, 4096)
                } else {
                    let ctx = egui::Context::default();
                    egui_extras::install_image_loaders(&ctx);
                    ctx
                };
                ctx.include_bytes("bytes://cold.png", raw.clone());
                let started = Instant::now();
                let mut result = texture(&ctx, "bytes://cold.png");
                let mode = usize::from(bounded);
                first_calls[mode].push(started.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(matches!(result, Ok(TexturePoll::Pending { .. })), bounded);
                while matches!(result, Ok(TexturePoll::Pending { .. })) {
                    assert!(started.elapsed() < Duration::from_secs(10));
                    std::thread::sleep(Duration::from_millis(1));
                    result = texture(&ctx, "bytes://cold.png");
                }
                assert!(matches!(result, Ok(TexturePoll::Ready { .. })));
                totals[mode].push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        for (mode, label) in ["synchronous", "background"].into_iter().enumerate() {
            first_calls[mode].sort_by(f64::total_cmp);
            totals[mode].sort_by(f64::total_cmp);
            eprintln!(
                "cold PNG 1024x1024, mode={label}, encoded_bytes={}, first_call_ms={:?}, complete_ms={:?}",
                raw.len(), first_calls[mode], totals[mode],
            );
        }
    }

    #[test]
    #[ignore = "isolated cold animation decode timing"]
    fn perf_animation_cold_load() {
        use std::time::{Duration, Instant};

        let mut raw = Vec::new();
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut raw);
        for frame in 0..120 {
            let pixels = image::RgbaImage::from_fn(112, 112, |x, y| {
                image::Rgba([
                    ((x * 3 + frame) % 256) as u8,
                    ((y * 3 + frame * 2) % 256) as u8,
                    ((x + y + frame * 5) % 256) as u8,
                    255,
                ])
            });
            encoder
                .encode_frame(image::Frame::from_parts(
                    pixels,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(20, 1),
                ))
                .unwrap();
        }
        drop(encoder);
        let mut first_calls = Vec::new();
        let mut totals = Vec::new();
        for _ in 0..7 {
            let ctx = context(128 * 1024 * 1024, 4096);
            ctx.include_bytes("bytes://cold.gif", raw.clone());
            let started = Instant::now();
            let mut result = texture(&ctx, "bytes://cold.gif#0");
            first_calls.push(started.elapsed().as_secs_f64() * 1000.0);
            while matches!(result, Ok(TexturePoll::Pending { .. })) {
                assert!(started.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(1));
                result = texture(&ctx, "bytes://cold.gif#0");
            }
            assert!(matches!(result, Ok(TexturePoll::Ready { .. })));
            totals.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        first_calls.sort_by(f64::total_cmp);
        totals.sort_by(f64::total_cmp);
        eprintln!(
            "cold GIF 112x112x120, encoded_bytes={}, first_call_ms={first_calls:?}, complete_ms={totals:?}",
            raw.len(),
        );
    }
}
