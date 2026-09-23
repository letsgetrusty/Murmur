// Speech-to-text via local `whisper-rs` (whisper.cpp, Metal). `Transcriber` is
// the seam; a hand-rolled Pin<Box<Future>> avoids pulling in `async-trait`.

use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

mod streaming;
pub use streaming::LiveTranscription;

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a>;

    /// Decode a pause-delimited chunk; backends can defer corrections until
    /// all chunks are joined, so multi-word corrections still span pauses.
    fn transcribe_chunk<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        self.transcribe(wav)
    }

    fn finish_chunks(&self, chunks: &[String]) -> String {
        chunks.join(" ").trim().to_string()
    }

    /// Preload the model off the calling thread so the first `transcribe`
    /// doesn't pay the load. Default no-op; safe to call repeatedly (it's a
    /// cheap check once the model is resident).
    fn warm(&self) {}

    /// Set the vocabulary/"initial prompt" that biases the decoder toward the
    /// user's spellings (proper nouns, jargon). Applied on the next transcribe;
    /// no reload. Default no-op for impls without prompt biasing.
    fn set_vocabulary(&self, _vocabulary: &str) {}

    /// Set deterministic post-transcription corrections ("spoken form =>
    /// replacement" lines) for terms biasing can't land reliably. Applied on the
    /// next transcribe; no reload. Default no-op.
    fn set_corrections(&self, _rules: &str) {}
}

/// Parse the correction rules config — one `spoken form => replacement` per
/// line, `#` comments and blanks skipped — into (pattern, replacement) pairs.
/// Patterns keep their original text; matching lowercases per-char at compare
/// time (`apply_corrections`), so store them as written.
fn parse_corrections(rules: &str) -> Vec<(String, String)> {
    rules
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (pat, rep) = line.split_once("=>")?;
            let pat = pat.trim();
            if pat.is_empty() {
                return None;
            }
            Some((pat.to_string(), rep.trim().to_string()))
        })
        .collect()
}

/// Apply the correction rules to `text`: case-insensitive (ASCII) whole-word
/// find/replace, so a rule for "c" never rewrites inside "cat" and multi-word
/// patterns ("rust see") match across the spaces. Rules apply in order.
fn apply_corrections(text: &str, rules: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (pat, rep) in rules {
        out = replace_whole_word_ci(&out, pat, rep);
    }
    out
}

/// One rule: replace every whole-word, ASCII-case-insensitive occurrence of
/// `needle` in `haystack` with `replacement`. A match at `i..j` is whole-word
/// when the chars just outside it are non-alphanumeric (or a string edge), so
/// substrings inside larger words are left alone.
fn replace_whole_word_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    let hay: Vec<char> = haystack.chars().collect();
    let need: Vec<char> = needle.chars().collect();
    if need.is_empty() {
        return haystack.to_string();
    }
    let bounded_before = |i: usize| i == 0 || !hay[i - 1].is_alphanumeric();
    let bounded_after = |j: usize| j == hay.len() || !hay[j].is_alphanumeric();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < hay.len() {
        let j = i + need.len();
        let matches = j <= hay.len()
            && bounded_before(i)
            && bounded_after(j)
            && hay[i..j]
                .iter()
                .zip(&need)
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if matches {
            out.push_str(replacement);
            i = j;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

// -----------------------------------------------------------------------------
// Local, on-device Whisper via whisper-rs (whisper.cpp, Metal on Apple Silicon).
// -----------------------------------------------------------------------------

/// Directory holding downloaded whisper.cpp GGML models.
pub fn models_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME env var unset")?;
    Ok(PathBuf::from(home).join("Library/Application Support/murmur/models"))
}

/// Local path for a whisper.cpp GGML model by short name (e.g. "small.en").
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
    // Vocabulary/"initial prompt" biasing text; read (and tokenized) fresh on
    // each transcribe, so `set_vocabulary` applies without a reload.
    vocabulary: Mutex<String>,
    decoder: Arc<Mutex<Option<WhisperState>>>,
    // Parsed deterministic corrections applied to each transcript; updated by
    // `set_corrections` so edits apply without a reload.
    corrections: Mutex<Vec<(String, String)>>,
}

