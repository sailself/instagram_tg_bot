//! Media download with a hard size cap (PLAN §6). Streams to disk and aborts
//! once the cap is exceeded so a huge file can't blow the 1 GB budget. Temp
//! dirs are managed by the caller via `tempfile::TempDir` (RAII cleanup).

use crate::extract::MediaKind;
use reqwest::Client;
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub enum DownloadError {
    TooBig,
    Failed(String),
}

/// Stream `url` into `dest`, failing fast with `TooBig` past `max_bytes`.
pub async fn download_capped(
    http: &Client,
    url: &str,
    dest: &Path,
    max_bytes: u64,
    user_agent: &str,
) -> Result<(), DownloadError> {
    let started = std::time::Instant::now();
    let resp = http
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .send()
        .await
        .map_err(|e| DownloadError::Failed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(DownloadError::Failed(format!("http {}", resp.status())));
    }
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(DownloadError::TooBig);
        }
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| DownloadError::Failed(e.to_string()))?;
    let mut written: u64 = 0;
    let mut resp = resp;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| DownloadError::Failed(e.to_string()))?
    {
        written += chunk.len() as u64;
        if written > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(DownloadError::TooBig);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::Failed(e.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|e| DownloadError::Failed(e.to_string()))?;
    tracing::debug!(bytes = written, elapsed_ms = started.elapsed().as_millis() as u64, "downloaded");
    Ok(())
}

pub fn temp_filename(shortcode: &str, idx: usize, kind: MediaKind) -> String {
    let ext = match kind {
        MediaKind::Video => "mp4",
        MediaKind::Image => "jpg",
    };
    format!("{shortcode}_{idx}.{ext}")
}
