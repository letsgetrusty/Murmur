// Speech-to-text via local `whisper-rs` (whisper.cpp, Metal). `Transcriber` is
// the seam; a hand-rolled Pin<Box<Future>> avoids pulling in `async-trait`.

use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Transcriber: Send + Sync {
    /// `lang` is a whisper language code ("en", "nl", …). It is always pinned
    /// by the caller rather than auto-detected: on a mismatch whisper doesn't
    /// fail, it emits confident text shaped like the language you asked for.
    fn transcribe<'a>(&'a self, wav: &'a [u8], lang: &'a str) -> TranscribeFuture<'a>;

    /// Preload the model off the calling thread so the first `transcribe`
    /// doesn't pay the load. Default no-op; safe to call repeatedly (it's a
    /// cheap check once the model is resident).
    fn warm(&self) {}
}

// -----------------------------------------------------------------------------
// Local, on-device Whisper via whisper-rs (whisper.cpp, Metal on Apple Silicon).
// -----------------------------------------------------------------------------

/// The language code meaning "let whisper detect it" (whisper.cpp treats this
/// value specially — see `whisper_full_with_state`).
pub const AUTO_LANGUAGE: &str = "auto";

/// A language whisper can transcribe: `("nl", "dutch")`.
pub type Language = (&'static str, &'static str);

/// Every language the linked whisper build supports, sorted by name. Read from
/// the library itself rather than a hardcoded list, so it can't drift from what
/// the model actually accepts.
pub fn languages() -> Vec<Language> {
    let mut out: Vec<Language> = (0..=whisper_rs::get_lang_max_id())
        .filter_map(|id| {
            Some((
                whisper_rs::get_lang_str(id)?,
                whisper_rs::get_lang_str_full(id)?,
            ))
        })
        .collect();
    out.sort_by_key(|(_, name)| *name);
    out
}

/// True if whisper accepts `lang` — a known code, or `auto` for detection.
/// Guards hand-edited config: an unknown code would otherwise reach whisper and
/// produce nonsense rather than an error.
pub fn is_valid_language(lang: &str) -> bool {
    lang == AUTO_LANGUAGE || whisper_rs::get_lang_id(lang).is_some()
}

/// Directory holding downloaded whisper.cpp GGML models.
pub fn models_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME env var unset")?;
    Ok(PathBuf::from(home).join("Library/Application Support/murmur/models"))
}

/// Local path for a whisper.cpp GGML model by short name (e.g. "small").
pub fn model_path(name: &str) -> Result<PathBuf> {
    Ok(models_dir()?.join(format!("ggml-{name}.bin")))
}

/// Ensure the given local Whisper model is present, downloading it from the
/// whisper.cpp model repo on Hugging Face if missing. Returns the file path. The
/// download streams to a `.part` file (resumable) and renames into place only when
/// complete, so a model that exists at the final path is always whole.
pub async fn ensure_local_model(name: &str, on_progress: impl Fn(u64, u64)) -> Result<PathBuf> {
    let path = model_path(name)?;
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(models_dir()?).context("create models dir")?;
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin");
    log::info!("stt: downloading Whisper model '{name}' from {url}");
    crate::download::to_file(&url, &path, on_progress).await?;
    log::info!("stt: Whisper model '{name}' ready");
    Ok(path)
}

/// On-device Whisper. The model is loaded lazily on first transcribe and cached
/// for the session, so app idle before the first dictation stays light.
pub struct WhisperStt {
    model_name: String,
    // Behind an `Arc` so `warm` can hand the cell to a background thread.
    ctx: Arc<Mutex<Option<Arc<WhisperContext>>>>,
}

impl WhisperStt {
    pub fn new(model_name: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            ctx: Arc::new(Mutex::new(None)),
        }
    }

    /// Load (once) and return the cached whisper context.
    fn context(&self) -> Result<Arc<WhisperContext>> {
        load_context(&self.model_name, &self.ctx)
    }
}

