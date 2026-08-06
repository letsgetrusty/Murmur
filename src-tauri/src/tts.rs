// Phase 3: read-aloud.
//
// Two backends behind a shared `Speaker` trait, chosen at startup based on
// whether an ElevenLabs API key is in Keychain. The mac backend uses
// AVSpeechSynthesizer (free, offline, but the default voice is rough). The
// ElevenLabs backend streams MP3 from the REST API and plays it through
// AVFoundation's AVPlayer with `audioTimePitchAlgorithm = .spectral` — that
// gives us pitch-preserving 2× speed (and beyond) without any resampling
// chipmunk effect.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use kokoro_en::KokoroTts;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;
use tokio::sync::Mutex as AsyncMutex;

use crate::secrets;

// Link AVFoundation — we only message its classes, no extern fns.
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// `Speaker` is the seam: any backend that can turn text into audio output.
/// All methods are non-blocking; `speak` and `stop` return immediately.
pub trait Speaker: Send + Sync {
    fn speak(&self, text: &str);
    fn stop(&self);
    fn is_speaking(&self) -> bool;

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

/// Voices the tray menu exposes for the ElevenLabs backend. Kept here so the
/// IDs and friendly names are colocated.
// Premade ElevenLabs voices (available to every account, verified usable).
// Chosen to be distinct and friendly — the old 2023 default set had two
// near-identical males (Antoni/Adam). Names match the ElevenLabs library.
pub const ELEVENLABS_VOICES: &[(&str, &str)] = &[
    ("21m00Tcm4TlvDq8ikWAM", "Rachel"),  // calm female
    ("XrExE9yKIg1WjnnlVkGX", "Matilda"), // warm, friendly female
    ("cgSgspJ2msm6clMCkdW9", "Jessica"), // expressive, friendly female
    ("pFZP5JQG7iQjIQuC4Bku", "Lily"),    // warm British female
    ("bIHbv24MWmeRgasZH58o", "Will"),    // young, chill male
    ("iP95p4xoKVk53GoZ742B", "Chris"),   // casual, natural male
    ("cjVigY5qzO86Huf0OWal", "Eric"),    // friendly, classy male
    ("JBFqnCBsd6RMkjVDRZzb", "George"),  // warm, mature British male
    ("nPczCjzI2devNBz1zQrb", "Brian"),   // deep narration male
];

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

// ──────────────────────────────────────────────────────────────────────────
// ElevenLabs backend (AVPlayer playback).
// ──────────────────────────────────────────────────────────────────────────

const DEFAULT_VOICE_ID: &str = "21m00Tcm4TlvDq8ikWAM";
const DEFAULT_MODEL_ID: &str = "eleven_turbo_v2_5";
/// Spectral is highest quality for speech — preserves formants and pitch.
const PITCH_ALG: &str = "AVAudioTimePitchAlgorithmSpectral";

/// SAFETY: the AVPlayer pointer is only mutated under `ElevenLabsSpeaker::player`'s
/// mutex. AVPlayer audio playback methods are documented as safe from any
/// thread; main-thread requirements only apply to AVPlayerView UI.
struct PlayerHolder {
    player: NonNull<AnyObject>,
    /// MP3 we wrote for this player; deleted on Drop so we don't leak temp files.
    temp_path: PathBuf,
}
// SAFETY: the AVPlayer pointer is only accessed under the player mutex, and
// AVPlayer playback methods are thread-safe (the main-thread rule is AVPlayer
// *UI* only).
unsafe impl Send for PlayerHolder {}

impl Drop for PlayerHolder {
    fn drop(&mut self) {
        // SAFETY: `self.player` is a retained AVPlayer; `pause`/`release` are
        // valid selectors and `release` balances the `retain` exactly once.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), pause];
            let _: () = msg_send![self.player.as_ptr(), release];
        }
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

/// Create a retained AVPlayer for `temp_path` and start playback at `speed` with
/// pitch-preserving spectral scaling. Returns the holder, or `None` (deleting
/// the temp file) on failure. Shared by the ElevenLabs and Kokoro backends,
/// which both play a decoded audio file through AVFoundation.
fn spawn_avplayer(temp_path: PathBuf, speed: f32) -> Option<PlayerHolder> {
    // SAFETY: the NSURL/AVPlayer selectors and argument types match their ObjC
    // signatures; `player` is null-checked before it's retained and stored, and
    // its item is null-checked before use.
    unsafe {
        let path_str = NSString::from_str(temp_path.to_str().unwrap_or_default());
        let nsurl: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*path_str];
        // `playerWithURL:` returns an autoreleased instance; retain so we can
        // hold it across thread boundaries.
        let player: *mut AnyObject = msg_send![class!(AVPlayer), playerWithURL: nsurl];
        if player.is_null() {
            log::warn!("tts: AVPlayer playerWithURL returned null");
            let _ = std::fs::remove_file(&temp_path);
            return None;
        }
        let _: () = msg_send![player, retain];
        // Pitch-preserving rate scaling lives on the player item.
        let item: *mut AnyObject = msg_send![player, currentItem];
        if !item.is_null() {
            let algo = NSString::from_str(PITCH_ALG);
            let _: () = msg_send![item, setAudioTimePitchAlgorithm: &*algo];
        }
        // Setting a non-zero rate starts playback.
        let _: () = msg_send![player, setRate: speed];
        let ptr = NonNull::new(player).expect("just checked non-null above");
        Some(PlayerHolder {
            player: ptr,
            temp_path,
        })
    }
}

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

pub struct ElevenLabsSpeaker {
    client: reqwest::Client,
    voice_id: Mutex<String>,
    api_key: Mutex<Option<String>>,
    speed: Mutex<f32>,
    /// Current AVPlayer + temp file. `None` when nothing is playing.
    player: Arc<Mutex<Option<PlayerHolder>>>,
    /// True between `speak()` and either an error, natural end, or `stop()`.
    /// Lets `is_speaking()` return `true` during the API-fetch window before
    /// the AVPlayer instance exists.
    active: Arc<AtomicBool>,
    /// Monotonic counter to generate unique temp file paths.
    counter: AtomicU64,
}

impl ElevenLabsSpeaker {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            voice_id: Mutex::new(DEFAULT_VOICE_ID.into()),
            api_key: Mutex::new(None),
            speed: Mutex::new(1.0),
            player: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(false)),
            counter: AtomicU64::new(0),
        }
    }

    fn api_key(&self) -> Result<String> {
        let mut cached = self
            .api_key
            .lock()
            .map_err(|_| anyhow!("elevenlabs key cache poisoned"))?;
        if let Some(k) = cached.as_ref() {
            return Ok(k.clone());
        }
        let k = secrets::get(secrets::ELEVENLABS_API_KEY).map_err(|_| {
            anyhow!(
                "no ElevenLabs API key in Keychain. Set with:\n  security add-generic-password -A -s murmur -a elevenlabs_api_key -w"
            )
        })?;
        *cached = Some(k.clone());
        Ok(k)
    }
}

