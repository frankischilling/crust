//! Image upload engine.
//!
//! Chatterino-compatible: POSTs a multipart form to a configurable endpoint,
//! then uses dotted-path JSON extraction on the response to interpolate a
//! pattern into the final image link and (optional) deletion link.

use std::path::PathBuf;
use std::time::Duration;

use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub mod log;

pub use log::{append_log_entry, log_file_path, LogEntry};

const UPLOAD_TIMEOUT_SECS: u64 = 30;

/// Source of an image buffer to upload.
#[derive(Debug, Clone)]
pub struct RawImage {
    pub bytes: Vec<u8>,
    /// Extension without the dot (`"png"`, `"gif"`, `"jpeg"`).
    pub format: String,
    /// Original on-disk path if the image came from a file drop.
    pub path: Option<PathBuf>,
}

/// Configuration for a single upload endpoint (Imgur, Nuuls, ShareX SXCU, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UploaderConfig {
    /// Full POST endpoint URL.
    pub endpoint: String,
    /// Multipart form field name that carries the image bytes.
    pub request_form_field: String,
    /// Dotted-path pattern used to interpolate the final image URL from the
    /// JSON response. Empty = use the raw response body verbatim.
    pub image_link_json_path: String,
    /// Dotted-path pattern for the deletion URL. Empty = none.
    pub deletion_link_json_path: String,
    /// Extra HTTP headers, one per line, `Name: value` format.
    pub extra_headers: String,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        // Chatterino default: Nuuls. No auth, no deletion URL.
        Self {
            endpoint: "https://i.nuuls.com/upload".to_owned(),
            request_form_field: "attachment".to_owned(),
            image_link_json_path: String::new(),
            deletion_link_json_path: String::new(),
            extra_headers: String::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("invalid JSON response: {0}")]
    Json(String),
    #[error("empty image link")]
    EmptyLink,
    #[error("invalid config: {0}")]
    Config(String),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, Clone)]
pub struct UploadResponse {
    pub image_url: String,
    pub deletion_url: Option<String>,
}

/// Traverse a JSON value with a dotted-key path.
/// Returns empty string if the path does not resolve to a scalar.
pub fn get_json_value(root: &Value, pattern: &str) -> String {
    let mut cur = root;
    for key in pattern.split('.') {
        if key.is_empty() {
            continue;
        }
        match cur {
            Value::Object(map) => match map.get(key) {
                Some(v) => cur = v,
                None => return String::new(),
            },
            Value::Array(arr) => match key.parse::<usize>() {
                Ok(idx) if idx < arr.len() => cur = &arr[idx],
                _ => return String::new(),
            },
            _ => return String::new(),
        }
    }
    match cur {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Replace `{foo.bar.1}` placeholders in `pattern` with values pulled from
/// `root` via [`get_json_value`].
pub fn interpolate(pattern: &str, root: &Value) -> String {
    let re = Regex::new(r"\{([^{}]+?)\}").expect("static regex");
    let mut out = pattern.to_owned();
    loop {
        let Some(m) = re.find(&out) else { break };
        let range = m.range();
        let path = &out[range.start + 1..range.end - 1].to_owned();
        let repl = get_json_value(root, path);
        out.replace_range(range, &repl);
    }
    out
}

/// Parse a freeform `Name: value` per-line string into header tuples.
pub fn parse_header_list(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once(':')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

/// Upload a single image and resolve it into `(image_url, deletion_url)`.
pub async fn upload_image(
    cfg: &UploaderConfig,
    img: &RawImage,
) -> Result<UploadResponse, UploadError> {
    if cfg.endpoint.trim().is_empty() {
        return Err(UploadError::Config("empty endpoint".to_owned()));
    }
    if cfg.request_form_field.trim().is_empty() {
        return Err(UploadError::Config("empty request_form_field".to_owned()));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| UploadError::Network(e.to_string()))?;

    let filename = format!("control_v.{}", img.format);
    let mime = format!("image/{}", img.format);
    let part = reqwest::multipart::Part::bytes(img.bytes.clone())
        .file_name(filename)
        .mime_str(&mime)
        .map_err(|e| UploadError::Config(e.to_string()))?;
    let form = reqwest::multipart::Form::new().part(cfg.request_form_field.clone(), part);

    let mut req = client.post(&cfg.endpoint).multipart(form);
    for (k, v) in parse_header_list(&cfg.extra_headers) {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| UploadError::Network(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| UploadError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(UploadError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let image_url = if cfg.image_link_json_path.trim().is_empty() {
        body.trim().to_owned()
    } else {
        let root: Value =
            serde_json::from_str(&body).map_err(|e| UploadError::Json(e.to_string()))?;
        interpolate(&cfg.image_link_json_path, &root)
    };
    if image_url.is_empty() {
        return Err(UploadError::EmptyLink);
    }

    let deletion_url = if cfg.deletion_link_json_path.trim().is_empty() {
        None
    } else {
        let root: Value =
            serde_json::from_str(&body).map_err(|e| UploadError::Json(e.to_string()))?;
        let d = interpolate(&cfg.deletion_link_json_path, &root);
        if d.is_empty() {
            None
        } else {
            Some(d)
        }
    };

    Ok(UploadResponse {
        image_url,
        deletion_url,
    })
}

/// Resolve the default log location (`$CONFIG_DIR/crust/ImageUploader.json`).
pub fn default_log_dir() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("dev", "crust", "crust")?;
    Some(dirs.config_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_json_value_object_path() {
        let v = json!({"foo": {"bar": [1, "baz"]}});
        assert_eq!(get_json_value(&v, "foo.bar.1"), "baz");
    }

    #[test]
    fn get_json_value_missing_returns_empty() {
        let v = json!({"a": 1});
        assert_eq!(get_json_value(&v, "b.c"), "");
    }

    #[test]
    fn interpolate_replaces_placeholders() {
        let v = json!({"data": {"link": "https://i.imgur.com/abc.png"}});
        let out = interpolate("{data.link}", &v);
        assert_eq!(out, "https://i.imgur.com/abc.png");
    }

    #[test]
    fn interpolate_multiple_placeholders() {
        let v = json!({"foo": {"bar": [1, "baz", "qox"]}});
        let out = interpolate("https://x.y/{foo.bar.1}.{foo.bar.2}", &v);
        assert_eq!(out, "https://x.y/baz.qox");
    }

    #[test]
    fn parse_header_list_trims_and_skips_blanks() {
        let h = parse_header_list("\nAuthorization: Client-ID abc\n  X-Foo:  bar \n\n: skipme\n");
        assert_eq!(
            h,
            vec![
                ("Authorization".to_owned(), "Client-ID abc".to_owned()),
                ("X-Foo".to_owned(), "bar".to_owned()),
            ]
        );
    }
}
