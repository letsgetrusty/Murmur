// Speech-to-text via local `whisper-rs` (whisper.cpp, Metal). `Transcriber` is
// the seam; a hand-rolled Pin<Box<Future>> avoids pulling in `async-trait`.

use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a>;
}

// -----------------------------------------------------------------------------
// Local, on-device Whisper via whisper-rs (whisper.cpp, Metal on Apple Silicon).
// -----------------------------------------------------------------------------

/// Directory holding downloaded whisper.cpp GGML models.
pub fn models_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME env var unset")?;
    Ok(PathBuf::from(home).join("Library/Application Support/openwispr/models"))
}

/// Local path for a whisper.cpp GGML model by short name (e.g. "small.en").
pub fn model_path(name: &str) -> Result<PathBuf> {
    Ok(models_dir()?.join(format!("ggml-{name}.bin")))
}

/// Ensure the given local Whisper model is present, downloading it from the
/// whisper.cpp model repo on Hugging Face if missing. Returns the file path.
/// Downloads to a `.part` file and renames into place, so a model that exists
/// at the final path is always complete.
pub async fn ensure_local_model(name: &str) -> Result<PathBuf> {
    let path = model_path(name)?;
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(models_dir()?).context("create models dir")?;
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin");
    log::info!("stt: downloading Whisper model '{name}' from {url}");
    let mut resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("GET whisper model")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "download whisper model '{name}' failed: HTTP {}",
            resp.status()
        ));
    }
    let part = path.with_extension("part");
    let mut file = fs::File::create(&part).context("create model .part")?;
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("read model chunk")? {
        file.write_all(&chunk).context("write model chunk")?;
        written += chunk.len() as u64;
    }
    file.flush().ok();
    drop(file);
    fs::rename(&part, &path).context("finalize model file")?;
    log::info!("stt: Whisper model '{name}' ready ({written} bytes)");
    Ok(path)
}

/// On-device Whisper. The model is loaded lazily on first transcribe and cached
/// for the session, so app idle before the first dictation stays light.
pub struct WhisperStt {
    model_name: String,
    ctx: Mutex<Option<Arc<WhisperContext>>>,
}

impl WhisperStt {
    pub fn new(model_name: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            ctx: Mutex::new(None),
        }
    }

    /// Load (once) and return the cached whisper context.
    fn context(&self) -> Result<Arc<WhisperContext>> {
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| anyhow!("whisper ctx poisoned"))?;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let path = model_path(&self.model_name)?;
        if !path.exists() {
            return Err(anyhow!(
                "local Whisper model '{}' not found at {} — it downloads on startup; wait a moment and retry, or run ./scripts/setup.sh",
                self.model_name,
                path.display()
            ));
        }
        let ctx = WhisperContext::new_with_params(&path, WhisperContextParameters::default())
            .map_err(|e| anyhow!("load whisper model '{}': {e}", self.model_name))?;
        let ctx = Arc::new(ctx);
        *guard = Some(ctx.clone());
        Ok(ctx)
    }
}

impl Transcriber for WhisperStt {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        Box::pin(async move {
            let ctx = self.context()?;
            let samples = wav_to_mono_f32(wav)?;
            // whisper.cpp is a blocking CPU/GPU job — keep it off the async
            // runtime's worker threads.
            let text = tokio::task::spawn_blocking(move || -> Result<String> {
                let mut state = ctx
                    .create_state()
                    .map_err(|e| anyhow!("whisper state: {e}"))?;
                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                // Pin to English (matches the cloud path) and silence whisper's
                // stdout chatter.
                params.set_language(Some("en"));
                params.set_translate(false);
                params.set_print_special(false);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_timestamps(false);
                state
                    .full(params, &samples)
                    .map_err(|e| anyhow!("whisper inference: {e}"))?;
                let mut out = String::new();
                for segment in state.as_iter() {
                    out.push_str(
                        &segment
                            .to_str_lossy()
                            .map_err(|e| anyhow!("segment: {e}"))?,
                    );
                }
                Ok(out)
            })
            .await
            .context("whisper task join")??;
            Ok(text.trim().to_string())
        })
    }
}

/// Decode our own capture WAV (16 kHz mono 16-bit PCM from audio.rs) into the
/// f32 [-1, 1] samples whisper-rs expects. Averages channels if the buffer ever
/// carries more than one.
fn wav_to_mono_f32(wav: &[u8]) -> Result<Vec<f32>> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(anyhow!("not a RIFF/WAVE buffer"));
    }
    let mut channels: u16 = 1;
    let mut bits: u16 = 16;
    let mut data: Option<&[u8]> = None;
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        let body = pos + 8;
        let end = (body + size).min(wav.len());
        match id {
            b"fmt " if end - body >= 16 => {
                channels = u16::from_le_bytes([wav[body + 2], wav[body + 3]]);
                bits = u16::from_le_bytes([wav[body + 14], wav[body + 15]]);
            }
            b"data" => data = Some(&wav[body..end]),
            _ => {}
        }
        pos = body + size + (size & 1); // chunks are word-aligned
    }
    let data = data.ok_or_else(|| anyhow!("WAV has no data chunk"))?;
    if bits != 16 {
        return Err(anyhow!("expected 16-bit PCM, got {bits}-bit"));
    }
    let ch = channels.max(1) as usize;
    let mut out = Vec::with_capacity(data.len() / 2 / ch);
    for frame in data.chunks_exact(2 * ch) {
        let mut acc = 0f32;
        for c in 0..ch {
            acc += i16::from_le_bytes([frame[c * 2], frame[c * 2 + 1]]) as f32 / 32768.0;
        }
        out.push(acc / ch as f32);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 16 kHz mono 16-bit WAV around the given samples.
    fn wav16(samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&16_000u32.to_le_bytes());
        w.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
        w.extend_from_slice(&2u16.to_le_bytes()); // block align
        w.extend_from_slice(&16u16.to_le_bytes()); // bits
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    }

    #[test]
    fn decodes_mono_pcm_to_f32() {
        let wav = wav16(&[0, i16::MAX, i16::MIN, 16384]);
        let f = wav_to_mono_f32(&wav).unwrap();
        assert_eq!(f.len(), 4);
        assert!((f[0] - 0.0).abs() < 1e-6);
        assert!((f[1] - 1.0).abs() < 1e-3);
        assert!((f[2] + 1.0).abs() < 1e-3);
        assert!((f[3] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn rejects_non_wav() {
        assert!(wav_to_mono_f32(b"not a wav at all").is_err());
    }

    /// End-to-end local transcription against a real WAV fixture + model.
    /// Ignored by default (needs the ~0.5 GB model on disk). Run manually:
    ///   OPENWISPR_TEST_WAV=/path/to/jfk.wav \
    ///     cargo test --no-default-features -- --ignored --nocapture transcribes_fixture
    #[test]
    #[ignore = "needs a local Whisper model + a 16 kHz WAV fixture (OPENWISPR_TEST_WAV)"]
    fn transcribes_fixture() {
        let wav_path = std::env::var("OPENWISPR_TEST_WAV").expect("set OPENWISPR_TEST_WAV");
        let model = std::env::var("OPENWISPR_TEST_MODEL").unwrap_or_else(|_| "small.en".into());
        let wav = std::fs::read(&wav_path).unwrap();
        let stt = WhisperStt::new(model);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let text = rt.block_on(stt.transcribe(&wav)).unwrap();
        eprintln!("TRANSCRIPT: {text}");
        assert!(!text.is_empty(), "transcript should not be empty");
    }
}
