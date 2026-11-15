use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use crust_core::model::SenderNamePaint;
use egui::{Color32, ColorImage, Context, Galley, Id, Label, Pos2, Rect, RichText, Ui, Vec2};

use crate::image_cache::UiImageCache;
use crate::theme as t;

const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
const MAX_PIXELS: usize = 512 * 1024;
const MAX_BUILDS_PER_PASS: usize = 8;
const MAX_SHADOWS: usize = 16;
const MAX_SHADOW_EXTENT: f32 = 512.0;
const COVERAGE_GAMMA: f32 = 0.55;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PaintOptions {
    pub show: bool,
    pub shadows: bool,
    pub large_shadows: bool,
}

impl Default for PaintOptions {
    fn default() -> Self {
        Self {
            show: true,
            shadows: true,
            large_shadows: true,
        }
    }
}

pub(crate) fn configure(ctx: &Context, options: PaintOptions) {
    ctx.data_mut(|data| data.insert_temp(Id::new("name_paint_options"), options));
}

fn options(ctx: &Context) -> PaintOptions {
    ctx.data(|data| {
        data.get_temp(Id::new("name_paint_options"))
            .unwrap_or_default()
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    galley: usize,
    style: u64,
    animate: bool,
}

struct CachedName {
    // Keep the allocation alive so another galley cannot reuse its cache key.
    _galley: Arc<Galley>,
    mask: NameMask,
    shadow: Vec<Color32>,
    texture: egui::TextureHandle,
    frame: Option<u64>,
    used: u64,
    bytes: usize,
}

#[derive(Default)]
struct NameCache {
    names: HashMap<CacheKey, CachedName>,
    bytes: usize,
    pass: u64,
    builds: usize,
}

impl NameCache {
    fn make_room(&mut self, bytes: usize, pass: u64) -> bool {
        if bytes > CACHE_BYTES {
            return false;
        }
        while self.bytes + bytes > CACHE_BYTES || self.names.len() >= CACHE_ENTRIES {
            let victim = self
                .names
                .iter()
                .filter(|(_, value)| value.used != pass)
                .min_by_key(|(_, value)| value.used)
                .map(|(key, _)| *key);
            let Some(key) = victim else { return false };
            if let Some(entry) = self.names.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        true
    }
}

pub(crate) fn show_sender_name(
    ui: &mut Ui,
    name: &str,
    fallback_color: Color32,
    paint: Option<&SenderNamePaint>,
    images: &UiImageCache,
    animate: bool,
) -> egui::Response {
    show_sender_name_with_sense(
        ui,
        name,
        fallback_color,
        paint,
        images,
        animate,
        egui::Sense::click(),
    )
}

pub(crate) fn show_sender_name_with_sense(
    ui: &mut Ui,
    name: &str,
    fallback_color: Color32,
    paint: Option<&SenderNamePaint>,
    images: &UiImageCache,
    animate: bool,
    sense: egui::Sense,
) -> egui::Response {
    let base = Color32::from_rgb(fallback_color.r(), fallback_color.g(), fallback_color.b());
    let readable = t::readable_username_color(base);
    let settings = options(ui.ctx());
    let Some(paint) = paint.filter(|_| settings.show) else {
        return ui.add(
            Label::new(RichText::new(name).color(readable).strong())
                .selectable(false)
                .sense(sense),
        );
    };
    let (position, galley, response) = Label::new(RichText::new(name).strong())
        .selectable(false)
        .sense(sense)
        .layout_in_ui(ui);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), name));
    if !ui.is_rect_visible(response.rect) {
        return response;
    }

    let source = if is_image_paint(paint) {
        paint
            .image_url
            .as_deref()
            .and_then(normalize_image_url)
            .and_then(|url| images.color_frame(&url, ui.ctx(), animate))
    } else {
        None
    };
    let frame = source.as_ref().map(|(_, frame)| *frame);
    let image = source.as_ref().map(|(image, _)| image.as_ref());
    let key = CacheKey {
        galley: Arc::as_ptr(&galley) as usize,
        style: style_hash(paint, base, t::text_primary(), settings),
        animate,
    };
    let cache = ui.ctx().data_mut(|data| {
        data.get_temp_mut_or_default::<Arc<Mutex<NameCache>>>(Id::new("name_paint_cache"))
            .clone()
    });
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    let pass = ui.ctx().cumulative_pass_nr_for(egui::ViewportId::ROOT);
    if cache.pass != pass {
        cache.pass = pass;
        cache.builds = 0;
    }
    if let Some(entry) = cache.names.get_mut(&key) {
        entry.used = pass;
        if entry.frame != frame {
            entry.texture.set(
                render_name(
                    &entry.mask,
                    &entry.shadow,
                    paint,
                    base,
                    t::text_primary(),
                    image,
                ),
                egui::TextureOptions::LINEAR,
            );
            entry.frame = frame;
        }
        draw_cached(ui, position, entry);
        return response;
    }
    if cache.builds >= MAX_BUILDS_PER_PASS {
        ui.ctx().request_repaint();
        ui.painter().galley(position, galley, readable);
        return response;
    }
    let Some(mask) = name_mask(ui.ctx(), &galley, paint, settings) else {
        ui.painter().galley(position, galley, readable);
        return response;
    };
    let bytes = mask.name.len().saturating_mul(10);
    if !cache.make_room(bytes, pass) {
        ui.painter().galley(position, galley, readable);
        return response;
    }
    let shadow = shadow_layer(&mask, paint, settings);
    let pixels = render_name(&mask, &shadow, paint, base, t::text_primary(), image);
    let texture =
        ui.ctx()
            .load_texture("painted sender name", pixels, egui::TextureOptions::LINEAR);
    let entry = CachedName {
        _galley: galley,
        mask,
        shadow,
        texture,
        frame,
        used: pass,
        bytes,
    };
    draw_cached(ui, position, &entry);
    cache.bytes += bytes;
    cache.builds += 1;
    cache.names.insert(key, entry);
    response
}

