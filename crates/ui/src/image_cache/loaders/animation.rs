//! Decode animation frames on a shared bounded worker queue.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::load::{Bytes, BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use image::{AnimationDecoder, ImageDecoder, ImageFormat};

use super::{frame, preflight_decoded_size, LoaderMetrics};

const LOADER_ID: &str = "crust::background_animation_loader";
const WORKERS: usize = 2;
const QUEUED_JOBS: usize = 32;

struct Reservation {
    bytes: AtomicUsize,
    metrics: Arc<LoaderMetrics>,
}

impl Reservation {
    fn acquire(bytes: usize, limit: usize, metrics: &Arc<LoaderMetrics>) -> Option<Self> {
        metrics
            .animation_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|total| *total <= limit)
            })
            .ok()?;
        Some(Self {
            bytes: AtomicUsize::new(bytes),
            metrics: metrics.clone(),
        })
    }

    fn shrink(&self, bytes: usize) {
        let previous = self.bytes.swap(bytes, Ordering::AcqRel);
        debug_assert!(bytes <= previous);
        self.metrics
            .animation_bytes
            .fetch_sub(previous.saturating_sub(bytes), Ordering::AcqRel);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.metrics
            .animation_bytes
            .fetch_sub(*self.bytes.get_mut(), Ordering::AcqRel);
    }
}

struct Animation {
    frames: Vec<Arc<egui::ColorImage>>,
    durations: egui::FrameDurations,
    bytes: usize,
}

struct DecodeSlot {
    result: Mutex<Option<Result<Arc<Animation>, String>>>,
    cancelled: AtomicBool,
    size: egui::Vec2,
    reservation: Reservation,
}

struct Entry {
    slot: Arc<DecodeSlot>,
    used_pass: u64,
    order: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    order: u64,
}

impl Cache {
    fn remove(&mut self, uri: &str) {
        if let Some(entry) = self.entries.remove(uri) {
            entry.slot.cancelled.store(true, Ordering::Release);
        }
    }
}

struct PendingJob(Arc<LoaderMetrics>);

impl Drop for PendingJob {
    fn drop(&mut self) {
        self.0.pending_decodes.fetch_sub(1, Ordering::Relaxed);
    }
}

struct DecodeJob {
    raw: Bytes,
    format: ImageFormat,
    first_frame: bool,
    ctx: egui::Context,
    slot: Arc<DecodeSlot>,
    _pending: PendingJob,
}

impl DecodeJob {
    fn run(self) {
        if self.slot.cancelled.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decode(&self.raw, self.format, self.first_frame, &self.slot)
        }))
        .unwrap_or_else(|_| Err("Animation decoder failed".to_owned()));
        let metrics = &self.slot.reservation.metrics;
        metrics.completed_decodes.fetch_add(1, Ordering::Relaxed);
        metrics.decode_micros.fetch_add(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if self.slot.cancelled.load(Ordering::Acquire) {
            drop(result);
            self.slot.reservation.shrink(0);
        } else {
            let bytes = result.as_ref().map_or(0, |animation| animation.bytes);
            *self.slot.result.lock().unwrap() = Some(result.map(Arc::new));
            self.slot.reservation.shrink(bytes);
        }
        self.ctx.request_repaint();
    }
}

fn shared_queue() -> Result<&'static SyncSender<DecodeJob>, &'static str> {
    static QUEUE: OnceLock<Result<SyncSender<DecodeJob>, ()>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<DecodeJob>(QUEUED_JOBS);
            let receiver = Arc::new(Mutex::new(receiver));
            let mut workers = 0;
            for index in 0..WORKERS {
                let receiver = receiver.clone();
                if std::thread::Builder::new()
                    .name(format!("crust-image-{index}"))
                    .spawn(move || loop {
                        let job = receiver.lock().unwrap().recv();
                        match job {
                            Ok(job) => job.run(),
                            Err(_) => break,
                        }
                    })
                    .is_ok()
                {
                    workers += 1;
                }
            }
            if workers == 0 {
                Err(())
            } else {
                Ok(sender)
            }
        })
        .as_ref()
        .map_err(|_| "Could not start an image decoder")
}

pub(super) struct AnimationLoader {
    cache: Mutex<Cache>,
    bytes_limit: usize,
    entries_limit: usize,
    metrics: Arc<LoaderMetrics>,
    #[cfg(test)]
    queue: Option<SyncSender<DecodeJob>>,
}