impl Default for ElevenLabsSpeaker {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for ElevenLabsSpeaker {
    fn speak(&self, text: &str) {
        let api_key = match self.api_key() {
            Ok(k) => k,
            Err(e) => {
                log::warn!("tts/11labs: {e}");
                return;
            }
        };
        let voice_id = self.voice_id.lock().expect("voice id mutex").clone();
        let speed = *self.speed.lock().expect("speed mutex");
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let url = format!("https://api.elevenlabs.io/v1/text-to-speech/{voice_id}");
        // Always request audio at 1.0× from the API — AVPlayer applies speed
        // locally with pitch preservation, which lets us go past the API's
        // 1.2× cap.
        let body = serde_json::json!({
            "text": text,
            "model_id": DEFAULT_MODEL_ID,
            "voice_settings": { "stability": 0.5, "similarity_boost": 0.8 }
        });
        let client = self.client.clone();
        let player_slot = self.player.clone();
        let active = self.active.clone();
        // Mark active so `is_speaking()` returns true immediately, even
        // before the API call has returned and the player exists.
        active.store(true, Ordering::Release);

        tauri::async_runtime::spawn(async move {
            log::info!("tts/11labs: POST /text-to-speech");
            let resp = client
                .post(url)
                .header("xi-api-key", api_key)
                .header("accept", "audio/mpeg")
                .json(&body)
                .send()
                .await;
            let bytes = match resp {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => {
                        log::info!("tts/11labs: received {} bytes of audio", b.len());
                        b.to_vec()
                    }
                    Err(e) => {
                        log::warn!("tts/11labs: read body failed: {e}");
                        active.store(false, Ordering::Release);
                        return;
                    }
                },
                Ok(r) => {
                    let s = r.status();
                    let body = r.text().await.unwrap_or_default();
                    log::warn!("tts/11labs: {s}: {body}");
                    active.store(false, Ordering::Release);
                    return;
                }
                Err(e) => {
                    log::warn!("tts/11labs: request failed: {e}");
                    active.store(false, Ordering::Release);
                    return;
                }
            };

            let temp_path = std::env::temp_dir().join(format!("murmur-tts-{n}.mp3"));
            if let Err(e) = std::fs::write(&temp_path, &bytes) {
                log::warn!("tts/11labs: write temp file failed: {e}");
                active.store(false, Ordering::Release);
                return;
            }
            log::info!("tts/11labs: play rate={speed}");
            match spawn_avplayer(temp_path, speed) {
                // Replacing drops the previous holder, pausing+releasing the
                // prior player and removing its temp file.
                Some(holder) => *player_slot.lock().expect("player mutex") = Some(holder),
                None => active.store(false, Ordering::Release),
            }
        });
    }

    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        // Drop the current player; its `Drop` impl pauses, releases, and
        // cleans up the temp file.
        *self.player.lock().expect("player mutex") = None;
    }

    fn is_speaking(&self) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let g = self.player.lock().expect("player mutex");
        match g.as_ref() {
            // Active but no player yet — still fetching audio.
            None => true,
            // SAFETY: `h.player` is a live AVPlayer held under the mutex; `rate`
            // returns a float.
            Some(h) => unsafe {
                let rate: f32 = msg_send![h.player.as_ptr(), rate];
                if rate != 0.0 {
                    true
                } else {
                    // AVPlayer hit end-of-media (rate auto-resets to 0).
                    // Mark inactive so subsequent calls return quickly.
                    self.active.store(false, Ordering::Release);
                    false
                }
            },
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
        // Apply live to any in-flight player so the user hears the change
        // immediately, not on the next utterance.
        let g = self.player.lock().expect("player mutex");
        if let Some(h) = g.as_ref() {
            // SAFETY: `h.player` is a live AVPlayer held under the mutex;
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
        *self.voice_id.lock().expect("voice id mutex") = voice_id.to_string();
    }

    fn current_voice(&self) -> Option<String> {
        Some(self.voice_id.lock().expect("voice id mutex").clone())
    }
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

const KOKORO_DEFAULT_VOICE: &str = "af_heart";
const KOKORO_SAMPLE_RATE: u32 = 24_000;

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
pub async fn ensure_kokoro_assets() -> Result<()> {
    const BASE: &str = "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main";
    let model = kokoro_model_path()?;
    if !model.exists() {
        if let Some(p) = model.parent() {
            std::fs::create_dir_all(p).ok();
        }
        log::info!("tts/kokoro: downloading model (~310 MB, one-time)…");
        download_to(&format!("{BASE}/onnx/model.onnx"), &model).await?;
    }
    let dir = kokoro_voices_dir()?;
    std::fs::create_dir_all(&dir).ok();
    for (id, _) in KOKORO_VOICES {
        let dst = dir.join(format!("{id}.bin"));
        if !dst.exists() {
            log::info!("tts/kokoro: downloading voice '{id}'…");
            download_to(&format!("{BASE}/voices/{id}.bin"), &dst).await?;
        }
    }
    log::info!("tts/kokoro: assets ready");
    Ok(())
}

/// Stream a URL to `dst` via a `.part` temp + rename, so a file at `dst` is
/// always complete.
async fn download_to(url: &str, dst: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let mut resp = reqwest::Client::new().get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("download {url} failed: HTTP {}", resp.status()));
    }
    let part = dst.with_extension("part");
    let mut file = std::fs::File::create(&part)?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
    }
    file.flush().ok();
    drop(file);
    std::fs::rename(&part, dst)?;
    Ok(())
}

