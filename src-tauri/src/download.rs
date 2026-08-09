// Resumable background file downloads for the on-device models (Whisper, Qwen,
// Kokoro). Streams to a `<dst>.part` temp and renames into place only once the
// whole file has arrived, so a file at `dst` is always complete. If a `.part`
// from an interrupted download is present, it resumes from there via an HTTP
// range request; a server that ignores ranges falls back to a fresh download.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use reqwest::header::RANGE;
use reqwest::StatusCode;

/// Download `url` to `dst`, resuming a partial `<dst>.part` when the server
/// supports it. `on_progress(downloaded, total)` reports cumulative bytes; `total`
/// is 0 when the server sends no length.
pub async fn to_file(url: &str, dst: &Path, on_progress: impl Fn(u64, u64)) -> Result<()> {
    let part = dst.with_extension("part");
    let client = reqwest::Client::new();
    // Bytes already fetched by a prior (interrupted) attempt.
    let mut resume_from = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    loop {
        let mut req = client.get(url);
        if resume_from > 0 {
            req = req.header(RANGE, format!("bytes={resume_from}-"));
        }
        let mut resp = req.send().await?;
        let status = resp.status();

        // Our saved offset is past the end (stale/oversized partial): discard and
        // restart from scratch.
        if status == StatusCode::RANGE_NOT_SATISFIABLE && resume_from > 0 {
            let _ = std::fs::remove_file(&part);
            resume_from = 0;
            continue;
        }
        if !status.is_success() {
            return Err(anyhow!("download {url} failed: HTTP {status}"));
        }

        // 206 => the range was honored, append to the partial. Anything else
        // (typically 200) means the server ignored it, so start the file over.
        let resuming = resume_from > 0 && status == StatusCode::PARTIAL_CONTENT;
        // On a 206 the Content-Length is the REMAINING bytes, so add what we have.
        let body_len = resp.content_length().unwrap_or(0);
        let total = if resuming {
            resume_from + body_len
        } else {
            body_len
        };

        if let Some(dir) = part.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let mut file = if resuming {
            std::fs::OpenOptions::new().append(true).open(&part)?
        } else {
            std::fs::File::create(&part)? // truncates any non-resumable partial
        };

        let mut written = if resuming { resume_from } else { 0 };
        let mut last_emit = written;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk)?;
            written += chunk.len() as u64;
            // Throttle to ~1 MB steps so we don't flood the event bus.
            if written - last_emit >= 1_000_000 {
                on_progress(written, total);
                last_emit = written;
            }
        }
        file.flush().ok();
        drop(file);

        // Only publish a complete file — guards a silently truncated stream from
        // being renamed into place as if it were whole.
        if total > 0 && written != total {
            return Err(anyhow!(
                "download {url} incomplete: {written}/{total} bytes"
            ));
        }
        std::fs::rename(&part, dst)?;
        on_progress(written, total.max(written));
        return Ok(());
    }
}

/// Delete orphaned `*.part` temp files in `dirs` — leftovers from interrupted
/// downloads of models the user no longer has selected — while keeping any whose
/// path is in `keep` (the currently configured models, whose downloads may still
/// resume from their partial).
pub fn sweep_stale_parts(dirs: &[PathBuf], keep: &HashSet<PathBuf>) {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "part")
                && !keep.contains(&p)
                && std::fs::remove_file(&p).is_ok()
            {
                log::info!("download: removed stale temp {}", p.display());
            }
        }
    }
}
