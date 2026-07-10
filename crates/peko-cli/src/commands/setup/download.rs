//! Streaming download of a release asset to a temporary file.

use std::io::Write;

use super::{Result, SetupError};

/// Download `url` to a temp file, invoking `on_progress(downloaded, total)` as
/// bytes arrive. The body is streamed, so large assets do not buffer in memory.
pub async fn download_to_temp(
    client: &reqwest::Client,
    url: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<tempfile::NamedTempFile> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| SetupError::Network(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(SetupError::HttpStatus {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }

    let total = response.content_length();
    let mut file =
        tempfile::NamedTempFile::new().map_err(|e| SetupError::io("create temp file", e))?;
    let mut downloaded: u64 = 0;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SetupError::Network(e.to_string()))?
    {
        file.write_all(&chunk)
            .map_err(|e| SetupError::io("write temp file", e))?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }

    Ok(file)
}