pub struct KokoroSpeaker {
    model_path: PathBuf,
    voices_path: PathBuf,
    /// Loaded lazily on first `speak` and cached (model load is ~1s).
    tts: Arc<AsyncMutex<Option<Arc<KokoroTts>>>>,
    voice: Mutex<String>,
    speed: Mutex<f32>,
    player: Arc<Mutex<Option<PlayerHolder>>>,
    active: Arc<AtomicBool>,
    /// Read-aloud progress × 1000 (so it fits an integer atomic).
    progress: Arc<AtomicU64>,
    counter: AtomicU64,
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
            counter: AtomicU64::new(0),
        }
    }
}

/// Split read-aloud text into sentence-ish chunks so a long selection can start
/// playing almost immediately. The FIRST chunk is kept short (and may break at a
/// comma) so audio starts fast; later chunks are larger to keep prosody natural
/// and cut per-chunk overhead.
fn split_for_tts(text: &str) -> Vec<String> {
    let text = text.trim();
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        let first = chunks.is_empty();
        let (min, cap) = if first { (12, 90) } else { (40, 240) };
        let boundary = matches!(ch, '.' | '!' | '?' | '\n' | ';' | ':') || (first && ch == ',');
        // Flush at a boundary once past `min`, or hard-cap the length (byte len
        // is a fine proxy for the char cap).
        if (boundary && cur.trim_end().len() >= min) || cur.len() >= cap {
            let t = cur.trim().to_string();
            if !t.is_empty() {
                chunks.push(t);
            }
            cur.clear();
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        chunks.push(t);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

/// Synthesize one chunk to a 16-bit WAV buffer; `None` on synth error.
async fn synth_chunk_wav(tts: &KokoroTts, text: &str, voice: &str) -> Option<Vec<u8>> {
    match tts.synth(text, voice).await {
        Ok((samples, _)) => Some(pcm_f32_to_wav(&samples, KOKORO_SAMPLE_RATE)),
        Err(e) => {
            log::warn!("tts/kokoro: synth chunk failed: {e}");
            None
        }
    }
}

impl Speaker for KokoroSpeaker {
    fn speak(&self, text: &str) {
        let text = text.to_string();
        let voice = Arc::new(self.voice.lock().expect("voice mutex").clone());
        let speed = *self.speed.lock().expect("speed mutex");
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let model_path = self.model_path.clone();
        let voices_path = self.voices_path.clone();
        let tts_cell = self.tts.clone();
        let player_slot = self.player.clone();
        let active = self.active.clone();
        let progress = self.progress.clone();
        // Mark active immediately so is_speaking() is true during synthesis.
        active.store(true, Ordering::Release);
        progress.store(0, Ordering::Release);

        tauri::async_runtime::spawn(async move {
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

            // Stream sentence-by-sentence: play each chunk as it's ready, and
            // synthesize the *next* chunk while the current one plays — so a long
            // selection starts speaking in ~1s and the seams stay gap-free
            // instead of waiting for the whole text to synthesize.
            let chunks = split_for_tts(&text);
            log::info!(
                "tts/kokoro: reading {} chars in {} chunk(s)",
                text.len(),
                chunks.len()
            );
            // Char-weighted progress, smoothed within each chunk by elapsed
            // playback time so the overlay fill moves continuously, not in jumps.
            let total_chars = chunks
                .iter()
                .map(|c| c.chars().count())
                .sum::<usize>()
                .max(1) as f32;
            let mut chars_before = 0f32;
            let mut pending = synth_chunk_wav(&tts, &chunks[0], voice.as_str()).await;

            for i in 0..chunks.len() {
                if !active.load(Ordering::Acquire) {
                    break;
                }
                let wav = match pending.take() {
                    Some(w) => w,
                    None => break, // synth failed
                };
                let chunk_chars = chunks[i].chars().count() as f32;
                // Audio seconds for this chunk (24 kHz, 16-bit mono → 2 bytes/sample).
                let chunk_secs =
                    (wav.len().saturating_sub(44) / 2) as f32 / KOKORO_SAMPLE_RATE as f32;
                // Kick off the next chunk's synthesis while this one plays.
                let next_task = if i + 1 < chunks.len() {
                    let (t, c, v) = (tts.clone(), chunks[i + 1].clone(), voice.clone());
                    Some(tauri::async_runtime::spawn(async move {
                        synth_chunk_wav(&t, &c, v.as_str()).await
                    }))
                } else {
                    None
                };

                let temp = std::env::temp_dir().join(format!("murmur-kokoro-{n}-{i}.wav"));
                if std::fs::write(&temp, &wav).is_err() {
                    active.store(false, Ordering::Release);
                    break;
                }
                match spawn_avplayer(temp, speed) {
                    Some(h) => *player_slot.lock().expect("player mutex") = Some(h),
                    None => {
                        active.store(false, Ordering::Release);
                        break;
                    }
                }

                // Play out this chunk, advancing progress by elapsed time (scaled
                // by playback speed), until it ends or a stop() lands.
                let started = std::time::Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    if !active.load(Ordering::Acquire) {
                        break;
                    }
                    let frac = if chunk_secs > 0.0 {
                        (started.elapsed().as_secs_f32() * speed / chunk_secs).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let p = ((chars_before + chunk_chars * frac) / total_chars * 1000.0) as u64;
                    progress.store(p.min(1000), Ordering::Release);

                    let finished = {
                        let g = player_slot.lock().expect("player mutex");
                        match g.as_ref() {
                            None => true,
                            // SAFETY: live AVPlayer held under the mutex; `rate`
                            // returns a float (0.0 once the clip ends).
                            Some(h) => unsafe {
                                let rate: f32 = msg_send![h.player.as_ptr(), rate];
                                rate == 0.0
                            },
                        }
                    };
                    if finished {
                        break;
                    }
                }

                chars_before += chunk_chars;
                progress.store(
                    ((chars_before / total_chars * 1000.0) as u64).min(1000),
                    Ordering::Release,
                );

                pending = match next_task {
                    Some(t) => t.await.ok().flatten(),
                    None => None,
                };
            }
            active.store(false, Ordering::Release);
        });
    }

    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        *self.player.lock().expect("player mutex") = None;
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
            // SAFETY: `h.player` is a live AVPlayer held under the mutex;
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
        if !voice_id.is_empty() {
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
        assert_eq!(
            chunks.join(" ").split_whitespace().count(),
            text.split_whitespace().count()
        );
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_for_tts("Hi there."), vec!["Hi there.".to_string()]);
        assert_eq!(
            split_for_tts("no punctuation just a few words"),
            vec!["no punctuation just a few words".to_string()]
        );
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