impl AnimationLoader {
    pub(super) fn new(
        bytes_limit: usize,
        entries_limit: usize,
        metrics: Arc<LoaderMetrics>,
    ) -> Self {
        Self {
            cache: Mutex::default(),
            bytes_limit,
            entries_limit,
            metrics,
            #[cfg(test)]
            queue: None,
        }
    }

    fn enqueue(&self, job: DecodeJob) -> Result<(), TrySendError<DecodeJob>> {
        #[cfg(test)]
        if let Some(queue) = &self.queue {
            return queue.try_send(job);
        }
        match shared_queue() {
            Ok(queue) => queue.try_send(job),
            Err(_) => Err(TrySendError::Disconnected(job)),
        }
    }

    fn poll(
        ctx: &egui::Context,
        source: &str,
        index: usize,
        first_frame: bool,
        slot: &DecodeSlot,
    ) -> ImageLoadResult {
        let result = slot.result.lock().unwrap().clone();
        match result {
            None => Ok(ImagePoll::Pending {
                size: Some(slot.size),
            }),
            Some(Err(reason)) => Err(LoadError::Loading(reason)),
            Some(Ok(animation)) => {
                // Only a currently retained slot can publish timing metadata.
                // Forgetting a source detaches cancelled work from this path.
                if !first_frame {
                    ctx.data_mut(|data| {
                        data.insert_temp(egui::Id::new(source), animation.durations.clone())
                    });
                }
                Ok(ImagePoll::Ready {
                    image: animation.frames[index % animation.frames.len()].clone(),
                })
            }
        }
    }

    #[cfg(test)]
    fn color_frame(
        &self,
        ctx: &egui::Context,
        source: &str,
        animate: bool,
    ) -> Option<(Arc<egui::ColorImage>, usize)> {
        let first_frame = !animate
            || !matches!(
                ctx.try_load_bytes(source),
                Ok(BytesPoll::Ready { bytes, .. })
                    if matches!(image::guess_format(&bytes), Ok(ImageFormat::Gif | ImageFormat::WebP))
            );
        let mut index = if first_frame {
            0
        } else {
            frame_index(ctx, source)
        };
        let ImagePoll::Ready { mut image } =
            self.load_source(ctx, source, index, first_frame).ok()?
        else {
            return None;
        };
        if !first_frame {
            // The first completed poll restores timing after a decode or eviction.
            // Select the current frame immediately instead of flashing frame zero.
            let current = frame_index(ctx, source);
            if current != index {
                let ImagePoll::Ready {
                    image: current_image,
                } = self.load_source(ctx, source, current, false).ok()?
                else {
                    return None;
                };
                image = current_image;
                index = current;
            }
        }
        Some((image, index))
    }