/// Load the whisper context for `model_name` into `cell`, returning the cached
/// one on later calls. The lock is held across the load so a concurrent
/// `warm` + `transcribe` can't load the model twice.
fn load_context(
    model_name: &str,
    cell: &Mutex<Option<Arc<WhisperContext>>>,
) -> Result<Arc<WhisperContext>> {
    let mut guard = cell.lock().map_err(|_| anyhow!("whisper ctx poisoned"))?;
    if let Some(c) = guard.as_ref() {
        return Ok(c.clone());
    }
    let ctx = open_context(model_name)?;
    *guard = Some(ctx.clone());
    Ok(ctx)
}

/// Open a fresh whisper context — reads the ~150 MB weights and inits the Metal
/// backend. No caching; callers own the cell. Split out so `warm` can hold the
/// cell lock across load + warm-inference + publish (see `warm`).
fn open_context(model_name: &str) -> Result<Arc<WhisperContext>> {
    let path = model_path(model_name)?;
    if !path.exists() {
        return Err(anyhow!(
            "local Whisper model '{}' not found at {} — it downloads on startup; wait a moment and retry, or run ./scripts/setup.sh",
            model_name,
            path.display()
        ));
    }
    let mut cparams = WhisperContextParameters::default();
    // Flash attention runs faster fused attention kernels on Metal. Its only
    // incompatibility is DTW token timestamps, which we don't use.
    cparams.flash_attn(true);
    let ctx = WhisperContext::new_with_params(&path, cparams)
        .map_err(|e| anyhow!("load whisper model '{}': {e}", model_name))?;
    // The ggml Metal backend is now initialized; register the process-exit guard
    // here (not at startup) so it lands after ggml's static destructor in atexit's
    // LIFO order and reliably fires first, skipping ggml's aborting teardown.
    crate::install_exit_guard();
    Ok(Arc::new(ctx))
}

/// The whisper decode params shared by warm-up and real transcription, kept in
/// one place so the two can't drift. Pin the caller's language (which hotkey
/// fired); a dictation clip is one self-contained utterance, so don't seed the
/// decoder with prior-window text, and pin a single temperature so a hard clip
/// can't trip whisper's temperature-fallback retries (which re-decode and spike
/// latency). Give it the machine's cores, and silence whisper's stdout chatter.
fn set_dictation_params<'a>(params: &mut FullParams<'a, '_>, lang: &'a str) {
    params.set_language(Some(lang));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_temperature(0.0);
    params.set_temperature_inc(0.0);
    params.set_n_threads(transcribe_threads());
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
}

/// Run one throwaway inference on ~0.5 s of silence to force whisper's Metal
/// graph compile + state allocation up front, so the first real dictation hits a
/// fully-warm engine rather than paying that one-time cost. Uses the same
/// no-fallback params as `transcribe` so it stays fast and deterministic.
fn warm_infer(ctx: &WhisperContext) -> Result<()> {
    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow!("whisper warm state: {e}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    // Any valid code works here: this decodes silence and throws the result
    // away, and what we're forcing (Metal graph compile + state alloc) doesn't
    // depend on the language token.
    set_dictation_params(&mut params, crate::config::DEFAULT_STT_LANGUAGE);
    let silence = vec![0f32; 16_000 / 2];
    state
        .full(params, &silence)
        .map_err(|e| anyhow!("whisper warm inference: {e}"))?;
    Ok(())
}

/// Threads for whisper's CPU-side work (Metal handles the heavy matmuls).
/// whisper defaults to `min(4, ncpu)`; allow a few more but cap so we don't
/// oversubscribe the efficiency cores.
fn transcribe_threads() -> std::os::raw::c_int {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8) as std::os::raw::c_int)
        .unwrap_or(4)
}

