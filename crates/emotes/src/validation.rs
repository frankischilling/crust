use std::io::Cursor;
use std::sync::{Arc, OnceLock};

use image::ImageDecoder;
use tokio::sync::Semaphore;

use crate::EmoteError;

/// Maximum native or RGBA output size accepted before validating image pixels.
/// Decoder scratch allocations and encoded input have separate limits.
pub const MAX_IMAGE_DECODE_BYTES: usize = 128 * 1024 * 1024;
const MAX_IMAGE_VALIDATIONS: usize = 2;

fn validation_slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_IMAGE_VALIDATIONS)))
}

/// Validate pixels on a bounded blocking worker and return the original source
/// with its dimensions. The worker retains its slot if the caller is cancelled.
pub async fn validate_image<B>(bytes: B) -> Result<(B, (u32, u32)), EmoteError>
where
    B: AsRef<[u8]> + Send + 'static,
{
    validate_with(bytes, validation_slots().clone(), |raw| {
        validate_pixels(raw, MAX_IMAGE_DECODE_BYTES)
    })
    .await
}

async fn validate_with<B, F>(
    bytes: B,
    slots: Arc<Semaphore>,
    validate: F,
) -> Result<(B, (u32, u32)), EmoteError>
where
    B: AsRef<[u8]> + Send + 'static,
    F: FnOnce(&[u8]) -> Result<(u32, u32), EmoteError> + Send + 'static,
{
    let permit = slots
        .acquire_owned()
        .await
        .map_err(|error| EmoteError::Io(std::io::Error::other(error.to_string())))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let dimensions = validate(bytes.as_ref())?;
        Ok((bytes, dimensions))
    })
    .await
    .map_err(|error| EmoteError::Io(std::io::Error::other(error.to_string())))?
}

fn size_limit_error() -> EmoteError {
    image::ImageError::Limits(image::error::LimitError::from_kind(
        image::error::LimitErrorKind::InsufficientMemory,
    ))
    .into()
}

fn check_output_size(
    width: u32,
    height: u32,
    native_bytes: u64,
    byte_limit: usize,
) -> Result<(), EmoteError> {
    let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(size_limit_error)?;
    if rgba_bytes.max(native_bytes) > byte_limit as u64 {
        return Err(size_limit_error());
    }
    Ok(())
}

fn validate_pixels(raw: &[u8], byte_limit: usize) -> Result<(u32, u32), EmoteError> {
    let mut reader = image::ImageReader::new(Cursor::new(raw)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(byte_limit as u64);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let dimensions = decoder.dimensions();
    check_output_size(
        dimensions.0,
        dimensions.1,
        decoder.total_bytes(),
        byte_limit,
    )?;
    decoder.set_limits(limits)?;
    // Validate the static image or first animation frame before caching the response.
    image::DynamicImage::from_decoder(decoder)?;
    Ok(dimensions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn encode(image: image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn output_size_covers_rgba_conversion_native_depth_and_overflow() {
        assert!(check_output_size(2, 1, 8, 8).is_ok());
        for (width, height, native, limit) in [
            (2, 1, 2, 7),
            (2, 1, 16, 8),
            (u32::MAX, u32::MAX, 0, usize::MAX),
        ] {
            assert!(matches!(
                check_output_size(width, height, native, limit),
                Err(EmoteError::Image(image::ImageError::Limits(_)))
            ));
        }
    }

    #[test]
    fn pixel_validation_rejects_oversized_output_from_image_headers() {
        let grayscale = encode(
            image::DynamicImage::new_luma8(2, 1),
            image::ImageFormat::Png,
        );
        let deep_rgba = encode(
            image::DynamicImage::new_rgba16(2, 1),
            image::ImageFormat::Png,
        );
        for (raw, limit) in [(grayscale, 7), (deep_rgba, 8)] {
            assert!(matches!(
                validate_pixels(&raw, limit),
                Err(EmoteError::Image(image::ImageError::Limits(_)))
            ));
        }
    }

    #[tokio::test]
    async fn supported_images_keep_their_bytes_and_dimensions() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
        ] {
            let raw = encode(image::DynamicImage::new_rgb8(3, 2), format);
            let expected = raw.clone();
            let (raw, dimensions) = validate_image(raw).await.unwrap();
            assert_eq!(raw, expected);
            assert_eq!(dimensions, (3, 2));
        }
    }

    #[tokio::test]
    async fn readable_header_with_corrupt_pixels_is_rejected() {
        let mut raw = encode(
            image::DynamicImage::new_rgba8(2, 1),
            image::ImageFormat::Png,
        );
        let image_data = raw.windows(4).position(|part| part == b"IDAT").unwrap();
        raw.truncate(image_data + 4);
        assert!(image::ImageReader::new(Cursor::new(&raw))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .is_ok());
        assert!(validate_image(raw).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_validation_retains_its_slot_until_blocking_work_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let first = tokio::spawn(validate_with(b"first".to_vec(), slots.clone(), move |_| {
            let _ = started.send(());
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok((1, 1))
        }));
        tokio::time::timeout(Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(slots.available_permits(), 0);

        let (second_started, mut second_ready) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(validate_with(
            b"second".to_vec(),
            slots.clone(),
            move |_| {
                let _ = second_started.send(());
                Ok((2, 1))
            },
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut second_ready)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let (raw, dimensions) = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(raw, b"second");
        assert_eq!(dimensions, (2, 1));
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn failed_validation_releases_its_slot() {
        let slots = Arc::new(Semaphore::new(1));
        assert!(
            validate_with(Vec::<u8>::new(), slots.clone(), |_| Err(size_limit_error()))
                .await
                .is_err()
        );
        assert_eq!(slots.available_permits(), 1);
        assert!(validate_with(Vec::<u8>::new(), slots.clone(), |_| panic!(
            "decoder failure"
        ))
        .await
        .is_err());
        assert_eq!(slots.available_permits(), 1);
    }
}
