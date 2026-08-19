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

use anyhow::Result;
use kokoro_en::KokoroTts;
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
// Kokoro backend — local neural TTS (kokoro-en: ONNX via `ort` + bundled
// espeak-ng, CoreML-accelerated). Fully on-device; no API key.
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
    speed: Mutex<f32>,
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
            speed: Mutex::new(1.0),
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

/// Pin Kokoro's CoreML compute path. The crate defaults to `ALL` (ANE + GPU +
/// CPU), which lets CoreML silently fall off the power-managed Apple Neural
/// Engine mid-session — synth then drops from ~4.5× realtime to ~1.7×, the TTS
/// latency swings we measured. Pinning the Metal GPU trades a little peak
/// throughput for a *consistent* path: the GPU isn't power-gated / shared the way
/// the ANE is, so no silent fallback. Set once at startup, **before the first
/// synth** (the crate reads it when it builds the session). Respects an explicit
/// user override so it can be A/B'd from the environment.
pub fn pin_coreml_compute_units() {
    if std::env::var_os("KOKORO_COREML_COMPUTE_UNITS").is_some() {
        log::info!("tts: KOKORO_COREML_COMPUTE_UNITS set by env — leaving as-is");
        return;
    }
    // Safe on edition 2021; set at startup before any synth reads it.
    std::env::set_var("KOKORO_COREML_COMPUTE_UNITS", "cpu_and_gpu");
    log::info!("tts: pinned CoreML compute units → cpu_and_gpu (consistent GPU path)");
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

/// Split read-aloud text into `(chunk, hard_break)` pairs, breaking **only at
/// natural pauses** so a boundary never lands mid-phrase (Kokoro synthesizes each
/// chunk as an independent utterance with its own prosody, so a mid-clause seam
/// sounds like an unnatural pause). Rules:
/// - Break at sentence ends (`. ! ? ; :` or newline) past `MIN` → a whole
///   sentence, `hard = true` (a natural breath follows it).
/// - **First chunk only:** also break at the earliest comma within `FIRST_MAX`
///   → a small opening chunk so the first word plays sooner. It's a *real* pause
///   (a comma), marked `hard = false` so it flows into the rest of the sentence.
/// - Only a sentence longer than `CAP` is broken further, at the last comma
///   before the cap (else the last word boundary). Rare; most sentences stay whole.
///
/// The `hard` flag drives how much silence we append after the chunk (see
/// `SENTENCE_GAP_MS` / `SOFT_GAP_MS`); trimming makes seams gapless so these small
/// controlled gaps are the *only* spacing between chunks.
fn split_for_tts(text: &str) -> Vec<(String, bool)> {
    let text = text.trim();
    const MIN: usize = 16; // keep tiny fragments merged into the next clause
    const FIRST_MAX: usize = 100; // first chunk may break at an *early* comma only
    const CAP: usize = 220; // only a long sentence is broken further, at a comma
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
        let sentence_end = matches!(ch, '.' | '!' | '?' | '\n' | ';' | ':');
        if sentence_end && cur.trim_end().len() >= MIN {
            push_chunk(&mut chunks, &cur, true);
            cur.clear();
            last_comma = 0;
            last_space = 0;
        } else if first && ch == ',' && cur.trim_end().len() >= MIN && cur.len() <= FIRST_MAX {
            // Fast first word: end the opening chunk at the first comma (a real
            // pause). Soft break — flows into the rest of the sentence.
            push_chunk(&mut chunks, &cur, false);
            cur.clear();
            last_comma = 0;
            last_space = 0;
        } else if cur.len() >= CAP {
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
            } else {
                push_chunk(&mut chunks, &cur, false);
                cur.clear();
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
    let t0 = std::time::Instant::now();
    match tts.synth(text, voice).await {
        Ok((samples, _)) => {
            // Kokoro pads every chunk with edge silence (~220ms lead, ~430ms tail
            // measured). Trim it so chunks play gapless — the leading trim comes
            // straight off time-to-first-word — then append a short, controlled
            // gap so chunks don't slam together (0 = seamless, for the last chunk).
            let voiced = trim_silence(&samples);
            let gap = tail_gap_ms as usize * KOKORO_SAMPLE_RATE as usize / 1000;
            let mut out = Vec::with_capacity(voiced.len() + gap);
            out.extend_from_slice(voiced);
            out.resize(out.len() + gap, 0.0);
            // Synth-vs-realtime ratio: >~2x means the ONNX session is really on
            // the ANE/GPU (CoreML), ~1x or slower means it fell back to CPU. At
            // `debug` (per-chunk, ~20 lines/read) — set RUST_LOG=debug to see it
            // when diagnosing synth speed; time-to-first-word below stays `info`.
            let synth_secs = t0.elapsed().as_secs_f32();
            let audio_secs = out.len() as f32 / KOKORO_SAMPLE_RATE as f32;
            log::debug!(
                "tts/kokoro: synth {} chars → {:.2}s audio in {:.0}ms ({:.1}x realtime) [trimmed {:.0}ms pad]",
                text.chars().count(),
                audio_secs,
                synth_secs * 1000.0,
                audio_secs / synth_secs.max(1e-3),
                samples.len().saturating_sub(voiced.len()) as f32 / KOKORO_SAMPLE_RATE as f32 * 1000.0,
            );
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
fn play_preview_file(slot: &Arc<Mutex<Option<KokoroQueue>>>, path: &std::path::Path, speed: f32) {
    let Some(qp) = new_queue_player() else {
        return;
    };
    let queue = KokoroQueue {
        player: qp,
        temps: Vec::new(),
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
    *slot.lock().expect("preview mutex") = Some(queue);
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
        if path.exists() {
            play_preview_file(&self.preview_player, &path, speed); // cached → instant
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
            if let Some(wav) = synth_chunk_wav(&tts, &text, &voice, 0).await {
                if write_cache_file(&path, &wav) {
                    play_preview_file(&slot, &path, speed);
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
        let speed = *self.speed.lock().expect("speed mutex");
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
            // Load + cache the model on first use.
            let tts = {
                let mut guard = tts_cell.lock().await;
                if guard.is_none() {
                    match KokoroTts::new(&model_path, &voices_path).await {
                        Ok(t) => *guard = Some(Arc::new(t)),
                        Err(e) => {
                            log::warn!("tts/kokoro: load model failed: {e}");
                            active.store(false, Ordering::Release);
                            return;
                        }
                    }
                }
                guard.as_ref().expect("just loaded").clone()
            };
            // Time to get the session: ~0 when warm; seconds means it reloaded or
            // waited on the shared session lock (another synth/preview in flight).
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
            *player_slot.lock().expect("player mutex") = Some(KokoroQueue {
                player: qp,
                temps: Vec::new(),
            });

            let chunk_chars: Arc<Vec<f32>> =
                Arc::new(chunks.iter().map(|(c, _)| c.chars().count() as f32).collect());
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
                    let mut item_start = std::time::Instant::now();
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
                        let cur = {
                            let g = player_slot.lock().expect("player mutex");
                            match g.as_ref() {
                                Some(h) => {
                                    // SAFETY: live AVQueuePlayer held under the mutex.
                                    unsafe {
                                        let c: *mut AnyObject =
                                            msg_send![h.player.as_ptr(), currentItem];
                                        c as usize
                                    }
                                }
                                None => break,
                            }
                        };
                        let enq = durations.lock().expect("dur mutex").len();
                        if cur != 0 {
                            drained_polls = 0;
                            if cur != last_item {
                                if started {
                                    chars_before += chunk_chars.get(idx).copied().unwrap_or(0.0);
                                    idx += 1;
                                }
                                last_item = cur;
                                item_start = std::time::Instant::now();
                                started = true;
                            }
                            let dur = durations
                                .lock()
                                .expect("dur mutex")
                                .get(idx)
                                .copied()
                                .unwrap_or(0.0);
                            let frac = if dur > 0.0 {
                                (item_start.elapsed().as_secs_f32() * speed / dur).clamp(0.0, 1.0)
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
            // Hold playback until this many seconds of audio are buffered, then
            // start. Kept small: synth measured ~2–4.5× real time on the ANE (well
            // above 1×), so the lead only *grows* once playing and low prebuffer
            // won't stall — it just sets how soon the first word plays. The first
            // chunk is a small opening clause when the sentence has an early comma
            // (split_for_tts), so playback begins as soon as that synthesizes.
            const PREBUFFER_SECS: f32 = 0.6;
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
                let wav = match synth_chunk_wav(&tts, chunk, voice.as_str(), tail_gap_ms).await {
                    Some(w) => w,
                    None => break,
                };
                if !playing {
                    synth_ms += t_synth.elapsed().as_secs_f32() * 1000.0;
                }
                if !active.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != n {
                    break;
                }
                let secs = (wav.len().saturating_sub(44) / 2) as f32 / KOKORO_SAMPLE_RATE as f32;
                let temp = std::env::temp_dir().join(format!("murmur-kokoro-{n}-{i}.wav"));
                if std::fs::write(&temp, &wav).is_err() {
                    break;
                }
                let ok = {
                    let mut g = player_slot.lock().expect("player mutex");
                    match g.as_mut() {
                        // A newer read already owns the player slot (re-trigger):
                        // its generation bump means this queue isn't ours, so
                        // don't enqueue our chunk into it. Checked under the lock
                        // the new read installs its queue with, so it's race-free.
                        Some(_) if generation.load(Ordering::Acquire) != n => false,
                        Some(h) => {
                            // SAFETY: live AVQueuePlayer; enqueue + rate control.
                            let ok = unsafe { enqueue_wav(h.player.as_ptr(), &temp) };
                            if ok {
                                h.temps.push(temp.clone());
                                durations.lock().expect("dur mutex").push(secs);
                                buffered += secs;
                                // SAFETY: live AVQueuePlayer held under the mutex.
                                unsafe {
                                    let rate: f32 = msg_send![h.player.as_ptr(), rate];
                                    if !playing {
                                        if buffered >= PREBUFFER_SECS || i + 1 == chunk_count {
                                            let _: () =
                                                msg_send![h.player.as_ptr(), setRate: speed];
                                            playing = true;
                                            log::info!(
                                                "tts/kokoro: first audio {:.0}ms after speak (model {:.0}ms, synth {:.0}ms @ {:.1}x realtime, {:.1}s buffered)",
                                                speak_t0.elapsed().as_secs_f32() * 1000.0,
                                                model_ms,
                                                synth_ms,
                                                buffered / (synth_ms / 1000.0).max(1e-3),
                                                buffered,
                                            );
                                        }
                                    } else if rate == 0.0 {
                                        // Queue drained mid-read (synth fell behind) → resume.
                                        // Logged: a mid-read stall is a "seconds" pause
                                        // the user hears in the middle of a read.
                                        log::info!(
                                            "tts/kokoro: playback stalled at chunk {}/{} (synth fell behind) — resuming",
                                            i + 1,
                                            chunk_count,
                                        );
                                        let _: () = msg_send![h.player.as_ptr(), setRate: speed];
                                    }
                                }
                            }
                            ok
                        }
                        None => false, // stopped
                    }
                };
                if !ok {
                    let _ = std::fs::remove_file(&temp);
                    break;
                }
            }
            all_enqueued.store(true, Ordering::Release);
            // If we buffered audio but never crossed the prebuffer threshold, start
            // now. If nothing was enqueued (synth failed / stopped), clear `active`
            // so the progress task ends instead of spinning.
            if !playing {
                let started_now = {
                    let g = player_slot.lock().expect("player mutex");
                    match g.as_ref() {
                        // Superseded by a newer read — its queue isn't ours to start.
                        Some(_) if generation.load(Ordering::Acquire) != n => false,
                        Some(h) if !durations.lock().expect("dur mutex").is_empty() => {
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
        *self.speed.lock().expect("speed mutex") = speed;
        let g = self.player.lock().expect("player mutex");
        if let Some(h) = g.as_ref() {
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
        assert_eq!(split_for_tts("Hi there."), vec![("Hi there.".to_string(), true)]);
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
    fn chunks_break_only_at_sentence_ends() {
        // Every chunk of a normal (sub-CAP) multi-sentence read must end at a
        // sentence boundary, so Kokoro never resets prosody mid-phrase (which
        // sounds like an unnatural pause). Words are preserved.
        let text = "The quick brown fox jumps over the lazy dog again and again \
                    while the sleepy cat watches from the warm windowsill. Then it \
                    finally drifts off to sleep. A third sentence follows here.";
        let chunks = split_for_tts(text);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks, got {chunks:?}"
        );
        // This text has no early comma, so every chunk is a whole sentence (the
        // first-chunk comma exception is covered separately).
        for (c, hard) in &chunks {
            assert!(hard, "sentence-only text should yield hard breaks: {c:?}");
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
}
