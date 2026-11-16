pub mod cache;
mod download;
pub mod providers;
mod validation;

pub use cache::EmoteCache;
pub use download::{read_image_response, MAX_IMAGE_DOWNLOAD_BYTES};
pub use providers::{EmoteInfo, EmoteProvider};
pub use validation::{validate_image, MAX_IMAGE_DECODE_BYTES};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmoteError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Image decode error: {0}")]
    Image(#[from] image::ImageError),
    #[error("Image response exceeds the {limit} byte download limit")]
    ImageTooLarge { limit: usize },
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Not found: {0}")]
    NotFound(String),
}
