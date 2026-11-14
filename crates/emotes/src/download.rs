use crate::EmoteError;

/// Maximum encoded body size for an image download, including animated emotes.
/// The 32 MiB limit applies to both declared and streamed response sizes.
pub const MAX_IMAGE_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Read a successful image response without buffering an unbounded HTTP body.
/// Image decoding and cache writes remain the caller's responsibility.
pub async fn read_image_response(response: reqwest::Response) -> Result<Vec<u8>, EmoteError> {
    read_image_response_with_limit(response, MAX_IMAGE_DOWNLOAD_BYTES).await
}

async fn read_image_response_with_limit(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, EmoteError> {
    let mut response = response.error_for_status()?;
    let declared_size = response.content_length();
    if declared_size.is_some_and(|size| size > limit as u64) {
        return Err(EmoteError::ImageTooLarge { limit });
    }

    let initial_capacity = declared_size.unwrap_or(0).min(64 * 1024) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(EmoteError::ImageTooLarge { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn response(headers: &str, body: &[u8]) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let mut wire = headers.as_bytes().to_vec();
        wire.extend_from_slice(body);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "request closed before its headers");
                request.extend_from_slice(&chunk[..count]);
                assert!(request.len() <= 16 * 1024);
            }
            let _ = stream.write_all(&wire).await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn oversized_content_length_is_rejected_without_waiting_for_its_body() {
        let response = response(
            "HTTP/1.1 200 OK\r\nContent-Length: 65\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        assert!(matches!(
            read_image_response_with_limit(response, 64).await,
            Err(EmoteError::ImageTooLarge { limit: 64 })
        ));
    }

    #[tokio::test]
    async fn chunked_body_is_rejected_when_cumulative_bytes_cross_the_limit() {
        let response = response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            b"4\r\nabcd\r\n4\r\nefgh\r\n1\r\ni\r\n0\r\n\r\n",
        )
        .await;
        assert!(matches!(
            read_image_response_with_limit(response, 8).await,
            Err(EmoteError::ImageTooLarge { limit: 8 })
        ));
    }

    #[tokio::test]
    async fn known_and_chunked_bodies_at_the_limit_are_preserved() {
        for (headers, body) in [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n",
                &b"abcdefgh"[..],
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                &b"4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n"[..],
            ),
        ] {
            let response = response(headers, body).await;
            assert_eq!(
                read_image_response_with_limit(response, 8).await.unwrap(),
                b"abcdefgh"
            );
        }
    }

    #[tokio::test]
    async fn body_without_a_length_header_is_also_bounded() {
        let response = response("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", b"abcdefghi").await;
        assert!(matches!(
            read_image_response_with_limit(response, 8).await,
            Err(EmoteError::ImageTooLarge { limit: 8 })
        ));
    }

    #[tokio::test]
    async fn unsuccessful_status_is_reported_before_reading_the_body() {
        let response = response(
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 65\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        assert!(matches!(
            read_image_response_with_limit(response, 64).await,
            Err(EmoteError::Http(error)) if error.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
        ));
    }
}