fn draw_cached(ui: &Ui, position: Pos2, entry: &CachedName) {
    let scale = entry.mask.pixels_per_point;
    let position = (position.to_vec2() * scale).round() / scale;
    let rect = Rect::from_min_size(
        position.to_pos2() + entry.mask.origin,
        egui::vec2(entry.mask.size[0] as f32, entry.mask.size[1] as f32) / scale,
    );
    ui.painter().image(
        entry.texture.id(),
        rect,
        Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn style_hash(
    paint: &SenderNamePaint,
    base: Color32,
    colon: Color32,
    options: PaintOptions,
) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    paint.function.hash(&mut hash);
    paint.angle.map(f32::to_bits).hash(&mut hash);
    paint.repeat.hash(&mut hash);
    paint.image_url.hash(&mut hash);
    for stop in &paint.stops {
        stop.at.to_bits().hash(&mut hash);
        stop.color.hash(&mut hash);
    }
    for shadow in &paint.shadows {
        shadow.x_offset.to_bits().hash(&mut hash);
        shadow.y_offset.to_bits().hash(&mut hash);
        shadow.radius.to_bits().hash(&mut hash);
        shadow.color.hash(&mut hash);
    }
    base.hash(&mut hash);
    colon.hash(&mut hash);
    options.hash(&mut hash);
    hash.finish()
}

fn is_image_paint(paint: &SenderNamePaint) -> bool {
    matches!(
        paint.function.trim().to_ascii_lowercase().as_str(),
        "url" | "image"
    )
}

fn normalize_image_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("//") {
        Some(format!("https:{url}"))
    } else if url.starts_with("https://") || url.starts_with("http://") {
        Some(url.to_owned())
    } else {
        None
    }
}