    fn load_source(
        &self,
        ctx: &egui::Context,
        source: &str,
        index: usize,
        first_frame: bool,
    ) -> ImageLoadResult {
        let key = if first_frame {
            format!("first-frame:{source}")
        } else {
            source.to_owned()
        };
        let pass = frame(ctx);
        let mut cache = self.cache.lock().unwrap();
        cache.order = cache.order.wrapping_add(1);
        let order = cache.order;
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.used_pass = pass;
            entry.order = order;
            let slot = entry.slot.clone();
            drop(cache);
            return Self::poll(ctx, source, index, first_frame, &slot);
        }
        let raw = match ctx.try_load_bytes(source)? {
            BytesPoll::Pending { size } => return Ok(ImagePoll::Pending { size }),
            BytesPoll::Ready { bytes, .. } => bytes,
        };
        let format = match image::guess_format(&raw) {
            Ok(format) if first_frame => format,
            Ok(format @ (ImageFormat::Gif | ImageFormat::WebP)) => format,
            _ => return Err(LoadError::NotSupported),
        };
        let (width, height) = image::ImageReader::with_format(Cursor::new(&raw), format)
            .into_dimensions()
            .map_err(|error| LoadError::Loading(error.to_string()))?;
        let bytes = if first_frame {
            super::preflight_size(&raw, self.bytes_limit, true).map_err(LoadError::Loading)?
        } else {
            preflight_decoded_size(&raw, self.bytes_limit).map_err(LoadError::Loading)?
        };
        let size = Some(egui::vec2(width as f32, height as f32));
        while self
            .metrics
            .animation_bytes
            .load(Ordering::Acquire)
            .saturating_add(bytes)
            > self.bytes_limit
            || cache.entries.len() >= self.entries_limit
        {
            let oldest = cache
                .entries
                .iter()
                .filter(|(_, entry)| entry.used_pass != pass)
                .min_by_key(|(_, entry)| entry.order)
                .map(|(uri, _)| uri.clone());
            let Some(oldest) = oldest else {
                ctx.request_repaint_after(Duration::from_millis(25));
                return Ok(ImagePoll::Pending { size });
            };
            cache.remove(&oldest);
            ctx.data_mut(|data| data.remove::<egui::FrameDurations>(egui::Id::new(&oldest)));
        }
        let Some(reservation) = Reservation::acquire(bytes, self.bytes_limit, &self.metrics) else {
            ctx.request_repaint_after(Duration::from_millis(25));
            return Ok(ImagePoll::Pending { size });
        };
        let slot = Arc::new(DecodeSlot {
            result: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            size: size.unwrap(),
            reservation,
        });
        self.metrics.pending_decodes.fetch_add(1, Ordering::Relaxed);
        let job = DecodeJob {
            raw,
            format,
            first_frame,
            ctx: ctx.clone(),
            slot: slot.clone(),
            _pending: PendingJob(self.metrics.clone()),
        };
        match self.enqueue(job) {
            Ok(()) => {
                cache.entries.insert(
                    key,
                    Entry {
                        slot,
                        used_pass: pass,
                        order,
                    },
                );
                Ok(ImagePoll::Pending { size })
            }
            Err(TrySendError::Full(_)) => {
                ctx.request_repaint_after(Duration::from_millis(25));
                Ok(ImagePoll::Pending { size })
            }
            Err(TrySendError::Disconnected(_)) => Err(LoadError::Loading(
                "Image decoder is unavailable".to_owned(),
            )),
        }
    }
}

impl ImageLoader for AnimationLoader {
    fn id(&self) -> &str {
        LOADER_ID
    }

    fn load(&self, ctx: &egui::Context, uri: &str, _: SizeHint) -> ImageLoadResult {
        let (source, index) =
            egui::decode_animated_image_uri(uri).map_err(|_| LoadError::NotSupported)?;
        self.load_source(
            ctx,
            source,
            index,
            source.starts_with(super::FIRST_FRAME_PREFIX),
        )
    }

    fn forget(&self, uri: &str) {
        let source = super::source_uri(uri);
        let mut cache = self.cache.lock().unwrap();
        cache.remove(source);
        cache.remove(&format!("first-frame:{source}"));
    }

    fn forget_all(&self) {
        let mut cache = self.cache.lock().unwrap();
        for (_, entry) in cache.entries.drain() {
            entry.slot.cancelled.store(true, Ordering::Release);
        }
    }

    fn byte_size(&self) -> usize {
        self.metrics.animation_bytes.load(Ordering::Relaxed)
    }
}

pub(super) fn frame_index(ctx: &egui::Context, source: &str) -> usize {
    let Some(durations) =
        ctx.data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(source)))
    else {
        return 0;
    };
    if durations.all().len() <= 1 {
        return 0;
    }
    let total: Duration = durations.all().sum();
    if total.is_zero() {
        return 0;
    }
    let time = ctx.input(|input| input.time);
    let now = if time.is_finite() && time >= 0.0 {
        Duration::from_secs_f64(time)
    } else {
        Duration::ZERO
    };
    let position = now.as_nanos() % total.as_nanos();
    let mut end = 0;
    for (index, duration) in durations.all().enumerate() {
        end += duration.as_nanos();
        if position < end {
            let remaining = (end - position).min(u64::MAX as u128) as u64;
            ctx.request_repaint_after(Duration::from_nanos(remaining));
            return index;
        }
    }
    0
}

