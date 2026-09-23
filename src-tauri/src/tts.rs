// Phase 3: read-aloud.
//
// Two on-device backends behind a shared `Speaker` trait, chosen at startup by
// `tts_provider` in config. The mac backend uses AVSpeechSynthesizer (free,
// offline, but the default voice is rough). The Kokoro backend runs a local
// neural model (ONNX via `ort`) and plays synthesized WAV chunks through an
// AVQueuePlayer with `audioTimePitchAlgorithm = .spectral` — pitch-preserving
// speed changes without a resampling chipmunk effect.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::kokoro::{Cancellation, KokoroTts};
use anyhow::Result;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;
use tokio::sync::Mutex as AsyncMutex;

// Link AVFoundation — we only message its classes, no extern fns.
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// `Speaker` is the seam: any backend that can turn text into audio output.
/// All methods are non-blocking; `speak` and `stop` return immediately.
pub trait Speaker: Send + Sync {
    fn speak(&self, text: &str);
    fn stop(&self);
    fn is_speaking(&self) -> bool;

    /// Speak a short preview sample (used when picking a voice in Settings).
    /// Backends may render + cache it for instant replay; the default just
    /// speaks it live, which is fine for the instant native voice.
    fn preview(&self, text: &str) {
        self.speak(text);
    }

    /// Preload a heavy backend (model load + any GPU/ANE graph compile) in the
    /// background so the first `speak` doesn't stall. Default no-op — native
    /// TTS is instant and needs no warming.
    fn warm(&self) {}

    // Speed (1.0 = normal, 2.0 = double). Backends without a real speed
    // control no-op these.
    fn cycle_speed(&self) -> f32 {
        1.0
    }
    fn set_speed(&self, _speed: f32) {}
    fn current_speed(&self) -> f32 {
        1.0
    }

    // Voice selection. `set_voice` takes a backend-specific identifier.
    fn set_voice(&self, _voice_id: &str) {}
    fn current_voice(&self) -> Option<String> {
        None
    }

    /// Progress of the current read-aloud in `[0.0, 1.0]`, or `None` if not
    /// reading or the backend can't report it. Drives the overlay progress fill.
    fn progress(&self) -> Option<f32> {
        None
    }
}

/// Speeds the tray menu exposes. AVPlayer's spectral pitch algorithm sounds
/// natural up to 2.0×; we cap there.
pub const SPEEDS: &[f32] = &[1.0, 1.5, 2.0];

// ──────────────────────────────────────────────────────────────────────────
// AVSpeechSynthesizer backend.
// ──────────────────────────────────────────────────────────────────────────

/// SAFETY: the held pointer is only mutated under the mutex, and methods are
/// only message-sent from the main thread (see module comment).
struct SynthHolder(NonNull<AnyObject>);
// SAFETY: the pointer is only accessed under `MacSpeaker`'s mutex and messaged
// from the main thread, so moving the holder between threads can't race.
unsafe impl Send for SynthHolder {}

impl Drop for SynthHolder {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live AVSpeechSynthesizer from `new`; `release`
        // is a valid selector and is sent exactly once (on drop).
        unsafe {
            let _: () = msg_send![self.0.as_ptr(), release];
        }
    }
}

pub struct MacSpeaker {
    synth: Mutex<Option<SynthHolder>>,
}

impl MacSpeaker {
    pub fn new() -> Self {
        Self {
            synth: Mutex::new(None),
        }
    }
}

impl Default for MacSpeaker {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for MacSpeaker {
    fn speak(&self, text: &str) {
        let mut g = self.synth.lock().expect("speaker mutex");
        if let Some(prev) = g.take() {
            // SAFETY: `prev.0` is a live synth from `new`; `stopSpeakingAtBoundary:`
            // takes an NSInteger, matching the `i64` argument.
            unsafe {
                let _: () = msg_send![prev.0.as_ptr(), stopSpeakingAtBoundary: 0i64];
            }
        }
        // SAFETY: every selector (`new`, `alloc`, `initWithString:`,
        // `speakUtterance:`, `release`) exists on the messaged class/instance with
        // the argument types used; `synth` is checked non-null before it's stored.
        unsafe {
            let synth: *mut AnyObject = msg_send![class!(AVSpeechSynthesizer), new];
            let nss = NSString::from_str(text);
            let utt_alloc: *mut AnyObject = msg_send![class!(AVSpeechUtterance), alloc];
            let utt: *mut AnyObject = msg_send![utt_alloc, initWithString: &*nss];
            let _: () = msg_send![synth, speakUtterance: utt];
            let _: () = msg_send![utt, release];

            let ptr = NonNull::new(synth).expect("AVSpeechSynthesizer new returned null");
            *g = Some(SynthHolder(ptr));
        }
    }

    fn stop(&self) {
        let mut g = self.synth.lock().expect("speaker mutex");
        if let Some(prev) = g.take() {
            // SAFETY: `prev.0` is a live synth from `new`; the argument type
            // matches `stopSpeakingAtBoundary:`.
            unsafe {
                let _: () = msg_send![prev.0.as_ptr(), stopSpeakingAtBoundary: 0i64];
            }
        }
    }

    fn is_speaking(&self) -> bool {
        let g = self.synth.lock().expect("speaker mutex");
        match g.as_ref() {
            None => false,
            // SAFETY: `h.0` is a live synth held under the mutex; `isSpeaking`
            // returns a BOOL.
            Some(h) => unsafe {
                let speaking: bool = msg_send![h.0.as_ptr(), isSpeaking];
                speaking
            },
        }
    }
}

/// Spectral is highest quality for speech — preserves formants and pitch.
/// Used by the Kokoro backend's AVQueuePlayer for pitch-preserving speed.
const PITCH_ALG: &str = "AVAudioTimePitchAlgorithmSpectral";

/// Encode mono f32 [-1, 1] PCM into a 16-bit WAV byte buffer at `rate` Hz.
fn pcm_f32_to_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate (mono, 16-bit)
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        w.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    w
}

// ──────────────────────────────────────────────────────────────────────────
// Kokoro backend — local neural TTS (kokoro-en: ONNX via `ort`, CoreML-
// accelerated; cmudict G2P, no espeak — see THIRD-PARTY-NOTICES.md). Fully
// on-device; no API key.
// ──────────────────────────────────────────────────────────────────────────

/// Curated subset of Kokoro's voices we ship. `(id, friendly name)`; `af_*`/`am_*`
/// are US female/male, `bf_*`/`bm_*` British.
pub const KOKORO_VOICES: &[(&str, &str)] = &[
    ("af_heart", "Heart (US female)"),
    ("af_bella", "Bella (US female)"),
    ("af_nicole", "Nicole (US female)"),
    ("am_michael", "Michael (US male)"),
    ("am_puck", "Puck (US male)"),
    ("am_fenrir", "Fenrir (US male)"),
    ("bf_emma", "Emma (UK female)"),
    ("bm_george", "George (UK male)"),
];

const KOKORO_DEFAULT_VOICE: &str = "am_puck";
const KOKORO_SAMPLE_RATE: u32 = 24_000;

/// Voice choices to show in the tray + settings pickers for a given TTS
/// provider, so the picker matches the backend that will actually speak.
/// Native (AVSpeechSynthesizer) has no in-app voice selection, so it's empty.
pub fn voices_for(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        "kokoro" => KOKORO_VOICES,
        _ => &[],
    }
}

/// Path to the Kokoro ONNX model file.
pub fn kokoro_model_path() -> Result<PathBuf> {
    Ok(crate::stt::models_dir()?.join("kokoro-v1.0.onnx"))
}

/// Directory holding Kokoro voice `.bin` packs.
pub fn kokoro_voices_dir() -> Result<PathBuf> {
    Ok(crate::stt::models_dir()?.join("kokoro-voices"))
}