struct NameMask {
    size: [usize; 2],
    origin: Vec2,
    pixels_per_point: f32,
    drawing_rect: Rect,
    name: Vec<u8>,
    colon: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Shadow {
    x: f32,
    y: f32,
    radius: f32,
    color: Color32,
}

fn shadows(paint: &SenderNamePaint, settings: PaintOptions, scale: f32) -> Vec<Shadow> {
    if !settings.shadows {
        return Vec::new();
    }
    let radius_scale = if settings.large_shadows { 3.0 } else { 1.0 };
    paint
        .shadows
        .iter()
        .take(MAX_SHADOWS)
        .filter_map(|shadow| {
            if !shadow.radius.is_finite()
                || shadow.radius <= 0.0
                || !shadow.x_offset.is_finite()
                || !shadow.y_offset.is_finite()
            {
                return None;
            }
            let color = t::parse_hex_rgba(&shadow.color)?;
            if color.a() == 0 {
                return None;
            }
            Some(Shadow {
                x: (shadow.x_offset * scale).clamp(-MAX_SHADOW_EXTENT, MAX_SHADOW_EXTENT),
                y: (shadow.y_offset * scale).clamp(-MAX_SHADOW_EXTENT, MAX_SHADOW_EXTENT),
                radius: (shadow.radius * radius_scale * scale).min(MAX_SHADOW_EXTENT),
                color,
            })
        })
        .collect()
}

fn name_mask(
    ctx: &Context,
    galley: &Galley,
    paint: &SenderNamePaint,
    settings: PaintOptions,
) -> Option<NameMask> {
    let scale = galley.pixels_per_point;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let colon = galley
        .rows
        .iter()
        .rev()
        .flat_map(|row| row.glyphs.iter().rev())
        .next()
        .filter(|glyph| glyph.chr == ':' && galley.job.text.ends_with(':'));
    let colon_start = colon.map(|glyph| glyph.pos);
    // Inline layout can include indentation and taller preceding widgets in
    // the galley rectangle. Only the name's glyphs define its paint bounds.
    let mut text_bounds = Rect::NOTHING;
    let mut drawing_rect = Rect::NOTHING;
    for glyph in galley.rows.iter().flat_map(|row| &row.glyphs) {
        text_bounds = text_bounds.union(glyph.logical_rect());
        if !(colon_start == Some(glyph.pos) && glyph.chr == ':') {
            drawing_rect = drawing_rect.union(glyph.logical_rect());
        }
    }
    if !drawing_rect.is_positive() {
        drawing_rect = text_bounds;
    }
    let mut bounds = galley.mesh_bounds.union(text_bounds);
    for shadow in shadows(paint, settings, scale) {
        let spread = shadow.radius.ceil() + 2.0;
        let shadow_bounds = bounds
            .translate(egui::vec2(shadow.x, shadow.y) / scale)
            .expand(spread / scale);
        bounds = bounds.union(shadow_bounds);
    }
    let min = (bounds.min.to_vec2() * scale).floor();
    let max = (bounds.max.to_vec2() * scale).ceil();
    let width = (max.x - min.x) as usize;
    let height = (max.y - min.y) as usize;
    let pixels = width.checked_mul(height)?;
    if width == 0 || height == 0 || width > 4096 || height > 4096 || pixels > MAX_PIXELS {
        return None;
    }
    let mut mask = NameMask {
        size: [width, height],
        origin: min / scale,
        pixels_per_point: scale,
        drawing_rect,
        name: vec![0; pixels],
        colon: vec![0; pixels],
    };
    // Borrow the atlas only while extracting the small set of required glyphs.
    let atlas = ctx.fonts(|fonts| fonts.texture_atlas());
    let atlas = atlas.lock();
    let font = atlas.image();
    for row in &galley.rows {
        for glyph in &row.glyphs {
            if glyph.uv_rect.is_nothing() {
                continue;
            }
            let target = if colon_start == Some(glyph.pos) && glyph.chr == ':' {
                &mut mask.colon
            } else {
                &mut mask.name
            };
            let top_left = ((glyph.pos + glyph.uv_rect.offset).to_vec2() * scale).round() - min;
            let source_width = usize::from(glyph.uv_rect.max[0] - glyph.uv_rect.min[0]);
            let source_height = usize::from(glyph.uv_rect.max[1] - glyph.uv_rect.min[1]);
            for y in 0..source_height {
                let dy = top_left.y as isize + y as isize;
                let sy = usize::from(glyph.uv_rect.min[1]) + y;
                if dy < 0 || dy >= height as isize || sy >= font.size[1] {
                    continue;
                }
                for x in 0..source_width {
                    let dx = top_left.x as isize + x as isize;
                    let sx = usize::from(glyph.uv_rect.min[0]) + x;
                    if dx < 0 || dx >= width as isize || sx >= font.size[0] {
                        continue;
                    }
                    let alpha = (font.pixels[sy * font.size[0] + sx]
                        .clamp(0.0, 1.0)
                        .powf(COVERAGE_GAMMA)
                        * 255.0)
                        .round() as u8;
                    let index = dy as usize * width + dx as usize;
                    target[index] = over_alpha(alpha, target[index]);
                }
            }
        }
    }
    Some(mask)
}

fn over_alpha(foreground: u8, background: u8) -> u8 {
    (u32::from(foreground) + (u32::from(background) * (255 - u32::from(foreground)) + 127) / 255)
        as u8
}

fn over(foreground: Color32, background: Color32) -> Color32 {
    if foreground.a() == 0 {
        return background;
    }
    if foreground.a() == 255 {
        return foreground;
    }
    let front = foreground.to_srgba_unmultiplied();
    let back = background.to_srgba_unmultiplied();
    let front_weight = u32::from(front[3]) * 255;
    let back_weight = u32::from(back[3]) * (255 - u32::from(front[3]));
    let weight = front_weight + back_weight;
    let channel = |index: usize| {
        ((u32::from(front[index]) * front_weight
            + u32::from(back[index]) * back_weight
            + weight / 2)
            / weight) as u8
    };
    Color32::from_rgba_unmultiplied(
        channel(0),
        channel(1),
        channel(2),
        ((weight + 127) / 255) as u8,
    )
}

fn opacity(color: Color32, alpha: u8) -> Color32 {
    let [red, green, blue, original_alpha] = color.to_srgba_unmultiplied();
    let alpha = ((u32::from(original_alpha) * u32::from(alpha) + 127) / 255) as u8;
    Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

fn shadow_layer(mask: &NameMask, paint: &SenderNamePaint, settings: PaintOptions) -> Vec<Color32> {
    let mut layer = vec![Color32::TRANSPARENT; mask.name.len()];
    let mut alpha = mask.name.clone();
    for shadow in shadows(paint, settings, mask.pixels_per_point) {
        let blurred = blur_alpha(&alpha, mask.size, shadow.radius);
        for y in 0..mask.size[1] {
            for x in 0..mask.size[0] {
                let coverage = sample_alpha(
                    &blurred,
                    mask.size,
                    x as f32 - shadow.x,
                    y as f32 - shadow.y,
                );
                let index = y * mask.size[0] + x;
                layer[index] = over(layer[index], opacity(shadow.color, coverage));
                alpha[index] = over_alpha(mask.name[index], layer[index].a());
            }
        }
    }
    layer
}

fn sample_alpha(pixels: &[u8], [width, height]: [usize; 2], x: f32, y: f32) -> u8 {
    let left = x.floor() as isize;
    let top = y.floor() as isize;
    let tx = x - x.floor();
    let ty = y - y.floor();
    let pixel = |x: isize, y: isize| -> f32 {
        if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
            0.0
        } else {
            pixels[y as usize * width + x as usize] as f32
        }
    };
    let top_value = pixel(left, top) * (1.0 - tx) + pixel(left + 1, top) * tx;
    let bottom = pixel(left, top + 1) * (1.0 - tx) + pixel(left + 1, top + 1) * tx;
    (top_value * (1.0 - ty) + bottom * ty).round() as u8
}

fn blur_alpha(pixels: &[u8], size: [usize; 2], radius: f32) -> Vec<u8> {
    let mut result = pixels.to_vec();
    // Three running box filters approximate a Gaussian in linear time.
    let sigma = (radius * 0.5).max(0.25);
    let ideal = (4.0 * sigma * sigma + 1.0).sqrt();
    let mut lower = ideal.floor() as usize;
    if lower % 2 == 0 {
        lower = lower.saturating_sub(1);
    }
    lower = lower.max(1);
    let upper = lower + 2;
    let lower_f = lower as f32;
    let count = ((12.0 * sigma * sigma - 3.0 * lower_f * lower_f - 12.0 * lower_f - 9.0)
        / (-4.0 * lower_f - 4.0))
        .round()
        .clamp(0.0, 3.0) as usize;
    let mut scratch = vec![0; pixels.len()];
    for pass in 0..3 {
        let radius = (if pass < count { lower } else { upper }) / 2;
        box_blur(&result, &mut scratch, size, radius, true);
        box_blur(&scratch, &mut result, size, radius, false);
    }
    result
}

fn box_blur(
    source: &[u8],
    target: &mut [u8],
    [width, height]: [usize; 2],
    radius: usize,
    horizontal: bool,
) {
    let (lines, length, stride) = if horizontal {
        (height, width, 1)
    } else {
        (width, height, width)
    };
    let count = (radius * 2 + 1) as u32;
    for line in 0..lines {
        let start = if horizontal { line * width } else { line };
        let mut sum = 0u32;
        for pos in 0..radius.min(length) {
            sum += u32::from(source[start + pos * stride]);
        }
        for pos in 0..length {
            if pos + radius < length {
                sum += u32::from(source[start + (pos + radius) * stride]);
            }
            target[start + pos * stride] = ((sum + count / 2) / count) as u8;
            if pos >= radius {
                sum -= u32::from(source[start + (pos - radius) * stride]);
            }
        }
    }
}

fn render_name(
    mask: &NameMask,
    shadow: &[Color32],
    paint: &SenderNamePaint,
    base: Color32,
    colon: Color32,
    image: Option<&ColorImage>,
) -> ColorImage {
    let mut pixels = shadow.to_vec();
    let readable = t::readable_username_color(base);
    let stops = normalized_stops(paint, base);
    let gradient = Gradient::new(paint, mask.drawing_rect, &stops);
    let image_paint = is_image_paint(paint);
    for y in 0..mask.size[1] {
        for x in 0..mask.size[0] {
            let index = y * mask.size[0] + x;
            let alpha = mask.name[index];
            if alpha != 0 {
                let point = mask.origin
                    + egui::vec2(x as f32 + 0.5, y as f32 + 0.5) / mask.pixels_per_point;
                let color = if let Some(image) = image {
                    over(
                        sample_image(image, mask.drawing_rect, point.to_pos2()),
                        base,
                    )
                } else if image_paint || stops.is_empty() {
                    readable
                } else {
                    gradient_color(&stops, gradient.position(point.to_pos2()), paint.repeat)
                };
                pixels[index] = over(opacity(color, alpha), pixels[index]);
            }
            if mask.colon[index] != 0 {
                pixels[index] = over(opacity(colon, mask.colon[index]), pixels[index]);
            }
        }
    }
    ColorImage {
        size: mask.size,
        pixels,
    }
}

fn normalized_stops(paint: &SenderNamePaint, base: Color32) -> Vec<(f32, Color32)> {
    let mut stops: Vec<_> = paint
        .stops
        .iter()
        .filter_map(|stop| {
            if !stop.at.is_finite() {
                return None;
            }
            t::parse_hex_rgba(&stop.color).map(|color| (stop.at.clamp(0.0, 1.0), over(color, base)))
        })
        .collect();
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    stops
}

enum Gradient {
    Linear {
        start: Pos2,
        axis: Vec2,
        inverse_length: f32,
    },
    Radial {
        center: Pos2,
        radius: f32,
        first: f32,
        length: f32,
    },
}

impl Gradient {
    fn new(paint: &SenderNamePaint, rect: Rect, stops: &[(f32, Color32)]) -> Self {
        if matches!(
            paint.function.trim().to_ascii_lowercase().as_str(),
            "radial-gradient" | "radial_gradient" | "radial"
        ) {
            let first = stops.first().map_or(0.0, |stop| stop.0);
            let last = stops.last().map_or(1.0, |stop| stop.0);
            let radius =
                rect.width().max(rect.height()) * 0.5 * if paint.repeat { last } else { 1.0 };
            Self::Radial {
                center: rect.center(),
                radius: radius.max(f32::EPSILON),
                first: if paint.repeat { first } else { 0.0 },
                length: if paint.repeat { last - first } else { 1.0 },
            }
        } else {
            let angle = paint
                .angle
                .filter(|angle| angle.is_finite())
                .unwrap_or(0.0)
                .rem_euclid(360.0);
            let radians = angle.to_radians();
            let direction = egui::vec2(radians.sin(), -radians.cos());
            let length = (rect.width() * direction.x.abs() + rect.height() * direction.y.abs())
                .max(f32::EPSILON);
            Self::Linear {
                start: rect.center() - direction * (length * 0.5),
                axis: direction,
                inverse_length: 1.0 / length,
            }
        }
    }