fn decode(
    raw: &[u8],
    format: ImageFormat,
    first_frame: bool,
    slot: &DecodeSlot,
) -> Result<Animation, String> {
    let limit = slot.reservation.bytes.load(Ordering::Relaxed);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(limit as u64);
    if first_frame {
        let mut reader = image::ImageReader::with_format(Cursor::new(raw), format);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| error.to_string())?
            .into_rgba8();
        let bytes = image.as_raw().len();
        if bytes > limit {
            return Err("Image exceeds the decoded byte limit".to_owned());
        }
        return Ok(Animation {
            frames: vec![Arc::new(egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            ))],
            durations: egui::FrameDurations::new(vec![Duration::ZERO]),
            bytes,
        });
    }
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(raw))
                .map_err(|error| error.to_string())?;
            decoder
                .set_limits(limits)
                .map_err(|error| error.to_string())?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(raw))
                .map_err(|error| error.to_string())?;
            decoder
                .set_limits(limits)
                .map_err(|error| error.to_string())?;
            if !decoder.has_animation() {
                let image = image::DynamicImage::from_decoder(decoder)
                    .map_err(|error| error.to_string())?
                    .into_rgba8();
                let bytes = image.as_raw().len();
                if bytes > limit {
                    return Err("Image exceeds the decoded byte limit".to_owned());
                }
                return Ok(Animation {
                    frames: vec![Arc::new(egui::ColorImage::from_rgba_unmultiplied(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    ))],
                    durations: egui::FrameDurations::new(vec![Duration::ZERO]),
                    bytes,
                });
            }
            decoder.into_frames()
        }
        _ => return Err("Unsupported animation format".to_owned()),
    };
    let mut images = Vec::new();
    let mut durations = Vec::new();
    let mut bytes = 0usize;
    for frame in frames {
        if slot.cancelled.load(Ordering::Acquire) {
            return Err("Animation decode cancelled".to_owned());
        }
        let frame = frame.map_err(|error| error.to_string())?;
        let image = frame.buffer();
        bytes = bytes
            .checked_add(image.as_raw().len())
            .filter(|bytes| *bytes <= limit)
            .ok_or_else(|| "Animation exceeds the decoded byte limit".to_owned())?;
        images.push(Arc::new(egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        )));
        durations.push(frame.delay().into());
    }
    if images.is_empty() {
        return Err("Animation contains no frames".to_owned());
    }
    Ok(Animation {
        frames: images,
        durations: egui::FrameDurations::new(durations),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif(first: [u8; 4]) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut raw);
        for color in [first, [0, 255, 0, 255]] {
            encoder
                .encode_frame(image::Frame::from_parts(
                    image::RgbaImage::from_pixel(2, 1, image::Rgba(color)),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
        }
        drop(encoder);
        raw
    }

    fn controlled(
        bytes: usize,
        entries: usize,
        queue_size: usize,
    ) -> (AnimationLoader, mpsc::Receiver<DecodeJob>) {
        let (queue, receiver) = mpsc::sync_channel(queue_size);
        let mut loader = AnimationLoader::new(bytes, entries, Arc::default());
        loader.queue = Some(queue);
        (loader, receiver)
    }

    fn load(loader: &AnimationLoader, ctx: &egui::Context, uri: &str) -> ImageLoadResult {
        loader.load(ctx, uri, SizeHint::default())
    }

    fn expect_ready(result: ImageLoadResult, color: egui::Color32) {
        let Ok(ImagePoll::Ready { image }) = result else {
            panic!("expected a decoded frame")
        };
        assert_eq!(image.size, [2, 1]);
        assert!(image.pixels.iter().all(|pixel| *pixel == color));
    }

    #[test]
    fn evicted_first_frames_reenter_the_combined_decoder_budget() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let (background, queue) = controlled(128, 16, 4);
        let background = Arc::new(background);
        let loaders = ctx.loaders();
        let mut images = loaders.image.lock();
        let mut inner = std::mem::take(&mut *images);
        inner.push(background.clone());
        let wrapper = Arc::new(super::super::BoundedImageLoader {
            inner,
            cache: Mutex::default(),
            bytes_limit: 128,
            entries_limit: 16,
        });
        images.push(wrapper.clone());
        drop(images);

        let first_a = super::super::first_frame_uri("bytes://a.png");
        let first_b = super::super::first_frame_uri("bytes://b.png");
        let first_c = super::super::first_frame_uri("bytes://c.png");
        let first_d = super::super::first_frame_uri("bytes://d.png");
        for (uri, width) in [
            ("bytes://old-static.png", 1),
            (first_a.as_str(), 2),
            (first_b.as_str(), 1),
            (first_c.as_str(), 27),
            ("bytes://recent-static.png", 1),
            (first_d.as_str(), 3),
        ] {
            let mut raw = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                width,
                1,
                image::Rgba([255, 0, 0, 255]),
            ))
            .write_to(&mut raw, ImageFormat::Png)
            .unwrap();
            ctx.include_bytes(uri.to_owned(), raw.into_inner());
        }
        let finish = |ctx: &egui::Context, source: &str| {
            let uri = format!("{source}#0");
            assert!(matches!(
                ctx.try_load_image(&uri, SizeHint::default()),
                Ok(ImagePoll::Pending { .. })
            ));
            queue.try_recv().unwrap().run();
            assert!(matches!(
                ctx.try_load_image(&uri, SizeHint::default()),
                Ok(ImagePoll::Ready { .. })
            ));
        };
        let _ = ctx.run(Default::default(), |ctx| {
            assert!(matches!(
                ctx.try_load_image("bytes://old-static.png", SizeHint::default()),
                Ok(ImagePoll::Ready { .. })
            ));
            for source in [&first_a, &first_b, &first_c] {
                finish(ctx, source);
            }
            assert!(matches!(
                ctx.try_load_image("bytes://recent-static.png", SizeHint::default()),
                Ok(ImagePoll::Ready { .. })
            ));
            assert_eq!(wrapper.inner_bytes(), 128);
        });
        let _ = ctx.run(Default::default(), |ctx| {
            // The background decoder drops A (8 bytes) to admit D (12 bytes).
            // The wrapper then drops the old static image (4 bytes), leaving
            // 124 background bytes plus a 4-byte static image.
            finish(ctx, &first_d);
            assert!(!background
                .cache
                .lock()
                .unwrap()
                .entries
                .contains_key(&format!("first-frame:{first_a}")));
            assert!(!wrapper.cache.lock().unwrap().entries[&first_a].pending);
            assert_eq!(wrapper.inner_bytes(), 128);

            // A wrapper hit now starts a new decode. Its inner eviction drops B
            // (4 bytes), which alone would leave 132 combined decoded bytes.
            let uri = format!("{first_a}#0");
            assert!(matches!(
                ctx.try_load_image(&uri, SizeHint::default()),
                Ok(ImagePoll::Pending { .. })
            ));
            assert!(wrapper.cache.lock().unwrap().entries[&first_a].pending);
            assert!(
                wrapper.inner_bytes() <= 128,
                "returning first frames must enforce the combined budget"
            );
            queue.try_recv().unwrap().run();
            assert!(matches!(
                ctx.try_load_image(&uri, SizeHint::default()),
                Ok(ImagePoll::Ready { .. })
            ));
            assert!(
                queue.try_recv().is_err(),
                "polling must not enqueue a duplicate decode"
            );
            assert_eq!(
                background.metrics.pending_decodes.load(Ordering::Relaxed),
                0
            );
            assert!(wrapper.byte_size() <= 128);
        });
    }

    #[test]
    fn pending_animation_deduplicates_and_publishes_frames_after_decode() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 1);
        ctx.include_bytes("bytes://one.gif", gif([255, 0, 0, 255]));
        assert!(matches!(
            load(&loader, &ctx, "bytes://one.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        let job = queue.try_recv().unwrap();
        assert!(matches!(
            load(&loader, &ctx, "bytes://one.gif#1"),
            Ok(ImagePoll::Pending { .. })
        ));
        assert!(queue.try_recv().is_err());
        assert_eq!(loader.byte_size(), 16);
        assert_eq!(loader.metrics.pending_decodes.load(Ordering::Relaxed), 1);
        assert!(ctx
            .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new("bytes://one.gif")))
            .is_none());
        job.run();
        expect_ready(load(&loader, &ctx, "bytes://one.gif#0"), egui::Color32::RED);
        expect_ready(
            load(&loader, &ctx, "bytes://one.gif#1"),
            egui::Color32::GREEN,
        );
        expect_ready(load(&loader, &ctx, "bytes://one.gif#2"), egui::Color32::RED);
        assert_eq!(loader.metrics.pending_decodes.load(Ordering::Relaxed), 0);
        assert_eq!(loader.metrics.completed_decodes.load(Ordering::Relaxed), 1);
        assert!(ctx
            .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new("bytes://one.gif")))
            .is_some());
    }

    #[test]
    fn forgotten_work_cannot_replace_a_new_source_generation() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(32, 2, 2);
        let source = "bytes://changing.gif";
        let uri = "bytes://changing.gif#0";
        ctx.include_bytes(source, gif([255, 0, 0, 255]));
        assert!(matches!(
            load(&loader, &ctx, uri),
            Ok(ImagePoll::Pending { .. })
        ));
        let old = queue.try_recv().unwrap();
        loader.forget(source);
        ctx.forget_image(source);
        ctx.include_bytes(source, gif([0, 0, 255, 255]));
        assert!(matches!(
            load(&loader, &ctx, uri),
            Ok(ImagePoll::Pending { .. })
        ));
        let current = queue.try_recv().unwrap();
        old.run();
        assert!(matches!(
            load(&loader, &ctx, uri),
            Ok(ImagePoll::Pending { .. })
        ));
        assert_eq!(loader.byte_size(), 16);
        current.run();
        expect_ready(load(&loader, &ctx, uri), egui::Color32::BLUE);
        assert_eq!(loader.metrics.completed_decodes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancellation_keeps_its_reservation_until_queued_work_is_released() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 2);
        for name in ["first", "second"] {
            ctx.include_bytes(format!("bytes://{name}.gif"), gif([255, 0, 0, 255]));
        }
        assert!(matches!(
            load(&loader, &ctx, "bytes://first.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        let old = queue.try_recv().unwrap();
        loader.forget_all();
        assert_eq!(loader.byte_size(), 16);
        assert!(matches!(
            load(&loader, &ctx, "bytes://second.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        assert!(queue.try_recv().is_err());
        old.run();
        assert_eq!(loader.byte_size(), 0);
        assert!(matches!(
            load(&loader, &ctx, "bytes://second.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://second.gif#0"),
            egui::Color32::RED,
        );
    }

    #[test]
    fn a_full_queue_leaves_the_source_retryable_without_retaining_a_reservation() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(64, 4, 1);
        for name in ["first", "second"] {
            ctx.include_bytes(format!("bytes://{name}.gif"), gif([255, 0, 0, 255]));
            assert!(matches!(
                load(&loader, &ctx, &format!("bytes://{name}.gif#0")),
                Ok(ImagePoll::Pending { .. })
            ));
        }
        assert_eq!(loader.metrics.pending_decodes.load(Ordering::Relaxed), 1);
        assert_eq!(loader.byte_size(), 16);
        assert_eq!(loader.cache.lock().unwrap().entries.len(), 1);
        queue.try_recv().unwrap().run();
        assert!(matches!(
            load(&loader, &ctx, "bytes://second.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://second.gif#0"),
            egui::Color32::RED,
        );
    }

    #[test]
    fn eviction_releases_old_frames_but_preserves_this_pass() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 2);
        for name in ["first", "second", "third"] {
            ctx.include_bytes(format!("bytes://{name}.gif"), gif([255, 0, 0, 255]));
        }
        assert!(matches!(
            load(&loader, &ctx, "bytes://first.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://first.gif#0"),
            egui::Color32::RED,
        );
        let _ = ctx.run(Default::default(), |_| {});
        assert!(matches!(
            load(&loader, &ctx, "bytes://second.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://second.gif#0"),
            egui::Color32::RED,
        );
        assert!(ctx
            .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new("bytes://first.gif")))
            .is_none());
        assert!(matches!(
            load(&loader, &ctx, "bytes://third.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        assert!(queue.try_recv().is_err());
        expect_ready(
            load(&loader, &ctx, "bytes://second.gif#0"),
            egui::Color32::RED,
        );
        let _ = ctx.run(Default::default(), |_| {});
        assert!(matches!(
            load(&loader, &ctx, "bytes://third.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://third.gif#0"),
            egui::Color32::RED,
        );
        assert_eq!(loader.byte_size(), 16);
    }

    #[test]
    fn invalid_frame_data_is_cached_as_an_error_and_releases_decode_memory() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 1);
        let mut raw = gif([255, 0, 0, 255]);
        let descriptor = raw.iter().position(|byte| *byte == 0x2c).unwrap();
        let packed = raw[descriptor + 9];
        let color_table = if packed & 0x80 != 0 {
            3usize << ((packed & 7) + 1)
        } else {
            0
        };
        raw[descriptor + 10 + color_table] = 0xff;
        ctx.include_bytes("bytes://invalid.gif", raw);
        assert!(matches!(
            load(&loader, &ctx, "bytes://invalid.gif#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        assert!(load(&loader, &ctx, "bytes://invalid.gif#0").is_err());
        assert!(load(&loader, &ctx, "bytes://invalid.gif#0").is_err());
        assert!(queue.try_recv().is_err());
        assert_eq!(loader.byte_size(), 0);
        assert_eq!(loader.metrics.pending_decodes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_still_webp_retains_its_single_frame() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(8, 1, 1);
        let mut raw = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            1,
            image::Rgba([255, 0, 0, 255]),
        ))
        .write_to(&mut raw, ImageFormat::WebP)
        .unwrap();
        ctx.include_bytes("bytes://still.webp", raw.into_inner());
        assert!(matches!(
            load(&loader, &ctx, "bytes://still.webp#0"),
            Ok(ImagePoll::Pending { .. })
        ));
        queue.try_recv().unwrap().run();
        expect_ready(
            load(&loader, &ctx, "bytes://still.webp#0"),
            egui::Color32::RED,
        );
    }

    #[test]
    fn paint_and_emote_animation_read_the_same_background_frames() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(32, 2, 2);
        let source = "bytes://paint.gif";
        ctx.include_bytes(source, gif([255, 0, 0, 255]));
        assert!(loader.color_frame(&ctx, source, true).is_none());
        assert!(loader.color_frame(&ctx, source, true).is_none());
        let job = queue.try_recv().unwrap();
        assert!(queue.try_recv().is_err());
        job.run();
        let _ = ctx.run(
            egui::RawInput {
                time: Some(0.15),
                ..Default::default()
            },
            |ctx| {
                let (paint, frame) = loader.color_frame(ctx, source, true).unwrap();
                assert_eq!(frame, 1);
                let ImagePoll::Ready { image } = load(&loader, ctx, "bytes://paint.gif#1").unwrap()
                else {
                    panic!("the shared emote frame must already be decoded");
                };
                assert!(Arc::ptr_eq(&paint, &image));
            },
        );
        assert!(loader.color_frame(&ctx, source, false).is_none());
        queue.try_recv().unwrap().run();
        let (still, frame) = loader.color_frame(&ctx, source, false).unwrap();
        assert_eq!(frame, 0);
        assert!(still
            .pixels
            .iter()
            .all(|pixel| *pixel == egui::Color32::RED));
        let durations = ctx
            .data(|data| data.get_temp::<egui::FrameDurations>(egui::Id::new(source)))
            .unwrap();
        assert_eq!(
            durations.all().count(),
            2,
            "a still frame must preserve animation timing"
        );
    }

    #[test]
    fn first_frame_native_png_reservation_shrinks_to_display_pixels() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 1);
        let source = "bytes://rgba16.png";
        let mut raw = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgba([u16::MAX, 0, 0, u16::MAX]),
        ))
        .write_to(&mut raw, ImageFormat::Png)
        .unwrap();
        ctx.include_bytes(source, raw.into_inner());
        assert!(loader.color_frame(&ctx, source, false).is_none());
        assert_eq!(
            loader.byte_size(),
            16,
            "native 16-bit output stays reserved during decoding"
        );
        queue.try_recv().unwrap().run();
        let (image, frame) = loader.color_frame(&ctx, source, false).unwrap();
        assert_eq!(frame, 0);
        assert!(image
            .pixels
            .iter()
            .all(|pixel| *pixel == egui::Color32::RED));
        assert_eq!(
            loader.byte_size(),
            8,
            "completed frames retain only RGBA8 pixels"
        );
    }

    #[test]
    fn still_frames_decode_off_thread_and_cancel_with_the_source() {
        let ctx = egui::Context::default();
        let (loader, queue) = controlled(16, 1, 2);
        let source = "bytes://still.png";
        let png = |color| {
            let mut raw = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 1, image::Rgba(color)))
                .write_to(&mut raw, ImageFormat::Png)
                .unwrap();
            raw.into_inner()
        };
        ctx.include_bytes(source, png([255, 0, 0, 255]));
        assert!(loader.color_frame(&ctx, source, false).is_none());
        let old = queue.try_recv().unwrap();
        assert!(loader.color_frame(&ctx, source, false).is_none());
        assert!(queue.try_recv().is_err());
        loader.forget(source);
        ctx.forget_image(source);
        ctx.include_bytes(source, png([0, 0, 255, 255]));
        assert!(loader.color_frame(&ctx, source, false).is_none());
        let current = queue.try_recv().unwrap();
        old.run();
        assert!(loader.color_frame(&ctx, source, false).is_none());
        current.run();
        let (image, frame) = loader.color_frame(&ctx, source, false).unwrap();
        assert_eq!(frame, 0);
        assert!(image
            .pixels
            .iter()
            .all(|pixel| *pixel == egui::Color32::BLUE));
        assert_eq!(loader.byte_size(), 8);
    }
}