/// Drop whisper.cpp's non-speech placeholders — "[BLANK_AUDIO]", "[ Silence ]",
/// "(music)", "[ Inaudible ]", etc. — that it emits for silent/non-speech audio.
/// Only bracketed/parenthesized spans containing a non-speech keyword are
/// removed, so ordinary dictation is untouched; if silence was all that was
/// "heard", the result is empty and the caller pastes nothing.
fn strip_nonspeech(text: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "blank_audio",
        "blank audio",
        "silence",
        "silent",
        "no speech",
        "music",
        "applause",
        "inaudible",
        "noise",
        "laughter",
        "laughs",
        "pause",
        "background",
        "sound",
        "static",
        "beep",
        "click",
        "typing",
        "wind",
        "coughing",
        "breathing",
        "sighs",
        "ringing",
    ];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let close = match chars[i] {
            '[' => Some(']'),
            '(' => Some(')'),
            _ => None,
        };
        if let Some(close) = close {
            if let Some(j) = (i + 1..chars.len()).find(|&k| chars[k] == close) {
                let inner: String = chars[i + 1..j]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                if KEYWORDS.iter().any(|k| inner.contains(k)) {
                    i = j + 1; // drop the whole "[…]" / "(…)" span
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    // Collapse the whitespace left where spans were removed, and trim.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Transcriber for WhisperStt {
    fn transcribe<'a>(&'a self, wav: &'a [u8], lang: &'a str) -> TranscribeFuture<'a> {
        Box::pin(async move {
            // Instrumentation: split the pre-inference setup so an intermittent
            // slow transcribe can be pinned to a layer — ctx acquire (model-lock
            // contention / reload), WAV decode, time queued for a blocking thread
            // (pool saturation), and whisper state alloc — vs. the inference
            // itself. See the summary log below.
            let t_enter = std::time::Instant::now();
            let ctx = self.context()?;
            let ctx_ms = t_enter.elapsed().as_secs_f32() * 1000.0;
            let t_decode = std::time::Instant::now();
            let samples = wav_to_mono_f32(wav)?;
            let decode_ms = t_decode.elapsed().as_secs_f32() * 1000.0;
            let audio_secs = samples.len() as f32 / 16_000.0;
            // Owned: the blocking closure below outlives this borrow.
            let lang = lang.to_string();
            // whisper.cpp is a blocking CPU/GPU job — keep it off the async
            // runtime's worker threads.
            let t_spawn = std::time::Instant::now();
            let (text, infer_secs, queue_ms, state_ms) =
                tokio::task::spawn_blocking(move || -> Result<(String, f32, f32, f32)> {
                    // Time spent waiting for a free thread in the blocking pool
                    // before this closure ran; seconds here means the pool was
                    // saturated (a concurrent transcribe/refine) and we queued.
                    let queue_ms = t_spawn.elapsed().as_secs_f32() * 1000.0;
                    let t_state = std::time::Instant::now();
                    let mut state = ctx
                        .create_state()
                        .map_err(|e| anyhow!("whisper state: {e}"))?;
                    let state_ms = t_state.elapsed().as_secs_f32() * 1000.0;
                    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                    set_dictation_params(&mut params, &lang);
                    let t0 = std::time::Instant::now();
                    state
                        .full(params, &samples)
                        .map_err(|e| anyhow!("whisper inference: {e}"))?;
                    let infer_secs = t0.elapsed().as_secs_f32();
                    let mut out = String::new();
                    for segment in state.as_iter() {
                        out.push_str(
                            &segment
                                .to_str_lossy()
                                .map_err(|e| anyhow!("segment: {e}"))?,
                        );
                    }
                    Ok((out, infer_secs, queue_ms, state_ms))
                })
                .await
                .context("whisper task join")??;
            log::info!(
                "stt: transcribed {:.1}s of audio in {:.0}ms ({:.1}x realtime) [setup: ctx {:.0}ms, decode {:.0}ms, blocking-queue {:.0}ms, state {:.0}ms]",
                audio_secs,
                infer_secs * 1000.0,
                audio_secs / infer_secs.max(1e-3),
                ctx_ms,
                decode_ms,
                queue_ms,
                state_ms,
            );
            Ok(strip_nonspeech(&text))
        })
    }

    fn warm(&self) {
        // Already resident? Nothing to do (cheap lock, dropped before return).
        if self.ctx.lock().map(|g| g.is_some()).unwrap_or(false) {
            return;
        }
        // Warm on a throwaway thread so callers (hotkey press, startup) never
        // block on the ~1s model read.
        let cell = self.ctx.clone();
        let name = self.model_name.clone();
        std::thread::spawn(move || {
            // Hold the cell lock across load → warm-inference → publish. A
            // concurrent transcribe fetches the ctx through the same lock, so it
            // can't touch the Metal backend until we've published a fully-warm
            // context — no concurrent inference on one context, and the first
            // real dictation skips both the weight load and the graph compile.
            let mut guard = match cell.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if guard.is_some() {
                return; // loaded (and warmed) by whoever got here first
            }
            let ctx = match open_context(&name) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("stt: warm-load failed: {e}");
                    return;
                }
            };
            if let Err(e) = warm_infer(&ctx) {
                log::warn!("stt: warm inference failed: {e}");
            }
            *guard = Some(ctx);
            log::info!("stt: model warmed (weights + graph)");
        });
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

    #[test]
    fn strips_whisper_nonspeech_placeholders() {
        // Silence-only transcripts collapse to empty → nothing gets pasted.
        assert_eq!(strip_nonspeech("[BLANK_AUDIO]"), "");
        assert_eq!(strip_nonspeech("  [ Silence ]  "), "");
        assert_eq!(strip_nonspeech("(music)"), "");
        assert_eq!(strip_nonspeech("[ Inaudible ]"), "");
        // Mixed: keep the speech, drop the annotation.
        assert_eq!(
            strip_nonspeech("Take out the trash [BLANK_AUDIO]"),
            "Take out the trash"
        );
        // Ordinary dictation is untouched, including harmless parentheses.
        assert_eq!(strip_nonspeech("Hello there, world"), "Hello there, world");
        assert_eq!(
            strip_nonspeech("the total (net) is five"),
            "the total (net) is five"
        );
    }

    #[test]
    fn languages_come_from_the_whisper_build() {
        let langs = languages();
        assert!(!langs.is_empty(), "whisper reports no languages");
        // The two Murmur defaults must be among them.
        assert!(langs.iter().any(|(c, _)| *c == "en"));
        assert!(langs.iter().any(|(c, _)| *c == "nl"));
        // Sorted by display name so the Settings dropdown reads sensibly.
        let names: Vec<&str> = langs.iter().map(|(_, n)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn validates_language_codes_including_auto() {
        assert!(is_valid_language("en"));
        assert!(is_valid_language("nl"));
        assert!(is_valid_language(AUTO_LANGUAGE));
        // whisper_lang_id also accepts the full English name, not just the
        // code (whisper.cpp:3987 falls back to a name match), so this is valid.
        assert!(is_valid_language("dutch"));
        // A plausible typo must not sail through into whisper.
        assert!(!is_valid_language("nk"));
        assert!(!is_valid_language(""));
        assert!(!is_valid_language("nederlands"));
    }

    /// End-to-end local transcription against a real WAV fixture + model.
    /// Ignored by default (needs the ~0.5 GB model on disk). Run manually:
    ///   MURMUR_TEST_WAV=/path/to/jfk.wav \
    ///     cargo test --no-default-features -- --ignored --nocapture transcribes_fixture
    /// `MURMUR_TEST_MODEL` / `MURMUR_TEST_LANG` override the model and language,
    /// which is also how you A/B two models over the same clip.
    #[test]
    #[ignore = "needs a local Whisper model + a 16 kHz WAV fixture (MURMUR_TEST_WAV)"]
    fn transcribes_fixture() {
        let wav_path = std::env::var("MURMUR_TEST_WAV").expect("set MURMUR_TEST_WAV");
        let model = std::env::var("MURMUR_TEST_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_STT_MODEL.into());
        let lang = std::env::var("MURMUR_TEST_LANG")
            .unwrap_or_else(|_| crate::config::DEFAULT_STT_LANGUAGE.into());
        let wav = std::fs::read(&wav_path).unwrap();
        let stt = WhisperStt::new(model);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let text = rt.block_on(stt.transcribe(&wav, &lang)).unwrap();
        eprintln!("TRANSCRIPT: {text}");
        assert!(!text.is_empty(), "transcript should not be empty");
    }
}