    fn position(&self, point: Pos2) -> f32 {
        match self {
            Self::Linear {
                start,
                axis,
                inverse_length,
            } => (point - *start).dot(*axis) * inverse_length,
            Self::Radial {
                center,
                radius,
                first,
                length,
            } => first + point.distance(*center) / radius * length,
        }
    }
}

fn gradient_color(stops: &[(f32, Color32)], mut position: f32, repeat: bool) -> Color32 {
    let Some(&(first, first_color)) = stops.first() else {
        return Color32::WHITE;
    };
    let &(last, last_color) = stops.last().unwrap();
    if repeat && last > first {
        position = first + (position - first).rem_euclid(last - first);
    }
    if position < first {
        return first_color;
    }
    // At a duplicate stop the last color at that position wins.
    let right = stops.partition_point(|(at, _)| *at <= position);
    if right == 0 {
        return first_color;
    }
    if right == stops.len() {
        return last_color;
    }
    let (left_at, left_color) = stops[right - 1];
    let (right_at, right_color) = stops[right];
    let fraction = ((position - left_at) / (right_at - left_at)).clamp(0.0, 1.0);
    mix(left_color, right_color, fraction)
}

fn mix(left: Color32, right: Color32, fraction: f32) -> Color32 {
    let left = left.to_srgba_unmultiplied();
    let right = right.to_srgba_unmultiplied();
    let left_weight = left[3] as f32 * (1.0 - fraction);
    let right_weight = right[3] as f32 * fraction;
    let alpha = left_weight + right_weight;
    if alpha <= 0.0 {
        return Color32::TRANSPARENT;
    }
    let channel = |index: usize| {
        ((left[index] as f32 * left_weight + right[index] as f32 * right_weight) / alpha).round()
            as u8
    };
    Color32::from_rgba_unmultiplied(channel(0), channel(1), channel(2), alpha.round() as u8)
}

fn sample_image(image: &ColorImage, rect: Rect, point: Pos2) -> Color32 {
    let [width, height] = image.size;
    if width == 0 || height == 0 || image.pixels.len() < width.saturating_mul(height) {
        return Color32::TRANSPARENT;
    }
    // Match a stretched image's texel centers, including clamped border samples.
    let x = (((point.x - rect.min.x) / rect.width().max(f32::EPSILON)) * width as f32 - 0.5)
        .clamp(0.0, (width - 1) as f32);
    let y = (((point.y - rect.min.y) / rect.height().max(f32::EPSILON)) * height as f32 - 0.5)
        .clamp(0.0, (height - 1) as f32);
    let left = x.floor() as usize;
    let top = y.floor() as usize;
    let right = (left + 1).min(width - 1);
    let bottom = (top + 1).min(height - 1);
    let tx = x - left as f32;
    let ty = y - top as f32;
    mix(
        mix(
            image.pixels[top * width + left],
            image.pixels[top * width + right],
            tx,
        ),
        mix(
            image.pixels[bottom * width + left],
            image.pixels[bottom * width + right],
            tx,
        ),
        ty,
    )
}

#[cfg(test)]
mod preview;

#[cfg(test)]
pub(crate) fn cached_name_count(ctx: &Context) -> usize {
    let cache =
        ctx.data(|data| data.get_temp::<Arc<Mutex<NameCache>>>(Id::new("name_paint_cache")));
    cache.map_or(0, |cache| cache.lock().unwrap().names.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{SenderNamePaintShadow, SenderNamePaintStop};

    fn gradient() -> SenderNamePaint {
        SenderNamePaint {
            function: "linear-gradient".to_owned(),
            angle: Some(90.0),
            stops: vec![
                SenderNamePaintStop {
                    at: 0.0,
                    color: "#ff0000".to_owned(),
                },
                SenderNamePaintStop {
                    at: 1.0,
                    color: "#0000ff".to_owned(),
                },
            ],
            ..Default::default()
        }
    }

    fn solid_mask(width: usize, height: usize) -> NameMask {
        NameMask {
            size: [width, height],
            origin: Vec2::ZERO,
            pixels_per_point: 1.0,
            drawing_rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(width as f32, height as f32)),
            name: vec![255; width * height],
            colon: vec![0; width * height],
        }
    }

    #[test]
    fn transparent_stops_reveal_username_color_without_darkening_twice() {
        let mut paint = gradient();
        paint.stops[0].color = "#ff000000".to_owned();
        paint.stops[1].color = "#ff000080".to_owned();
        let stops = normalized_stops(&paint, Color32::BLUE);
        assert_eq!(stops[0].1, Color32::BLUE);
        let half = stops[1].1;
        assert!((i16::from(half.r()) - 128).abs() <= 1, "{half:?}");
        assert!((i16::from(half.b()) - 127).abs() <= 1, "{half:?}");
        assert_eq!(half.a(), 255);
    }

    #[test]
    fn image_paints_keep_detail_between_glyph_corners_and_separate_the_colon() {
        let mut mask = solid_mask(5, 1);
        mask.name[4] = 0;
        mask.colon[4] = 255;
        mask.drawing_rect.max.x = 4.0;
        let paint = SenderNamePaint {
            function: "URL".to_owned(),
            ..Default::default()
        };
        let image = ColorImage {
            size: [4, 1],
            pixels: vec![
                Color32::RED,
                Color32::GREEN,
                Color32::BLUE,
                Color32::TRANSPARENT,
            ],
        };
        let out = render_name(
            &mask,
            &[Color32::TRANSPARENT; 5],
            &paint,
            Color32::YELLOW,
            Color32::WHITE,
            Some(&image),
        );
        assert_eq!(
            out.pixels,
            vec![
                Color32::RED,
                Color32::GREEN,
                Color32::BLUE,
                Color32::YELLOW,
                Color32::WHITE,
            ]
        );
    }

    #[test]
    fn linear_directions_and_radial_repeat_use_the_full_name_bounds() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 20.0));
        let mut paint = gradient();
        let stops = normalized_stops(&paint, Color32::BLACK);
        for (angle, start, end) in [
            (0.0, rect.center_bottom(), rect.center_top()),
            (90.0, rect.left_center(), rect.right_center()),
            (180.0, rect.center_top(), rect.center_bottom()),
            (270.0, rect.right_center(), rect.left_center()),
        ] {
            paint.angle = Some(angle);
            let sampler = Gradient::new(&paint, rect, &stops);
            assert!(sampler.position(start).abs() < 0.00001);
            assert!((sampler.position(end) - 1.0).abs() < 0.00001);
        }
        paint.function = "radial-gradient".to_owned();
        paint.repeat = true;
        let stops = vec![(0.25, Color32::RED), (0.5, Color32::BLUE)];
        let sampler = Gradient::new(&paint, rect, &stops);
        assert_eq!(
            gradient_color(&stops, sampler.position(rect.center()), true),
            Color32::RED
        );
        assert_eq!(
            gradient_color(
                &stops,
                sampler.position(rect.center() + egui::vec2(12.5, 0.0)),
                true
            ),
            Color32::from_rgb(128, 0, 128)
        );
        assert_eq!(
            gradient_color(
                &stops,
                sampler.position(rect.center() + egui::vec2(25.0, 0.0)),
                true
            ),
            Color32::RED
        );
    }

    #[test]
    fn repeated_and_hard_stops_keep_their_boundaries() {
        let stops = vec![
            (0.2, Color32::RED),
            (0.5, Color32::RED),
            (0.5, Color32::BLUE),
            (0.8, Color32::BLUE),
        ];
        assert_eq!(gradient_color(&stops, 0.499, false), Color32::RED);
        assert_eq!(gradient_color(&stops, 0.5, false), Color32::BLUE);
        assert_eq!(gradient_color(&stops, 0.9, true), Color32::RED);
        assert_eq!(gradient_color(&stops, 0.1, true), Color32::BLUE);
        assert_eq!(
            gradient_color(&[(0.5, Color32::RED), (0.5, Color32::BLUE)], 0.5, true),
            Color32::BLUE
        );
    }

    #[test]
    fn shadows_preserve_transparency_and_expand_only_when_enabled() {
        let mut mask = solid_mask(21, 21);
        mask.name.fill(0);
        mask.name[10 * 21 + 10] = 255;
        let mut paint = gradient();
        paint.shadows.push(SenderNamePaintShadow {
            x_offset: 2.0,
            y_offset: 0.0,
            radius: 2.0,
            color: "#ff000080".to_owned(),
        });
        let regular = PaintOptions {
            large_shadows: false,
            ..Default::default()
        };
        let layer = shadow_layer(&mask, &paint, regular);
        let center = layer[10 * 21 + 12];
        assert!(center.a() > 0 && center.a() < 128);
        assert!(center.to_srgba_unmultiplied()[0] >= 253);
        assert_eq!(center.g(), 0);
        assert_eq!(shadows(&paint, regular, 1.0)[0].radius, 2.0);
        assert_eq!(shadows(&paint, PaintOptions::default(), 1.0)[0].radius, 6.0);
        assert!(shadow_layer(
            &mask,
            &paint,
            PaintOptions {
                shadows: false,
                ..regular
            }
        )
        .iter()
        .all(|color| *color == Color32::TRANSPARENT));
    }

    #[test]
    fn a_wide_blur_handles_a_small_source_without_losing_edge_pixels() {
        let mut output = vec![0; 3];
        box_blur(&[30, 60, 90], &mut output, [3, 1], 5, true);
        assert_eq!(output, vec![16, 16, 16]);
    }

    #[test]
    fn high_dpi_shadows_keep_valid_long_offsets() {
        let paint = SenderNamePaint {
            shadows: vec![SenderNamePaintShadow {
                x_offset: 8.0,
                y_offset: 60.0,
                radius: 8.0,
                color: "#ffffff80".to_owned(),
            }],
            ..Default::default()
        };
        let shadow = shadows(&paint, PaintOptions::default(), 4.0)[0];
        assert_eq!((shadow.x, shadow.y, shadow.radius), (32.0, 240.0, 96.0));
    }

    #[test]
    fn actual_font_mask_has_separate_colon_and_survives_font_atlas_growth() {
        let ctx = Context::default();
        let paint = gradient();
        let _ = ctx.run(Default::default(), |ctx| {
            let galley = ctx.fonts(|fonts| {
                fonts.layout_no_wrap(
                    "Paint:".to_owned(),
                    egui::FontId::proportional(20.0),
                    Color32::WHITE,
                )
            });
            let mask = name_mask(ctx, &galley, &paint, PaintOptions::default()).unwrap();
            assert!(mask.name.iter().any(|alpha| *alpha > 0));
            assert!(mask.colon.iter().any(|alpha| *alpha > 0));
            assert!(mask.drawing_rect.width() < galley.size().x);
            let before = render_name(
                &mask,
                &vec![Color32::TRANSPARENT; mask.name.len()],
                &paint,
                Color32::GREEN,
                Color32::WHITE,
                None,
            );
            ctx.fonts(|fonts| {
                fonts.layout_no_wrap(
                    (32..1024).filter_map(char::from_u32).collect(),
                    egui::FontId::proportional(35.0),
                    Color32::WHITE,
                )
            });
            let after = render_name(
                &mask,
                &vec![Color32::TRANSPARENT; mask.name.len()],
                &paint,
                Color32::GREEN,
                Color32::WHITE,
                None,
            );
            assert_eq!(before.pixels, after.pixels);
        });
    }

    #[test]
    fn inline_names_keep_their_gradient_after_timestamps_and_badges() {
        let ctx = Context::default();
        let paint = gradient();
        let _ = ctx.run(Default::default(), |ctx| {
            let mut job = egui::text::LayoutJob::single_section(
                "Paint:".to_owned(),
                egui::TextFormat {
                    font_id: egui::FontId::proportional(20.0),
                    color: Color32::WHITE,
                    ..Default::default()
                },
            );
            job.wrap.max_width = 500.0;
            let plain = ctx.fonts(|fonts| fonts.layout_job(job.clone()));
            job.sections[0].leading_space = 80.0;
            let inline = ctx.fonts(|fonts| fonts.layout_job(job));
            let plain = name_mask(ctx, &plain, &paint, PaintOptions::default()).unwrap();
            let inline = name_mask(ctx, &inline, &paint, PaintOptions::default()).unwrap();
            assert!(
                (inline.drawing_rect.min.x - plain.drawing_rect.min.x - 80.0).abs() < 0.01,
                "plain={:?}, inline={:?}",
                plain.drawing_rect,
                inline.drawing_rect
            );
            assert_eq!(
                inline.size, plain.size,
                "preceding widgets must not enlarge the paint texture"
            );
            let render = |mask: &NameMask| {
                render_name(
                    mask,
                    &vec![Color32::TRANSPARENT; mask.name.len()],
                    &paint,
                    Color32::GREEN,
                    Color32::WHITE,
                    None,
                )
            };
            assert_eq!(render(&inline).pixels, render(&plain).pixels);
        });
    }

    #[test]
    fn warm_names_share_one_cached_texture_and_hidden_names_do_not_request_images() {
        let ctx = Context::default();
        let images = UiImageCache::default();
        let paint = gradient();
        for _ in 0..3 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for _ in 0..5 {
                        show_sender_name(ui, "Paint:", Color32::RED, Some(&paint), &images, true);
                    }
                });
            });
        }
        let cache = ctx.data(|data| {
            data.get_temp::<Arc<Mutex<NameCache>>>(Id::new("name_paint_cache"))
                .unwrap()
        });
        let cache = cache.lock().unwrap();
        assert_eq!(cache.names.len(), 1);
        assert!(cache.bytes < CACHE_BYTES);
        drop(cache);
        configure(
            &ctx,
            PaintOptions {
                show: false,
                ..Default::default()
            },
        );
        let image_paint = SenderNamePaint {
            function: "url".to_owned(),
            image_url: Some("https://example.test/hidden-paint.webp".to_owned()),
            ..Default::default()
        };
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_sender_name(
                    ui,
                    "Paint:",
                    Color32::RED,
                    Some(&image_paint),
                    &images,
                    true,
                );
            });
        });
        images.end_frame(&ctx, |_| {
            panic!("disabled name paints must not fetch images")
        });
    }

    #[test]
    fn malformed_gradient_metadata_still_renders_a_readable_name() {
        let mut paint = gradient();
        paint.angle = Some(f32::NAN);
        paint.stops[0].at = f32::NAN;
        paint.stops[1].color = "invalid".to_owned();
        let mask = solid_mask(3, 1);
        for base in [Color32::BLACK, Color32::WHITE, Color32::GREEN] {
            let out = render_name(
                &mask,
                &[Color32::TRANSPARENT; 3],
                &paint,
                base,
                Color32::WHITE,
                None,
            );
            assert_eq!(out.pixels, vec![t::readable_username_color(base); 3]);
        }
        assert_eq!(
            normalize_image_url(" //cdn.7tv.app/paint/x.webp ").as_deref(),
            Some("https://cdn.7tv.app/paint/x.webp")
        );
        assert!(normalize_image_url("javascript:alert(1)").is_none());
    }

    #[test]
    fn pending_image_paints_use_readable_fallback_but_transparency_keeps_the_base() {
        let mask = solid_mask(1, 1);
        let paint = SenderNamePaint {
            function: "URL".to_owned(),
            ..Default::default()
        };
        let transparent = ColorImage {
            size: [1, 1],
            pixels: vec![Color32::TRANSPARENT],
        };
        for base in [Color32::BLACK, Color32::WHITE] {
            let pending = render_name(
                &mask,
                &[Color32::TRANSPARENT],
                &paint,
                base,
                Color32::WHITE,
                None,
            );
            assert_eq!(pending.pixels[0], t::readable_username_color(base));
            let loaded = render_name(
                &mask,
                &[Color32::TRANSPARENT],
                &paint,
                base,
                Color32::WHITE,
                Some(&transparent),
            );
            assert_eq!(loaded.pixels[0], base);
        }
    }
}