/// True once the model and at least one voice pack are on disk.
pub fn kokoro_assets_present() -> bool {
    let model = kokoro_model_path().map(|p| p.exists()).unwrap_or(false);
    let has_voice = kokoro_voices_dir()
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|mut e| {
            e.any(|f| {
                f.map(|f| f.path().extension().is_some_and(|x| x == "bin"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    model && has_voice
}

/// Download the Kokoro model + curated voice packs if missing (onnx-community
/// Kokoro-82M v1.0 on Hugging Face). Safe to call repeatedly.
pub async fn ensure_kokoro_assets(on_progress: impl Fn(u64, u64)) -> Result<()> {
    const BASE: &str = "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main";
    let model = kokoro_model_path()?;
    if !model.exists() {
        if let Some(p) = model.parent() {
            std::fs::create_dir_all(p).ok();
        }
        log::info!("tts/kokoro: downloading model (~310 MB, one-time)…");
        // The model.onnx is ~310 MB — the whole download; report its progress.
        // The voice packs below are a few MB total, so they need no bar.
        crate::download::to_file(&format!("{BASE}/onnx/model.onnx"), &model, &on_progress).await?;
    }
    let dir = kokoro_voices_dir()?;
    std::fs::create_dir_all(&dir).ok();
    for (id, _) in KOKORO_VOICES {
        let dst = dir.join(format!("{id}.bin"));
        if !dst.exists() {
            log::info!("tts/kokoro: downloading voice '{id}'…");
            crate::download::to_file(&format!("{BASE}/voices/{id}.bin"), &dst, &|_, _| {}).await?;
        }
    }
    log::info!("tts/kokoro: assets ready");
    // Ensure listeners see a completed bar even if the model was already present.
    let done = std::fs::metadata(&model).map(|m| m.len()).unwrap_or(0);
    on_progress(done, done);
    Ok(())
}

/// One `AVQueuePlayer` for a whole read-aloud. Chunk WAVs are appended as items
/// as they finish synthesizing, and the OS plays them back-to-back. Every temp
/// WAV is deleted when the holder drops (stop, or the next read replacing it).
struct KokoroQueue {
    player: NonNull<AnyObject>,
    temps: Vec<PathBuf>,
    buffering: bool,
}
// SAFETY: the AVQueuePlayer pointer is only messaged under the player mutex, and
// AVPlayer playback methods are thread-safe (the main-thread rule is UI only).
unsafe impl Send for KokoroQueue {}

impl Drop for KokoroQueue {
    fn drop(&mut self) {
        // SAFETY: `self.player` is a retained AVQueuePlayer; `pause`/`release` are
        // valid selectors and `release` balances the `new` retain exactly once.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), pause];
            let _: () = msg_send![self.player.as_ptr(), release];
        }
        for t in &self.temps {
            let _ = std::fs::remove_file(t);
        }
    }
}

/// Create an empty, retained `AVQueuePlayer` (`new` = alloc/init, +1 owned).
fn new_queue_player() -> Option<NonNull<AnyObject>> {
    // SAFETY: `+new` returns a retained AVQueuePlayer or nil; we null-check it.
    unsafe {
        let player: *mut AnyObject = msg_send![class!(AVQueuePlayer), new];
        NonNull::new(player)
    }
}

/// Append the WAV at `temp` to the queue as a pitch-preserving item. Returns
/// false if the item can't be created/inserted.
///
/// SAFETY: `player` must be a live AVQueuePlayer; the selectors/argument types
/// match their ObjC signatures and the item is null-checked before use.
unsafe fn enqueue_wav(player: *mut AnyObject, temp: &std::path::Path) -> bool {
    let path_str = NSString::from_str(temp.to_str().unwrap_or_default());
    let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*path_str];
    let item: *mut AnyObject = msg_send![class!(AVPlayerItem), playerItemWithURL: url];
    if item.is_null() {
        return false;
    }
    let algo = NSString::from_str(PITCH_ALG);
    let _: () = msg_send![item, setAudioTimePitchAlgorithm: &*algo];
    let after: *mut AnyObject = std::ptr::null_mut();
    let can: bool = msg_send![player, canInsertItem: item, afterItem: after];
    if !can {
        return false;
    }
    let _: () = msg_send![player, insertItem: item, afterItem: after];
    true
}

pub struct KokoroSpeaker {
    model_path: PathBuf,
    voices_path: PathBuf,
    /// Loaded lazily on first `speak` and cached (model load is ~1s).
    tts: Arc<AsyncMutex<Option<Arc<KokoroTts>>>>,
    voice: Mutex<String>,
    speed: Arc<Mutex<f32>>,
    /// The queue player for the current read (all chunks play through this one).
    player: Arc<Mutex<Option<KokoroQueue>>>,
    active: Arc<AtomicBool>,
    /// Read-aloud progress × 1000 (so it fits an integer atomic).
    progress: Arc<AtomicU64>,
    /// Bumped per `speak`; each read tags its work with its own value so a
    /// finishing read only cleans up its own player, never a newer read's.
    generation: Arc<AtomicU64>,
    /// Player for the current voice preview, held so a new preview (or stop())
    /// replaces + releases the previous one. Separate from `player` (read-aloud).
    preview_player: Arc<Mutex<Option<KokoroQueue>>>,
    /// Set once the model's graph is compiled (first synth done), so repeat
    /// `warm()` calls — e.g. startup warm *and* the download-complete warm — skip
    /// the redundant throwaway synth.
    warmed: Arc<AtomicBool>,
}

impl KokoroSpeaker {
    pub fn new(model_path: PathBuf, voices_path: PathBuf) -> Self {
        Self {
            model_path,
            voices_path,
            tts: Arc::new(AsyncMutex::new(None)),
            voice: Mutex::new(KOKORO_DEFAULT_VOICE.into()),
            speed: Arc::new(Mutex::new(1.0)),
            player: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
            preview_player: Arc::new(Mutex::new(None)),
            warmed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Render + cache the preview clip for every shipped voice in the background,
    /// so switching voices in Settings is instant. Skips work when everything is
    /// already cached (and then never loads the model).
    pub fn pregenerate_previews(&self) {
        let all_cached = KOKORO_VOICES.iter().all(|(id, friendly)| {
            preview_cache_path(id, &preview_text(friendly))
                .map(|p| p.exists())
                .unwrap_or(false)
        });
        if all_cached {
            return;
        }
        let model_path = self.model_path.clone();
        let voices_path = self.voices_path.clone();
        let tts_cell = self.tts.clone();
        tauri::async_runtime::spawn(async move {
            let Some(tts) = load_kokoro_tts(&tts_cell, &model_path, &voices_path).await else {
                return;
            };
            for (id, friendly) in KOKORO_VOICES {
                let text = preview_text(friendly);
                let Some(path) = preview_cache_path(id, &text) else {
                    continue;
                };
                if path.exists() {
                    continue;
                }
                if let Some(wav) = synth_chunk_wav(&tts, &text, id, 0).await {
                    write_cache_file(&path, &wav);
                }
            }
            log::info!("tts/kokoro: voice previews cached");
        });
    }
}

/// Bundled dev-term pronunciation lexicon (`word<TAB>ipa`), version-controlled
/// in the repo. Kokoro's G2P mangles code jargon; these override it word-for-word.
const DEV_TERMS_TAB: &str = include_str!("dev_terms.tab");

/// Write the bundled dev-term lexicon to `<app-support>/murmur` and point
/// kokoro-en's `KOKORO_G2P_LEXICON` at it, so those pronunciations take priority
/// over the degraded built-in G2P. Call once at startup, **before the first
/// synth** — the crate reads the env var lazily on first lookup, and Kokoro's
/// preview pre-generation / warm can trigger that. Best-effort: logs and returns
/// on failure rather than blocking startup.
pub fn install_g2p_lexicon() {
    let Some(dir) = crate::stt::models_dir()
        .ok()
        .and_then(|d| d.parent().map(|p| p.to_path_buf()))
    else {
        return;
    };
    let path = dir.join("dev-terms.tab");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, DEV_TERMS_TAB))
    {
        log::warn!(
            "tts: could not install g2p lexicon at {}: {e}",
            path.display()
        );
        return;
    }
    // Safe on edition 2021; set at startup before any synth reads it.
    std::env::set_var("KOKORO_G2P_LEXICON", &path);
    log::info!("tts: g2p lexicon → {}", path.display());
    // Force the crate's lexicon to load now (it reads the env lazily on first
    // lookup) so a later synth on another thread can't init it before we're set.
    match kokoro_en::g2p("nginx", false) {
        Ok(ph) => log::debug!("tts: g2p lexicon self-check nginx = {ph:?}"),
        Err(e) => log::warn!("tts: g2p self-check failed: {e}"),
    }
}

/// Select the measured CoreML compute policy before the session is created.
/// This limits eligible devices; it does not prove where each operator runs.
/// Preserve explicit environment overrides for reproducible comparisons with
/// `scripts/profile-tts.py`. Inference timing alone cannot identify fallback.
pub fn pin_coreml_compute_units() {
    if std::env::var_os("KOKORO_COREML_COMPUTE_UNITS").is_some() {
        log::info!("tts: KOKORO_COREML_COMPUTE_UNITS set by env — leaving as-is");
        return;
    }
    // Safe on edition 2021; set at startup before any synth reads it.
    std::env::set_var("KOKORO_COREML_COMPUTE_UNITS", "cpu_and_gpu");
    log::info!("tts: CoreML compute policy → cpu_and_gpu");
}

/// Normalize text before synthesis so the neural voice pronounces code-style
/// words correctly. Kokoro's bundled G2P (cmudict/Misaki — no espeak, which is
/// GPL; see AGENTS.md) mispronounces run-together identifiers, so we split them
/// into spoken words: `camelCase`/`PascalCase` → "camel Case", `snake_case` and
/// `kebab-case` → spaces. Acronym runs stay together ("HTTPServer" → "HTTP
/// Server", "NASA" stays "NASA").
fn normalize_for_tts(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for i in 0..chars.len() {
        let c = chars[i];
        // `_`/`-` joining two word chars → space (leave standalone dashes and
        // things like "-5" alone).
        if (c == '_' || c == '-')
            && i > 0
            && chars[i - 1].is_alphanumeric()
            && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric())
        {
            out.push(' ');
            continue;
        }
        if i > 0 {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            // lower/digit → Upper: "camelCase", "mp3Player".
            let lower_to_upper = (prev.is_lowercase() || prev.is_ascii_digit()) && c.is_uppercase();
            // end of an ACRONYM run before a new word: "HTTPServer" → "HTTP Server".
            let acronym_to_word =
                prev.is_uppercase() && c.is_uppercase() && next.is_some_and(|n| n.is_lowercase());
            if lower_to_upper || acronym_to_word {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out
}

/// Inter-chunk gaps appended after trimming Kokoro's (uneven, ~630ms total) edge
/// padding, so a read keeps natural spacing without the dead air we measured.
const SENTENCE_GAP_MS: u32 = 140; // after a full sentence — a natural breath
const SOFT_GAP_MS: u32 = 60; // after a soft comma/clause break — just flows

// A sentence break must read as a longer pause than a soft (comma) break, or the
// prosody inverts. Enforced at compile time.
const _: () = assert!(SENTENCE_GAP_MS > SOFT_GAP_MS);

/// Split at natural pauses where possible. Bound the opening phrase to about
/// 64 characters even without punctuation, then ramp to 100 and 220 characters
/// for subsequent chunks. Word boundaries preserve text; soft gaps avoid a
/// sentence-length pause when a latency-driven split lands mid-clause.
fn split_for_tts(text: &str) -> Vec<(String, bool)> {
    let text = text.trim();
    const MIN: usize = 16; // keep tiny fragments merged into the next clause
    const FIRST_MAX: usize = 64; // short opening phrase, even without a comma
    const CAP: usize = 220; // normal chunks prefer sentences, then commas/words
    let mut chunks: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut last_comma = 0usize; // byte index just past the most recent comma
    let mut last_space = 0usize; // byte index just past the most recent space
    for ch in text.chars() {
        cur.push(ch);
        match ch {
            ',' => last_comma = cur.len(),
            ' ' => last_space = cur.len(),
            _ => {}
        }
        let first = chunks.is_empty();
        let cap = match chunks.len() {
            0 => FIRST_MAX,
            1 => 100,
            _ => CAP,
        };
        let sentence_end = matches!(ch, '.' | '!' | '?' | '\n' | ';' | ':');
        if sentence_end && cur.trim_end().len() >= MIN {
            push_chunk(&mut chunks, &cur, true);
            cur.clear();
            last_comma = 0;
            last_space = 0;
        } else if first
            && ch == ','
            && cur.trim_end().len() >= MIN
            && cur.chars().count() <= FIRST_MAX
        {
            // Fast first word: end the opening chunk at the first comma (a real
            // pause). Soft break — flows into the rest of the sentence.
            push_chunk(&mut chunks, &cur, false);
            cur.clear();
            last_comma = 0;
            last_space = 0;
        } else if cur.chars().count() >= cap {
            // Too long with no sentence end: break at the last comma (natural
            // pause), else the last word boundary — never mid-word.
            let at = if last_comma > MIN {
                last_comma
            } else {
                last_space
            };
            if at > MIN && at < cur.len() {
                let carry = cur.split_off(at);
                push_chunk(&mut chunks, &cur, false);
                cur = carry;
            } else if ch.is_whitespace() {
                push_chunk(&mut chunks, &cur, false);
                cur.clear();
            } else {
                // A single long word/URL must never be split in two.
                continue;
            }
            last_comma = 0;
            last_space = 0;
        }
    }
    push_chunk(&mut chunks, &cur, true);
    if chunks.is_empty() {
        chunks.push((text.to_string(), true));
    }
    chunks
}

/// Cover the estimated time to synthesize the next chunk, with 25% headroom.
/// Cap the startup/rebuffer budget at three wall-clock seconds: slower-than-
/// playback synthesis cannot be made stall-free by a small initial buffer.
fn buffer_target(speed: f32, seconds_per_char: f32, next_chars: usize) -> f32 {
    if next_chars == 0 {
        return 0.0;
    }
    speed * (seconds_per_char * next_chars as f32 * 1.25 + 0.08).clamp(0.15, 3.0)
}

/// Push `s` (trimmed) onto `chunks` with its break kind unless it's empty.
/// `hard` = ended at a sentence terminator (a natural pause follows); `false` =
/// a soft comma/clause break that should flow into the next chunk.
fn push_chunk(chunks: &mut Vec<(String, bool)>, s: &str, hard: bool) {
    let t = s.trim();
    if !t.is_empty() {
        chunks.push((t.to_string(), hard));
    }
}

/// Synthesize one chunk to a 16-bit WAV buffer; `None` on synth error.
async fn synth_chunk_wav(
    tts: &KokoroTts,
    text: &str,
    voice: &str,
    tail_gap_ms: u32,
) -> Option<Vec<u8>> {
    synth_chunk_wav_cancellable(tts, text, voice, tail_gap_ms, None).await
}

async fn synth_chunk_wav_cancellable(
    tts: &KokoroTts,
    text: &str,
    voice: &str,
    tail_gap_ms: u32,
    cancellation: Option<Cancellation>,
) -> Option<Vec<u8>> {
    let t0 = std::time::Instant::now();
    let result = tts
        .synth_cancellable(text, voice, cancellation.clone())
        .await;
    if cancellation
        .as_ref()
        .is_some_and(Cancellation::is_cancelled)
    {
        return None;
    }
    match result {
        Ok((samples, inference)) => {
            let synth_secs = t0.elapsed().as_secs_f32();
            let inference_ms = inference.as_secs_f32() * 1000.0;
            let other_ms = (synth_secs * 1000.0 - inference_ms).max(0.0);
            // Kokoro pads every chunk with edge silence (~220ms lead, ~430ms tail
            // measured). Trim it so chunks play gapless — the leading trim comes
            // straight off time-to-first-word — then append a short, controlled
            // gap so chunks don't slam together (0 = seamless, for the last chunk).
            let voiced = trim_silence(&samples);
            let gap = tail_gap_ms as usize * KOKORO_SAMPLE_RATE as usize / 1000;
            let mut out = Vec::with_capacity(voiced.len() + gap);
            out.extend_from_slice(voiced);
            out.resize(out.len() + gap, 0.0);
            // The worker reports ONNX run time separately from G2P, voice
            // lookup, tensor preparation, and waiting in its request queue.
            // Slow throughput is not evidence of a particular device/fallback.
            let audio_secs = out.len() as f32 / KOKORO_SAMPLE_RATE as f32;
            log::debug!(
                "tts/kokoro: synth {} chars → {:.2}s audio in {:.0}ms ({:.1}x realtime) [inference {:.0}ms, other/queue {:.0}ms, trimmed {:.0}ms pad]",
                text.chars().count(),
                audio_secs,
                synth_secs * 1000.0,
                audio_secs / synth_secs.max(1e-3),
                inference_ms,
                other_ms,
                samples.len().saturating_sub(voiced.len()) as f32 / KOKORO_SAMPLE_RATE as f32 * 1000.0,
            );
            if synth_secs >= 2.0 && audio_secs / synth_secs < 2.0 {
                log::info!(
                    "tts/kokoro: slow synth: {} chars, {:.0}ms total, {:.0}ms inference, {:.0}ms other/queue, {:.1}x realtime",
                    text.chars().count(), synth_secs * 1000.0, inference_ms,
                    other_ms, audio_secs / synth_secs,
                );
            }
            Some(pcm_f32_to_wav(&out, KOKORO_SAMPLE_RATE))
        }
        Err(e) => {
            log::warn!("tts/kokoro: synth chunk failed: {e}");
            None
        }
    }
}

/// Trim the near-silent padding Kokoro adds to each synthesized chunk, returning
/// the voiced middle. Keeps a ~10ms guard each side so a soft onset/tail is never
/// clipped. Returns the input unchanged if the whole clip is below threshold.
fn trim_silence(samples: &[f32]) -> &[f32] {
    const THRESH: f32 = 0.005;
    const GUARD: usize = KOKORO_SAMPLE_RATE as usize / 100; // 10ms each side
    match (
        samples.iter().position(|s| s.abs() >= THRESH),
        samples.iter().rposition(|s| s.abs() >= THRESH),
    ) {
        (Some(a), Some(b)) => {
            let start = a.saturating_sub(GUARD);
            let end = (b + 1 + GUARD).min(samples.len());
            &samples[start..end]
        }
        _ => samples,
    }
}

// ── Voice-preview cache ─────────────────────────────────────────────────────
// Kokoro synth is ~1s, so picking a voice in Settings felt laggy. We render each
// voice's preview clip once, cache the WAV next to the model, and play the file
// directly (instant). All previews are also pre-generated in the background at
// startup, so even the first switch is instant.

/// The spoken preview line for a voice's friendly name ("Puck (US male)" →
/// "Hey, my name is Puck!"). Kept in one place so the on-demand preview and the
/// pre-generated cache produce the same text (hence the same cache file).
pub fn preview_text(friendly_name: &str) -> String {
    let name = friendly_name
        .split('(')
        .next()
        .unwrap_or(friendly_name)
        .trim();
    if name.is_empty() {
        "Hey! This is how I sound.".to_string()
    } else {
        format!("Hey, my name is {name}!")
    }
}

/// Cache path for a voice's preview WAV, keyed by voice id + a hash of the text
/// (so changing the phrase auto-invalidates old clips). `None` if the models dir
/// can't be resolved.
fn preview_cache_path(voice: &str, text: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let dir = kokoro_model_path().ok()?.parent()?.join("previews");
    Some(dir.join(format!("{voice}-{:016x}.wav", h.finish())))
}

/// Write `bytes` to `path` atomically (`.part` + rename). Returns success.
fn write_cache_file(path: &std::path::Path, bytes: &[u8]) -> bool {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let part = path.with_extension("part");
    std::fs::write(&part, bytes).is_ok() && std::fs::rename(&part, path).is_ok()
}

/// Play a cached preview WAV at `speed` through an AVQueuePlayer — same
/// pitch-preserving spectral speed as read-aloud, so the preview matches the
/// current speed setting. Held in `slot` so a new preview (or stop()) replaces +
/// releases the previous one. The cached file is never deleted (`temps` stays
/// empty), unlike read-aloud's transient chunk files.
fn play_preview_file(
    slot: &Arc<Mutex<Option<KokoroQueue>>>,
    path: &std::path::Path,
    speed: f32,
    cancellation: &Cancellation,
) {
    let mut guard = slot.lock().expect("preview mutex");
    if cancellation.is_cancelled() {
        return;
    }
    let Some(qp) = new_queue_player() else {
        return;
    };
    let queue = KokoroQueue {
        player: qp,
        temps: Vec::new(),
        buffering: false,
    };
    // SAFETY: `qp` is a live AVQueuePlayer from new_queue_player(); `enqueue_wav`
    // and `playImmediatelyAtRate:` are valid messages with the argument types
    // used, and playback methods are thread-safe off the main thread (module
    // comment). On enqueue failure, dropping `queue` pauses + releases the player.
    unsafe {
        if !enqueue_wav(qp.as_ptr(), path) {
            return;
        }
        // Plays now at `speed`; the item's spectral pitch algorithm (set in
        // enqueue_wav) preserves pitch.
        let _: () = msg_send![qp.as_ptr(), playImmediatelyAtRate: speed];
    }
    // Replace any previous preview (its KokoroQueue drops → pause + release).
    *guard = Some(queue);
}

/// Load (and cache) the Kokoro model, shared by `speak`, `preview`, and
/// pre-generation. `None` if the model can't be loaded.
async fn load_kokoro_tts(
    cell: &Arc<AsyncMutex<Option<Arc<KokoroTts>>>>,
    model_path: &std::path::Path,
    voices_path: &std::path::Path,
) -> Option<Arc<KokoroTts>> {
    let mut guard = cell.lock().await;
    if guard.is_none() {
        match KokoroTts::new(model_path, voices_path).await {
            Ok(t) => *guard = Some(Arc::new(t)),
            Err(e) => {
                log::warn!("tts/kokoro: load model failed: {e}");
                return None;
            }
        }
    }
    guard.clone()
}

impl Speaker for KokoroSpeaker {
    fn preview(&self, text: &str) {
        let voice = self.voice.lock().expect("voice mutex").clone();
        let speed = *self.speed.lock().expect("speed mutex");
        let Some(path) = preview_cache_path(&voice, text) else {
            self.speak(text);
            return;
        };
        // Don't overlap an in-progress read-aloud (also clears a prior preview).
        self.stop();
        let cancellation = Cancellation::new(
            self.generation.clone(),
            self.generation.load(Ordering::Acquire),
        );
        if path.exists() {
            play_preview_file(&self.preview_player, &path, speed, &cancellation); // cached → instant
            return;
        }
        // Cache miss: synth once (~1s), cache, then play — next time is instant.
        let text = text.to_string();
        let model_path = self.model_path.clone();
        let voices_path = self.voices_path.clone();
        let tts_cell = self.tts.clone();
        let slot = self.preview_player.clone();
        tauri::async_runtime::spawn(async move {
            let Some(tts) = load_kokoro_tts(&tts_cell, &model_path, &voices_path).await else {
                return;
            };
            if let Some(wav) =
                synth_chunk_wav_cancellable(&tts, &text, &voice, 0, Some(cancellation.clone()))
                    .await
            {
                if write_cache_file(&path, &wav) {
                    play_preview_file(&slot, &path, speed, &cancellation);
                }
            }
        });
    }

    fn warm(&self) {
        // Load the ONNX session and run one throwaway synth in the background:
        // this pays the ~1s model load *and* the first-run CoreML/ANE graph
        // compile up front, so the first real read-aloud starts promptly rather
        // than after that one-time cost. Idempotent: `warmed` guards against the
        // redundant synth when warm is called more than once (startup warm plus
        // the download-complete warm).
        if self.warmed.load(Ordering::Acquire) {
            return;
        }
        let model_path = self.model_path.clone();
        let voices_path = self.voices_path.clone();
        let tts_cell = self.tts.clone();
        let voice = self.voice.lock().expect("voice mutex").clone();
        let warmed = self.warmed.clone();
        tauri::async_runtime::spawn(async move {
            // Recheck under the async task in case two warms raced the sync guard.
            if warmed.swap(true, Ordering::AcqRel) {
                return;
            }
            let Some(tts) = load_kokoro_tts(&tts_cell, &model_path, &voices_path).await else {
                warmed.store(false, Ordering::Release); // load failed — allow a retry
                return;
            };
            // A short synth forces the graph compile; the audio is discarded.
            if tts.synth("Ready.", &voice).await.is_ok() {
                log::info!("tts/kokoro: model warmed");
            } else {
                warmed.store(false, Ordering::Release); // synth failed — allow a retry
            }
        });
    }

    fn speak(&self, text: &str) {
        let text = text.to_string();
        let voice = Arc::new(self.voice.lock().expect("voice mutex").clone());
        let live_speed = self.speed.clone();
        // This read's id; a newer speak bumps `generation` past it so we only
        // clean up our own player.
        let n = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let model_path = self.model_path.clone();
        let voices_path = self.voices_path.clone();
        let tts_cell = self.tts.clone();
        let player_slot = self.player.clone();
        let active = self.active.clone();
        let progress = self.progress.clone();
        let generation = self.generation.clone();
        // Mark active immediately so is_speaking() is true during synthesis.
        active.store(true, Ordering::Release);
        progress.store(0, Ordering::Release);

        tauri::async_runtime::spawn(async move {
            // Time-to-first-word: from here (speak dispatched) until the player
            // actually starts. This is model-load (warm) + synth of the lead
            // chunks + prebuffer — the latency the user feels after pressing.
            let speak_t0 = std::time::Instant::now();
            // Load + cache the model on first use (shared with preview/warm).
            let Some(tts) = load_kokoro_tts(&tts_cell, &model_path, &voices_path).await else {
                if generation.load(Ordering::Acquire) == n {
                    active.store(false, Ordering::Release);
                }
                return;
            };
            // Time to get the session: ~0 when warm; seconds means it reloaded or
            // waited on the shared session lock (another synth/preview in flight).
            if generation.load(Ordering::Acquire) != n {
                return;
            }
            let model_ms = speak_t0.elapsed().as_secs_f32() * 1000.0;

            let chunks = split_for_tts(&normalize_for_tts(&text));
            log::info!(
                "tts/kokoro: reading {} chars in {} chunk(s) [voice {}]",
                text.len(),
                chunks.len(),
                voice.as_str()
            );

            // One AVQueuePlayer for the whole read: chunks are appended as items
            // as they finish synthesizing and play back-to-back.
            let qp = match new_queue_player() {
                Some(p) => p,
                None => {
                    log::warn!("tts/kokoro: AVQueuePlayer init failed");
                    active.store(false, Ordering::Release);
                    return;
                }
            };
            {
                let mut slot = player_slot.lock().expect("player mutex");
                let queue = KokoroQueue {
                    player: qp,
                    temps: Vec::new(),
                    buffering: true,
                };
                if generation.load(Ordering::Acquire) != n {
                    return;
                }
                *slot = Some(queue);
            }

            let chunk_chars: Arc<Vec<f32>> = Arc::new(
                chunks
                    .iter()
                    .map(|(c, _)| c.chars().count() as f32)
                    .collect(),
            );
            let total_chars = chunk_chars.iter().sum::<f32>().max(1.0);
            let durations: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
            let all_enqueued = Arc::new(AtomicBool::new(false));

            // Progress + completion task: follow the queue's currentItem, advance
            // char-weighted progress smoothed by elapsed playback time, and finish
            // once every chunk is enqueued and the last item has played out.
            let prog = {
                let (active, progress, player_slot) =
                    (active.clone(), progress.clone(), player_slot.clone());
                let (durations, all_enqueued, chunk_chars) =
                    (durations.clone(), all_enqueued.clone(), chunk_chars.clone());
                let generation = generation.clone();
                tauri::async_runtime::spawn(async move {
                    let mut last_item: usize = 0; // currentItem ptr as usize (0 = nil)
                    let mut idx: usize = 0; // currently-playing chunk index
                    let mut chars_before = 0f32;
                    let mut last_poll = std::time::Instant::now();
                    let mut item_elapsed = 0.0f32;
                    let mut started = false;
                    let mut drained_polls = 0u32;
                    loop {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        // Stop if this read ended (`active` false) or a newer read
                        // superseded us (generation bumped) — a re-trigger resets
                        // `active` to true, so `active` alone can't tell us apart.
                        if !active.load(Ordering::Acquire)
                            || generation.load(Ordering::Acquire) != n
                        {
                            break;
                        }
                        let (cur, playback_rate) = {
                            let g = player_slot.lock().expect("player mutex");
                            match g.as_ref() {
                                Some(h) => {
                                    // SAFETY: live AVQueuePlayer held under the mutex.
                                    unsafe {
                                        let c: *mut AnyObject =
                                            msg_send![h.player.as_ptr(), currentItem];
                                        let rate: f32 = msg_send![h.player.as_ptr(), rate];
                                        (c as usize, if h.buffering { 0.0 } else { rate })
                                    }
                                }
                                None => break,
                            }
                        };
                        let elapsed = last_poll.elapsed().as_secs_f32();
                        last_poll = std::time::Instant::now();
                        let enq = durations.lock().expect("dur mutex").len();
                        if cur != 0 {
                            drained_polls = 0;
                            if cur != last_item {
                                if started {
                                    chars_before += chunk_chars.get(idx).copied().unwrap_or(0.0);
                                    idx += 1;
                                }
                                last_item = cur;
                                item_elapsed = 0.0;
                                started = true;
                            } else {
                                item_elapsed += elapsed * playback_rate;
                            }
                            let dur = durations
                                .lock()
                                .expect("dur mutex")
                                .get(idx)
                                .copied()
                                .unwrap_or(0.0);
                            let frac = if dur > 0.0 {
                                (item_elapsed / dur).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            let cur_chars = chunk_chars.get(idx).copied().unwrap_or(0.0);
                            let p =
                                ((chars_before + cur_chars * frac) / total_chars * 1000.0) as u64;
                            progress.store(p.min(1000), Ordering::Release);
                        } else if started && all_enqueued.load(Ordering::Acquire) {
                            if idx + 1 >= enq {
                                break;
                            }
                            drained_polls += 1;
                            if drained_polls > 16 {
                                break;
                            }
                        }
                    }
                    // Finalize the shared progress/active only if we're still the
                    // current read. If a re-trigger superseded us, the new read
                    // owns these — storing here would flash its bar full and mark
                    // it "done" immediately.
                    if generation.load(Ordering::Acquire) == n {
                        progress.store(1000, Ordering::Release);
                        active.store(false, Ordering::Release);
                    }
                })
            };

            // Synthesize each chunk (one at a time — the ONNX session is the
            // bottleneck) and append it; playback of earlier chunks overlaps.
            //
            // Predict the next chunk's synthesis cost from observed throughput.
            // Re-evaluate at each enqueue and use the current playback speed.
            let mut seconds_per_char = 0.0f32;
            let mut playing = false;
            let mut buffered = 0f32;
            // Cumulative synth time for the lead chunks before playback starts —
            // the compute half of time-to-first-word (vs. `model_ms`, the wait to
            // get the session). Reported in the "first audio" line below.
            let mut synth_ms = 0f32;
            let chunk_count = chunks.len();
            for (i, (chunk, hard)) in chunks.iter().enumerate() {
                // Bail if we were stopped or a newer read superseded us (a
                // re-trigger resets `active`, so also check our generation).
                if !active.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != n {
                    break;
                }
                // Gap after this chunk: a natural breath after a sentence, a short
                // flow after a soft (comma) break, and nothing after the last one.
                let tail_gap_ms = if i + 1 == chunk_count {
                    0
                } else if *hard {
                    SENTENCE_GAP_MS
                } else {
                    SOFT_GAP_MS
                };
                let t_synth = std::time::Instant::now();
                let wav = match synth_chunk_wav_cancellable(
                    &tts,
                    chunk,
                    voice.as_str(),
                    tail_gap_ms,
                    Some(Cancellation::new(generation.clone(), n)),
                )
                .await
                {
                    Some(w) => w,
                    None => break,
                };
                let elapsed = t_synth.elapsed().as_secs_f32();
                let observed = elapsed / chunk.chars().count().max(1) as f32;
                seconds_per_char = if i == 0 {
                    observed
                } else {
                    0.5 * seconds_per_char + 0.5 * observed
                };
                if !playing {
                    synth_ms += elapsed * 1000.0;
                }
                if !active.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != n {
                    break;
                }
                let secs = (wav.len().saturating_sub(44) / 2) as f32 / KOKORO_SAMPLE_RATE as f32;
                let temp = std::env::temp_dir().join(format!("murmur-kokoro-{n}-{i}.wav"));
                if std::fs::write(&temp, &wav).is_err() {
                    break;
                }
                let speed_guard = live_speed.lock().expect("speed mutex");
                let speed = *speed_guard;
                let target = buffer_target(
                    speed,
                    seconds_per_char,
                    chunks
                        .get(i + 1)
                        .map_or(0, |(text, _)| text.chars().count()),
                );
                let ok = {
                    let mut g = player_slot.lock().expect("player mutex");
                    match g.as_mut() {
                        // A newer read already owns the player slot (re-trigger):
                        // its generation bump means this queue isn't ours, so
                        // don't enqueue our chunk into it. Checked under the lock
                        // the new read installs its queue with, so it's race-free.
                        Some(_) if generation.load(Ordering::Acquire) != n => false,
                        Some(h) => {
                            // SAFETY: live AVQueuePlayer held under the mutex.
                            let rate: f32 = unsafe { msg_send![h.player.as_ptr(), rate] };
                            if playing && rate == 0.0 && !h.buffering {
                                h.buffering = true;
                                buffered = 0.0;
                                log::info!("tts/kokoro: queue drained; rebuilding buffer");
                            }
                            // SAFETY: live AVQueuePlayer; enqueue + rate control.
                            let ok = unsafe { enqueue_wav(h.player.as_ptr(), &temp) };
                            if ok {
                                h.temps.push(temp.clone());
                                durations.lock().expect("dur mutex").push(secs);
                                buffered += secs;
                                // SAFETY: live AVQueuePlayer held under the mutex.
                                unsafe {
                                    if h.buffering && (buffered >= target || i + 1 == chunk_count) {
                                        let _: () = msg_send![h.player.as_ptr(), setRate: speed];
                                        h.buffering = false;
                                        if !playing {
                                            log::info!(
                                                "tts/kokoro: first audio {:.0}ms after speak (model {:.0}ms, synth {:.0}ms @ {:.1}x realtime, {:.1}s buffered, target {:.1}s)",
                                                speak_t0.elapsed().as_secs_f32() * 1000.0,
                                                model_ms, synth_ms,
                                                buffered / (synth_ms / 1000.0).max(1e-3),
                                                buffered, target,
                                            );
                                        } else {
                                            log::info!(
                                                "tts/kokoro: resuming with {:.1}s buffered at {}x",
                                                buffered,
                                                speed
                                            );
                                        }
                                        playing = true;
                                    }
                                }
                            }
                            ok
                        }
                        None => false, // stopped
                    }
                };
                drop(speed_guard);
                if !ok {
                    let _ = std::fs::remove_file(&temp);
                    break;
                }
            }
            all_enqueued.store(true, Ordering::Release);
            // If we buffered audio but never crossed the prebuffer threshold, start
            // now. If nothing was enqueued (synth failed / stopped), clear `active`
            // so the progress task ends instead of spinning.
            let needs_start = player_slot
                .lock()
                .expect("player mutex")
                .as_ref()
                .is_some_and(|h| h.buffering);
            if needs_start || !playing {
                let speed_guard = live_speed.lock().expect("speed mutex");
                let speed = *speed_guard;
                let started_now = {
                    let mut g = player_slot.lock().expect("player mutex");
                    match g.as_mut() {
                        // Superseded by a newer read — its queue isn't ours to start.
                        Some(_) if generation.load(Ordering::Acquire) != n => false,
                        Some(h) if !durations.lock().expect("dur mutex").is_empty() => {
                            h.buffering = false;
                            // SAFETY: live AVQueuePlayer held under the mutex.
                            unsafe {
                                let _: () = msg_send![h.player.as_ptr(), setRate: speed];
                            }
                            log::info!(
                                "tts/kokoro: first audio {:.0}ms after speak (model {:.0}ms, synth {:.0}ms @ {:.1}x realtime, whole read buffered)",
                                speak_t0.elapsed().as_secs_f32() * 1000.0,
                                model_ms,
                                synth_ms,
                                buffered / (synth_ms / 1000.0).max(1e-3),
                            );
                            true
                        }
                        _ => false,
                    }
                };
                // Only clear `active` if we're still the current read — a newer
                // speak() (e.g. re-triggering read-aloud) now owns it.
                if !started_now && generation.load(Ordering::Acquire) == n {
                    active.store(false, Ordering::Release);
                }
            }
            // Wait for playback to finish (or stop()), then release our player and
            // its temp files — unless a newer read already replaced it, in which
            // case that read owns `active`/`player` and we must not touch them.
            let _ = prog.await;
            if generation.load(Ordering::Acquire) == n {
                active.store(false, Ordering::Release);
                *player_slot.lock().expect("player mutex") = None;
            }
        });
    }

    fn stop(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.active.store(false, Ordering::Release);
        *self.player.lock().expect("player mutex") = None;
        *self.preview_player.lock().expect("preview mutex") = None;
    }

    fn is_speaking(&self) -> bool {
        // The speak task owns `active` for the whole (possibly multi-chunk) read:
        // it clears it when the last chunk finishes or on stop(), so a brief
        // inter-chunk gap (player rate 0) isn't mistaken for "done".
        self.active.load(Ordering::Acquire)
    }

    fn progress(&self) -> Option<f32> {
        if self.active.load(Ordering::Acquire) {
            Some((self.progress.load(Ordering::Acquire) as f32 / 1000.0).clamp(0.0, 1.0))
        } else {
            None
        }
    }

    fn cycle_speed(&self) -> f32 {
        let new_speed = {
            let g = self.speed.lock().expect("speed mutex");
            let i = SPEEDS
                .iter()
                .position(|s| (*s - *g).abs() < 1e-3)
                .unwrap_or(0);
            SPEEDS[(i + 1) % SPEEDS.len()]
        };
        self.set_speed(new_speed);
        new_speed
    }

    fn set_speed(&self, speed: f32) {
        // Keep speed → player lock order shared with enqueue/start, so a
        // concurrent synthesis completion cannot restore an old speed.
        let mut speed_guard = self.speed.lock().expect("speed mutex");
        *speed_guard = speed;
        let g = self.player.lock().expect("player mutex");
        if let Some(h) = g.as_ref().filter(|h| !h.buffering) {
            // SAFETY: `h.player` is a live AVQueuePlayer held under the mutex;
            // `setRate:` takes a float.
            unsafe {
                let _: () = msg_send![h.player.as_ptr(), setRate: speed];
            }
        }
    }

    fn current_speed(&self) -> f32 {
        *self.speed.lock().expect("speed mutex")
    }

    fn set_voice(&self, voice_id: &str) {
        // Only accept a real Kokoro voice; ignore stale ids from another
        // provider left in config so we don't try to synthesize with a voice
        // Kokoro doesn't have.
        if KOKORO_VOICES.iter().any(|(id, _)| *id == voice_id) {
            *self.voice.lock().expect("voice mutex") = voice_id.to_string();
        }
    }

    fn current_voice(&self) -> Option<String> {
        Some(self.voice.lock().expect("voice mutex").clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_riff_wave() {
        let wav = pcm_f32_to_wav(&[0.0, 0.5, -0.5, 1.0], KOKORO_SAMPLE_RATE);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 4 * 2);
    }

    #[test]
    fn splits_long_text_into_chunks() {
        // Several sentences → multiple chunks, each reassembling to the input.
        let text = "This is the first sentence about something. Here is a second \
                    one that continues. And a third sentence to be sure it splits. \
                    Finally a fourth to push past the threshold.";
        let chunks = split_for_tts(text);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks, got {chunks:?}"
        );
        let words: usize = chunks
            .iter()
            .map(|(c, _)| c.split_whitespace().count())
            .sum();
        assert_eq!(words, text.split_whitespace().count());
    }

    /// Authoring helper for dev_terms.tab: phonemize a real English word/phrase
    /// with misaki (the model's own G2P) and print `word<TAB>ipa` to copy in.
    /// The degraded G2P letter-spells non-words, so use real words that *sound*
    /// like the term (e.g. "cube control" for kubectl).
    ///   PHONEMIZE="engine ex, cash, cube control" \
    ///     cargo test --lib tts::tests::phonemize_helper -- --ignored --nocapture
    #[test]
    #[ignore = "authoring helper; set PHONEMIZE=<comma-separated words>"]
    fn phonemize_helper() {
        let text = std::env::var("PHONEMIZE").unwrap_or_else(|_| "engine".into());
        for w in text.split(',') {
            let w = w.trim();
            eprintln!(
                "{w}\t{}",
                kokoro_en::g2p(w, false).unwrap_or_default().trim()
            );
        }
    }

    #[test]
    fn dev_terms_lexicon_uses_valid_phonemes() {
        let mut count = 0;
        for line in DEV_TERMS_TAB.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (word, ipa) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("lexicon line missing a TAB: {line:?}"));
            assert!(!word.trim().is_empty(), "empty word in {line:?}");
            assert!(!ipa.trim().is_empty(), "empty IPA for {word:?}");
            // The IPA must be within our model's phoneme vocab or synth logs
            // "unknown phone" and drops it. We ship Kokoro v1.0 (VOCAB_V10);
            // misaki itself emits v10-only symbols (e.g. ɚ), so validating
            // against v10 is what matches the model we actually load.
            let unknown = kokoro_en::unknown_phonemes(ipa.trim(), false);
            assert!(
                unknown.is_empty(),
                "{word:?} has out-of-vocab phonemes {unknown:?}"
            );
            count += 1;
        }
        assert!(
            count >= 10,
            "expected a seeded lexicon, found {count} entries"
        );
    }

    #[test]
    fn normalize_splits_code_style_words() {
        assert_eq!(normalize_for_tts("camelCase"), "camel Case");
        assert_eq!(normalize_for_tts("readAloud now"), "read Aloud now");
        assert_eq!(normalize_for_tts("HTTPServer"), "HTTP Server");
        assert_eq!(normalize_for_tts("read_aloud text"), "read aloud text");
        assert_eq!(normalize_for_tts("well-known"), "well known");
        assert_eq!(normalize_for_tts("mp3Player"), "mp3 Player");
        // Left alone: all-caps acronyms, plain words, standalone dashes/numbers.
        assert_eq!(normalize_for_tts("NASA and the USA"), "NASA and the USA");
        assert_eq!(normalize_for_tts("just normal text."), "just normal text.");
        assert_eq!(normalize_for_tts("score: 3 - 5"), "score: 3 - 5");
    }

    #[test]
    fn short_text_is_one_chunk() {
        // Text shorter than the first-chunk cap stays whole — nothing to gain by
        // splitting a clip that's already tiny.
        assert_eq!(
            split_for_tts("Hi there."),
            vec![("Hi there.".to_string(), true)]
        );
        assert_eq!(
            split_for_tts("read this aloud"),
            vec![("read this aloud".to_string(), true)]
        );
    }

    #[test]
    fn first_chunk_breaks_at_early_comma() {
        // For a fast first word, the opening chunk ends at the first comma — a
        // real pause — marked soft (false) so it flows into the rest. Later
        // chunks stay whole sentences. Words are preserved.
        let chunks =
            split_for_tts("Local dictation should feel instant, and read aloud starts right away.");
        assert!(chunks.len() >= 2, "expected an early split, got {chunks:?}");
        assert_eq!(chunks[0].0, "Local dictation should feel instant,");
        assert!(!chunks[0].1, "opening comma chunk should be a soft break");
        let words: usize = chunks
            .iter()
            .map(|(c, _)| c.split_whitespace().count())
            .sum();
        assert_eq!(words, 11);
    }

    #[test]
    fn later_chunks_prefer_sentence_ends() {
        // After the short startup chunks, normal sentences stay together.
        let text = "The quick brown fox jumps over the lazy dog again and again \
                    while the sleepy cat watches from the warm windowsill. Then it \
                    finally drifts off to sleep. A third sentence follows here.";
        let chunks = split_for_tts(text);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks, got {chunks:?}"
        );
        // The opening ramp can split at word boundaries; later chunks keep
        // sentence boundaries when they fit within the regular cap.
        for (c, hard) in chunks.iter().skip(2) {
            assert!(hard, "later chunks should yield hard breaks: {c:?}");
            assert!(
                matches!(c.chars().last(), Some('.' | '!' | '?' | ';' | ':')),
                "chunk should end at a sentence boundary, not mid-phrase: {c:?}"
            );
        }
        let words: usize = chunks
            .iter()
            .map(|(c, _)| c.split_whitespace().count())
            .sum();
        assert_eq!(words, text.split_whitespace().count());
    }

    #[test]
    fn opening_is_bounded_without_punctuation_and_preserves_unicode_words() {
        let text = "Résumé naïve café this opening sentence has no comma and continues for a long time without giving the reader anywhere obvious to breathe before it finally ends.";
        let chunks = split_for_tts(text);
        assert!(chunks[0].0.chars().count() <= 64);
        assert!(!chunks[0].1);
        assert_eq!(
            chunks
                .iter()
                .flat_map(|(s, _)| s.split_whitespace())
                .collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
        let long_word = "x".repeat(300);
        assert_eq!(split_for_tts(&long_word)[0].0, long_word);
    }

    #[test]
    fn buffering_covers_next_synthesis_and_scales_with_speed() {
        assert!((buffer_target(1.0, 0.01, 100) - 1.33).abs() < 0.001);
        assert!((buffer_target(2.0, 0.01, 100) - 2.66).abs() < 0.001);
        assert!(buffer_target(1.0, 0.02, 100) > buffer_target(1.0, 0.01, 100));
        assert_eq!(buffer_target(2.0, 1.0, 200), 6.0);
        assert_eq!(buffer_target(2.0, 1.0, 0), 0.0);
    }

    #[test]
    fn voices_for_matches_provider() {
        assert_eq!(voices_for("kokoro"), KOKORO_VOICES);
        assert!(voices_for("native").is_empty());
    }

    #[test]
    fn kokoro_set_voice_accepts_only_kokoro_voices() {
        let s = KokoroSpeaker::new(PathBuf::new(), PathBuf::new());
        assert_eq!(s.current_voice().as_deref(), Some(KOKORO_DEFAULT_VOICE));
        s.set_voice("am_michael");
        assert_eq!(s.current_voice().as_deref(), Some("am_michael"));
        // A stale id from another provider is ignored.
        s.set_voice("bIHbv24MWmeRgasZH58o");
        assert_eq!(s.current_voice().as_deref(), Some("am_michael"));
    }

    /// End-to-end Kokoro synthesis; needs the model + voices on disk. Ignored by
    /// default. Run manually (writes /tmp/murmur-kokoro-test.wav to `afplay`):
    ///   cargo test --no-default-features -- --ignored kokoro_synth --nocapture
    #[test]
    #[ignore = "needs the Kokoro model + voices in <app-support>/murmur/models"]
    fn kokoro_synth() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tts = KokoroTts::new(kokoro_model_path().unwrap(), kokoro_voices_dir().unwrap())
                .await
                .unwrap();
            let (samples, dur) = tts
                .synth(
                    "Hello from Murmur. This is a local neural voice.",
                    "af_heart",
                )
                .await
                .unwrap();
            eprintln!(
                "SYNTH: {} samples, {:.2}s",
                samples.len(),
                dur.as_secs_f32()
            );
            assert!(samples.len() > 1000);
            std::fs::write(
                "/tmp/murmur-kokoro-test.wav",
                pcm_f32_to_wav(&samples, KOKORO_SAMPLE_RATE),
            )
            .unwrap();
        });
    }

    #[test]
    #[ignore = "requires installed Kokoro assets and plays local audio"]
    fn kokoro_worker_playback_and_stop() {
        pin_coreml_compute_units();
        let speaker =
            KokoroSpeaker::new(kokoro_model_path().unwrap(), kokoro_voices_dir().unwrap());
        speaker.set_voice("am_puck");
        speaker.speak("Murmur's background speech worker is ready.");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut saw_progress = false;
        while speaker.is_speaking() && !saw_progress && std::time::Instant::now() < deadline {
            saw_progress |= speaker.progress().is_some_and(|p| p > 0.0 && p < 1.0);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(saw_progress, "AVQueuePlayer never advanced");
        // This headless test verifies queue startup and stop. Natural playback
        // completion needs a separate live-app check with its native event loop.
        speaker.stop();
        assert!(!speaker.is_speaking());
        speaker.speak("This cancelled sentence must not start playing after stop.");
        speaker.stop();
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert!(!speaker.is_speaking());
        assert!(speaker.player.lock().unwrap().is_none());
        // A cache-miss preview shares the same cancellation epoch.
        speaker.preview("This uncached preview is cancelled before it can play.");
        speaker.stop();
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert!(speaker.preview_player.lock().unwrap().is_none());
    }

    /// Compare old sentence-sized startup against the actual new split/buffer
    /// policy using the installed model. No playback and no timing assertions.
    #[test]
    #[ignore = "needs the local Kokoro model; measures latency"]
    fn kokoro_startup_benchmark() {
        pin_coreml_compute_units();
        install_g2p_lexicon();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tts = KokoroTts::new(kokoro_model_path().unwrap(), kokoro_voices_dir().unwrap()).await.unwrap();
            synth_chunk_wav(&tts, "Ready to read.", "af_heart", 0).await.unwrap();
            let text = "Local dictation should feel instant and reading a long opening sentence without commas should start quickly while the rest of the text is generated in the background. Another sentence follows.";
            let old_first = text.split_once(". ").unwrap().0.to_string() + ".";
            let t = std::time::Instant::now();
            synth_chunk_wav(&tts, &old_first, "af_heart", SENTENCE_GAP_MS).await.unwrap();
            let baseline = t.elapsed();
            let chunks = split_for_tts(text);
            let t = std::time::Instant::now();
            let mut buffered = 0.0;
            let mut cost = 0.0;
            for (i, (text, hard)) in chunks.iter().enumerate() {
                let tick = std::time::Instant::now();
                let wav = synth_chunk_wav(&tts, text, "af_heart", if *hard { SENTENCE_GAP_MS } else { SOFT_GAP_MS }).await.unwrap();
                let observed = tick.elapsed().as_secs_f32() / text.chars().count() as f32;
                cost = if i == 0 { observed } else { (cost + observed) / 2.0 };
                buffered += (wav.len() - 44) as f32 / 2.0 / KOKORO_SAMPLE_RATE as f32;
                let target = buffer_target(1.0, cost, chunks.get(i+1).map_or(0, |(s, _)| s.chars().count()));
                if buffered >= target || i + 1 == chunks.len() {
                    eprintln!("KOKORO STARTUP: sentence {:.0}ms; incremental {:.0}ms ({} chunk(s), {:.2}s buffered, target {:.2}s)", baseline.as_secs_f32()*1000.0, t.elapsed().as_secs_f32()*1000.0, i+1, buffered, target);
                    break;
                }
            }
        });
    }

    // ── trim_silence: the read-aloud edge-trim (Layer 1 regression guards) ──
    // A bug here clips word onsets/tails or fails to remove Kokoro's padding, so
    // it's exactly the kind of quality regression we want caught in CI.

    const GUARD: usize = KOKORO_SAMPLE_RATE as usize / 100; // 10ms, mirrors trim_silence

    /// Build a clip: `lead` silent samples, `voiced` loud samples, `tail` silent.
    fn clip(lead: usize, voiced: usize, tail: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; lead];
        v.extend(std::iter::repeat(0.5f32).take(voiced));
        v.extend(std::iter::repeat(0.0f32).take(tail));
        v
    }

    #[test]
    fn trim_silence_removes_padding_but_keeps_a_guard() {
        // 2000 lead + 1000 voiced + 3000 tail. Trim keeps the voiced span plus a
        // GUARD on each side (clamped to the clip), never clipping the onset/tail.
        let samples = clip(2000, 1000, 3000);
        let out = trim_silence(&samples);
        // Guard preserved on each side around the 1000 voiced samples.
        assert_eq!(out.len(), 1000 + 2 * GUARD);
        // The voiced region is fully present (all 0.5 samples retained).
        assert_eq!(out.iter().filter(|s| **s == 0.5).count(), 1000);
    }

    #[test]
    fn trim_silence_never_clips_the_voiced_region() {
        // Even with a tiny lead/tail (< GUARD), every voiced sample survives.
        for (lead, tail) in [(0, 0), (5, 5), (GUARD * 3, GUARD * 3)] {
            let samples = clip(lead, 800, tail);
            let out = trim_silence(&samples);
            assert_eq!(
                out.iter().filter(|s| **s == 0.5).count(),
                800,
                "voiced samples lost for lead={lead} tail={tail}"
            );
            // Never longer than the input, never shorter than the voiced core.
            assert!(out.len() <= samples.len());
            assert!(out.len() >= 800);
        }
    }

    #[test]
    fn trim_silence_all_silence_is_returned_unchanged() {
        // Below-threshold everywhere: hand back the input rather than an empty
        // slice, so a silent chunk still enqueues (and its gap still applies).
        let samples = vec![0.0f32; 5000];
        assert_eq!(trim_silence(&samples).len(), samples.len());
        // Sub-threshold noise (< 0.005) also counts as silence.
        let quiet = vec![0.004f32; 5000];
        assert_eq!(trim_silence(&quiet).len(), quiet.len());
    }

    // ── chunk gap policy: mirrors the read-aloud loop's per-chunk gap choice ──
    // Kept in lockstep with lib-side logic; a change to the constants or the
    // sentence/soft/last rule shows up here.

    /// The gap the read-aloud loop appends after chunk `i` of `n`, given whether
    /// it ended at a sentence boundary. Mirrors the match in `speak`.
    fn chunk_gap_ms(i: usize, n: usize, hard: bool) -> u32 {
        if i + 1 == n {
            0
        } else if hard {
            SENTENCE_GAP_MS
        } else {
            SOFT_GAP_MS
        }
    }

    #[test]
    fn chunk_gap_policy_last_chunk_is_seamless() {
        // The final chunk never gets a trailing gap (nothing follows it).
        assert_eq!(chunk_gap_ms(3, 4, true), 0);
        assert_eq!(chunk_gap_ms(0, 1, true), 0);
    }

    #[test]
    fn chunk_gap_policy_sentence_vs_soft() {
        // Non-final: a sentence end gets the longer breath, a soft (comma) break
        // the shorter flow. (The breath > flow invariant is a compile-time
        // assert next to the constants.)
        assert_eq!(chunk_gap_ms(0, 3, true), SENTENCE_GAP_MS);
        assert_eq!(chunk_gap_ms(1, 3, false), SOFT_GAP_MS);
    }

    // ── split_for_tts invariants over adversarial inputs ──

    #[test]
    fn split_never_breaks_mid_word_and_preserves_words() {
        // No-punctuation, comma-only, and long-token inputs must still split
        // without losing or splitting words.
        for text in [
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen",
            "alpha, beta, gamma, delta, epsilon, zeta, eta, theta, iota, kappa, lambda, mu",
            &"supercalifragilisticexpialidocious ".repeat(20),
        ] {
            let chunks = split_for_tts(text);
            let joined: String = chunks
                .iter()
                .map(|(c, _)| c.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                joined.split_whitespace().count(),
                text.split_whitespace().count(),
                "word count changed for {text:?}"
            );
            // Every whitespace-delimited token in the output is a whole token
            // from the input — a mid-word break would produce a fragment.
            let input_words: std::collections::HashSet<&str> = text.split_whitespace().collect();
            for w in joined.split_whitespace() {
                let bare = w.trim_end_matches([',', '.', '!', '?', ';', ':']);
                assert!(
                    input_words.contains(w) || input_words.contains(bare),
                    "produced a non-word fragment {w:?} for {text:?}"
                );
            }
        }
    }

    #[test]
    fn split_handles_empty_and_whitespace() {
        // Never panics and never invents content: any chunk from empty/whitespace
        // input is itself empty (the defensive fallback pushes a blank chunk).
        for text in ["", "   \n  "] {
            for (c, _) in split_for_tts(text) {
                assert!(c.trim().is_empty(), "invented content {c:?} from {text:?}");
            }
        }
    }
}