impl WhisperStt {
    pub fn new(model_name: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            ctx: Arc::new(Mutex::new(None)),
            vocabulary: Mutex::new(String::new()),
            decoder: Arc::new(Mutex::new(None)),
            corrections: Mutex::new(Vec::new()),
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

/// First-pass decode temperature. 0 = deterministic and fast; the common case
/// decodes in one pass at this temperature.
const DICTATION_TEMPERATURE: f32 = 0.0;
/// Temperature-fallback step. MUST stay > 0 — it enables whisper's fallback
/// ladder so a hard clip that fails the entropy/logprob thresholds retries at a
/// higher temperature instead of returning an empty/garbage transcript. Setting
/// this to 0 silently dropped whole dictations (a 22 s clip decoding to nothing).
const DICTATION_TEMPERATURE_INC: f32 = 0.2;

// Compile-time guard: a zero step disables whisper's fallback ladder, which
// silently dropped whole dictations. Don't "fix" a build failure here by
// lowering the value to 0 — that reintroduces the empty-transcript bug.
const _: () = assert!(
    DICTATION_TEMPERATURE_INC > 0.0,
    "temperature_inc must stay > 0 so whisper's temperature fallback stays on"
);

/// The whisper decode params shared by warm-up and real transcription, kept in
/// one place so the two can't drift. Pin to English (matches the cloud path); a
/// dictation clip is one self-contained utterance, so don't seed the decoder with
/// prior-window text. Start at temperature 0 (deterministic, fast), but keep
/// whisper's temperature fallback on (see `DICTATION_TEMPERATURE_INC`), so a hard
/// clip recovers instead of pasting nothing. Give it the machine's cores, and
/// silence whisper's stdout chatter.
fn set_dictation_params(params: &mut FullParams) {
    params.set_language(Some("en"));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_temperature(DICTATION_TEMPERATURE);
    params.set_temperature_inc(DICTATION_TEMPERATURE_INC);
    params.set_n_threads(transcribe_threads());
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
}

/// Run one warm-up inference on ~0.5 s of silence to force whisper's Metal
/// graph compile + state allocation up front, so the first real dictation hits a
/// fully-warm engine rather than paying that one-time cost. Uses the same
/// fallback-enabled params as `transcribe` so it stays fast and deterministic.
fn warm_infer(state: &mut WhisperState) -> Result<()> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    set_dictation_params(&mut params);
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

/// Turn Whisper's raw output into the text we paste. This is the single ordered
/// pipeline for every transcript — keep all post-decode cleanup here (don't
/// inline steps at the call site) so the order is one place and the composition
/// tests below guard the whole thing against regressions:
///   1. drop non-speech placeholders ("[BLANK_AUDIO]" …),
///   2. strip Whisper's leading dialogue dash (a lone dash → empty),
///   3. apply the user's deterministic corrections.
///
/// An empty result means the caller pastes nothing.
fn finalize_transcript(raw: &str, rules: &[(String, String)]) -> String {
    let cleaned = strip_nonspeech(raw);
    let cleaned = strip_leading_dialogue_dash(&cleaned);
    apply_corrections(cleaned, rules)
}

/// Strip Whisper's leading "dialogue dash": it prefixes some segments with a
/// dash/bullet (e.g. "- No, this…"), and on a near-silent clip emits a lone "-".
/// Both are artifacts, not speech — remove one leading marker plus any following
/// space. A lone marker collapses to empty, so the caller pastes nothing instead
/// of a stray "-". A marker glued to a digit ("-5") is kept, since that's a
/// negative number rather than the dialogue artifact.
fn strip_leading_dialogue_dash(text: &str) -> &str {
    let t = text.trim_start();
    let rest = ['-', '\u{2013}', '\u{2014}', '\u{2022}', '*']
        .iter()
        .find_map(|m| t.strip_prefix(*m));
    match rest {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => t,
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

/// Drop whisper.cpp's non-speech placeholders — "[BLANK_AUDIO]", "[ Silence ]",
/// "(music)", "[ Inaudible ]", "[END PLAYBACK]", etc. — that it emits for
/// silent/non-speech audio. A bracketed/parenthesized span is removed when it
/// contains a non-speech keyword, OR when it's a square-bracket span with
/// letters but no lowercase (whisper writes its annotations as ALL-CAPS square
/// brackets — [END], [TYPING] — which ordinary dictation never produces). Real
/// prose and lowercase parentheticals are left intact; if silence was all that
/// was "heard", the result is empty and the caller pastes nothing.
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
                let inner: String = chars[i + 1..j].iter().collect();
                let has_keyword = {
                    let lower = inner.to_ascii_lowercase();
                    KEYWORDS.iter().any(|k| lower.contains(k))
                };
                // Whisper annotates non-speech in ALL-CAPS square brackets
                // ([END PLAYBACK], [TYPING]); a `[...]` span with letters but no
                // lowercase is one of those, not dictated text.
                let is_caps_annotation = chars[i] == '['
                    && inner.chars().any(|c| c.is_ascii_alphabetic())
                    && !inner.chars().any(|c| c.is_ascii_lowercase());
                if has_keyword || is_caps_annotation {
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
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        Box::pin(async move {
            let raw = self.transcribe_chunk(wav).await?;
            Ok(self.finish_chunks(&[raw]))
        })
    }

    fn finish_chunks(&self, chunks: &[String]) -> String {
        let rules = self
            .corrections
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let cleaned: Vec<String> = chunks
            .iter()
            .map(|raw| finalize_transcript(raw, &[]))
            .filter(|s| !s.trim().is_empty())
            .collect();
        finalize_transcript(&cleaned.join(" "), &rules)
    }

    fn transcribe_chunk<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        Box::pin(async move {
            // Instrumentation: split the pre-inference setup so an intermittent
            // slow transcribe can be pinned to a layer — ctx acquire (model-lock
            // contention / reload), WAV decode, time queued for a blocking thread
            // (pool saturation), and whisper state alloc — vs. the inference
            // itself. See the summary log below.
            let decoder = self.decoder.clone();
            let t_enter = std::time::Instant::now();
            let ctx = self.context()?;
            let ctx_ms = t_enter.elapsed().as_secs_f32() * 1000.0;
            // Vocabulary biasing text (may be empty); moved into the blocking
            // closure and tokenized there against the loaded model.
            let vocabulary = self
                .vocabulary
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let t_decode = std::time::Instant::now();
            let samples = wav_to_mono_f32(wav)?;
            let decode_ms = t_decode.elapsed().as_secs_f32() * 1000.0;
            let audio_secs = samples.len() as f32 / 16_000.0;
            // whisper.cpp is a blocking CPU/GPU job — keep it off the async
            // runtime's worker threads.
            let t_spawn = std::time::Instant::now();
            let (text, infer_secs, queue_ms, state_ms, prompt_n) =
                tokio::task::spawn_blocking(move || -> Result<(String, f32, f32, f32, usize)> {
                    // Time spent waiting for a free thread in the blocking pool
                    // before this closure ran; seconds here means the pool was
                    // saturated (a concurrent transcribe/refine) and we queued.
                    let queue_ms = t_spawn.elapsed().as_secs_f32() * 1000.0;
                    let t_state = std::time::Instant::now();
                    let mut slot = decoder
                        .lock()
                        .map_err(|_| anyhow!("whisper decoder poisoned"))?;
                    if slot.is_none() {
                        *slot = Some(
                            ctx.create_state()
                                .map_err(|e| anyhow!("whisper state: {e}"))?,
                        );
                    }
                    let state = slot.as_mut().expect("decoder initialized");
                    let state_ms = t_state.elapsed().as_secs_f32() * 1000.0;
                    // Tokenize the vocabulary into whisper's prompt tokens (0.16
                    // has no `set_initial_prompt`; `set_tokens` is the seam). Must
                    // outlive `params`, which borrows the slice — declare it first.
                    // 224 is whisper's prompt budget; extra tokens are dropped.
                    let prompt_tokens: Vec<std::os::raw::c_int> = if vocabulary.trim().is_empty() {
                        Vec::new()
                    } else {
                        ctx.tokenize(&vocabulary, 224).unwrap_or_default()
                    };
                    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                    set_dictation_params(&mut params);
                    if !prompt_tokens.is_empty() {
                        params.set_tokens(&prompt_tokens);
                    }
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
                    Ok((out, infer_secs, queue_ms, state_ms, prompt_tokens.len()))
                })
                .await
                .context("whisper task join")??;
            log::info!(
                "stt: transcribed {:.1}s of audio in {:.0}ms ({:.1}x realtime) [setup: ctx {:.0}ms, decode {:.0}ms, blocking-queue {:.0}ms, state {:.0}ms, vocab {prompt_n} tok]",
                audio_secs,
                infer_secs * 1000.0,
                audio_secs / infer_secs.max(1e-3),
                ctx_ms,
                decode_ms,
                queue_ms,
                state_ms,
            );
            Ok(text)
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
        let decoder = self.decoder.clone();
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
            match ctx.create_state() {
                Ok(mut state) => {
                    if let Err(e) = warm_infer(&mut state) {
                        log::warn!("stt: warm inference failed: {e}");
                    }
                    if let Ok(mut slot) = decoder.lock() {
                        *slot = Some(state);
                    }
                }
                Err(e) => log::warn!("stt: warm state allocation failed: {e}"),
            }
            *guard = Some(ctx);
            log::info!("stt: model warmed (weights + graph)");
        });
    }

    fn set_vocabulary(&self, vocabulary: &str) {
        if let Ok(mut g) = self.vocabulary.lock() {
            *g = vocabulary.to_string();
        }
    }

    fn set_corrections(&self, rules: &str) {
        if let Ok(mut g) = self.corrections.lock() {
            *g = parse_corrections(rules);
        }
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
    fn chunk_corrections_apply_once_across_boundaries() {
        let stt = WhisperStt::new("unused");
        stt.set_corrections("rust see => rustc\nrustc => Rust compiler");
        assert_eq!(
            stt.finish_chunks(&[" - Use rust".into(), "see here [BLANK_AUDIO]".into()]),
            "Use Rust compiler here"
        );
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
        // ALL-CAPS square-bracket annotations whisper hallucinates on silence.
        assert_eq!(strip_nonspeech("[END PLAYBACK]"), "");
        assert_eq!(strip_nonspeech("[END]"), "");
        assert_eq!(
            strip_nonspeech("Take out the trash [END PLAYBACK]"),
            "Take out the trash"
        );
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

    /// Exercise the vocabulary-biasing path against a real model: tokenize the
    /// shipped dev-terms default and feed it through `set_tokens` + `full`, the
    /// exact sequence `transcribe` runs. Proves the whisper-rs API wiring works
    /// and the default fits whisper's ~224-token prompt budget. Ignored by
    /// default (needs the model on disk); run manually:
    ///   MURMUR_TEST_MODEL=small.en \
    ///     cargo test --no-default-features -- --ignored --nocapture vocabulary_
    #[test]
    #[ignore = "needs a local Whisper model on disk"]
    fn vocabulary_tokenizes_and_biases_within_budget() {
        let model = std::env::var("MURMUR_TEST_MODEL").unwrap_or_else(|_| "small.en".into());
        let ctx = open_context(&model).expect("open model");
        let vocab = crate::config::DEFAULT_DICTATION_VOCABULARY;
        let tokens = ctx.tokenize(vocab, 224).expect("tokenize vocab");
        assert!(!tokens.is_empty(), "dev vocab should produce prompt tokens");
        assert!(
            tokens.len() <= 224,
            "default vocab is {} tokens, over whisper's ~224 prompt budget — trim it",
            tokens.len()
        );
        // Run the same param setup transcribe uses, with the prompt tokens set,
        // over 0.5 s of silence — proves set_tokens + full() don't panic.
        let mut state = ctx.create_state().expect("state");
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        set_dictation_params(&mut params);
        params.set_tokens(&tokens);
        state
            .full(params, &vec![0f32; 16_000 / 2])
            .expect("full() with prompt tokens");
    }

    // Guards the FULL post-decode pipeline (`finalize_transcript`) end-to-end, so
    // a regression in any step — or in the order/wiring — is caught even though
    // each step also has its own unit test. These mirror real bugs seen in the
    // wild (leading dialogue dash, lone dash pasting a stray "-", placeholders).
    #[test]
    fn finalize_transcript_handles_the_reported_regressions() {
        let rules = parse_corrections(crate::config::DEFAULT_DICTATION_CORRECTIONS);

        // Leading dialogue dash is stripped (was: pasted "- No, this…").
        assert_eq!(
            finalize_transcript("- No, this is good", &rules),
            "No, this is good"
        );
        // Lone dash → empty, so the caller pastes nothing (was: pasted a "-").
        assert_eq!(finalize_transcript("-", &rules), "");
        // Non-speech placeholder → empty (nothing pasted).
        assert_eq!(finalize_transcript("[BLANK_AUDIO]", &rules), "");
        // Dash strip AND corrections both apply, in order.
        assert_eq!(
            finalize_transcript("- the cdci pipeline shipped", &rules),
            "the CI/CD pipeline shipped"
        );
        // Placeholder removed mid-text, speech kept, corrections applied.
        assert_eq!(
            finalize_transcript("I use Rust C [BLANK_AUDIO]", &rules),
            "I use rustc"
        );
        // Ordinary dictation is returned unchanged.
        assert_eq!(
            finalize_transcript("Ship the feature today", &rules),
            "Ship the feature today"
        );
        // With no rules configured, cleanup still runs.
        assert_eq!(finalize_transcript("- hello world", &[]), "hello world");
    }

    #[test]
    fn strips_whispers_leading_dialogue_dash() {
        // The two reported symptoms: leading "- " on real text, and a lone "-".
        assert_eq!(
            strip_leading_dialogue_dash("- No, this is bad"),
            "No, this is bad"
        );
        assert_eq!(
            strip_leading_dialogue_dash("-add a sound effect"),
            "add a sound effect"
        );
        assert_eq!(strip_leading_dialogue_dash("-"), ""); // lone dash → nothing pasted
        assert_eq!(
            strip_leading_dialogue_dash("  –  spaced en-dash"),
            "spaced en-dash"
        );
        // Only one leading marker is removed; interior dashes are untouched.
        assert_eq!(
            strip_leading_dialogue_dash("well-behaved text"),
            "well-behaved text"
        );
        assert_eq!(strip_leading_dialogue_dash("normal text"), "normal text");
        // A dash glued to a digit is a negative number, not the artifact.
        assert_eq!(strip_leading_dialogue_dash("-5 degrees"), "-5 degrees");
    }

    #[test]
    fn parses_correction_rules_skipping_comments_and_blanks() {
        let rules = parse_corrections(
            "# a comment\n\nrust see => rustc\n  cicd  =>  CI/CD  \n=> nope\nbad line\n",
        );
        assert_eq!(
            rules,
            vec![
                ("rust see".to_string(), "rustc".to_string()),
                ("cicd".to_string(), "CI/CD".to_string()),
            ]
        );
    }

    #[test]
    fn corrections_are_whole_word_and_case_insensitive() {
        let rules = parse_corrections("rust see => rustc\ncicd => CI/CD\nc => C-lang");
        // Case-insensitive, multi-word, and mid-sentence.
        assert_eq!(
            apply_corrections("I ran Rust See then cicd", &rules),
            "I ran rustc then CI/CD"
        );
        // Whole-word only: "c" must not rewrite inside "cat" or "cicd".
        assert_eq!(apply_corrections("the cat", &rules), "the cat");
        // But a standalone "c" is replaced.
        assert_eq!(
            apply_corrections("write c today", &rules),
            "write C-lang today"
        );
        // No rules → unchanged.
        assert_eq!(apply_corrections("untouched", &[]), "untouched");
    }

    #[test]
    fn default_corrections_parse_and_apply() {
        let rules = parse_corrections(crate::config::DEFAULT_DICTATION_CORRECTIONS);
        assert!(!rules.is_empty());
        // The real forms observed from dictation: "Rust C" / "Rust-C" / "cdci".
        assert_eq!(
            apply_corrections("the cdci pipeline", &rules),
            "the CI/CD pipeline"
        );
        assert_eq!(
            apply_corrections("I use Rust C daily", &rules),
            "I use rustc daily"
        );
        assert_eq!(
            apply_corrections("using Rust-C here", &rules),
            "using rustc here"
        );
    }

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Golden smoke test of the FULL shipped pipeline (default vocabulary +
    /// corrections + decode params) against committed real audio. Guards the
    /// core experience end to end: real speech must not decode to empty (the
    /// reported regression) and must contain its words, while silence must
    /// produce nothing (hallucination placeholders stripped, not pasted). Run
    /// with the model on disk (CI downloads it):
    ///   MURMUR_TEST_MODEL=base.en \
    ///     cargo test --no-default-features -- --ignored --nocapture dictation_pipeline_golden
    #[test]
    #[ignore = "needs a local Whisper model (MURMUR_TEST_MODEL, default small.en)"]
    fn dictation_pipeline_golden() {
        let model = std::env::var("MURMUR_TEST_MODEL").unwrap_or_else(|_| "small.en".into());
        let stt = WhisperStt::new(&model);
        // Exercise the exact config users ship with.
        stt.set_vocabulary(crate::config::DEFAULT_DICTATION_VOCABULARY);
        stt.set_corrections(crate::config::DEFAULT_DICTATION_CORRECTIONS);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        // Real speech → non-empty, and its distinctive words survive the pipeline.
        let speech = std::fs::read(fixture("speech.wav")).expect("read speech.wav");
        let out = rt
            .block_on(stt.transcribe(&speech))
            .expect("transcribe speech");
        assert!(
            !out.trim().is_empty(),
            "real speech decoded to EMPTY — the core dictation regression"
        );
        // A distinctive word that reliably survives (exact wording varies by
        // model, so assert real content without being brittle about it).
        assert!(
            out.to_lowercase().contains("dictation"),
            "expected content missing from transcript: {out:?}"
        );

        // Silence → nothing pasted (whisper's placeholders must be stripped).
        let silence = std::fs::read(fixture("silence.wav")).expect("read silence.wav");
        let out = rt
            .block_on(stt.transcribe(&silence))
            .expect("transcribe silence");
        assert!(
            out.trim().is_empty(),
            "silence should produce no text, got {out:?}"
        );
    }

    /// End-to-end local transcription against a real WAV fixture + model.
    /// Ignored by default (needs the ~0.5 GB model on disk). Run manually:
    ///   MURMUR_TEST_WAV=/path/to/jfk.wav \
    ///     cargo test --no-default-features -- --ignored --nocapture transcribes_fixture
    #[test]
    #[ignore = "needs a local Whisper model + a 16 kHz WAV fixture (MURMUR_TEST_WAV)"]
    fn transcribes_fixture() {
        let wav_path = std::env::var("MURMUR_TEST_WAV").expect("set MURMUR_TEST_WAV");
        let model = std::env::var("MURMUR_TEST_MODEL").unwrap_or_else(|_| "small.en".into());
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
