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

use anyhow::{anyhow, Result};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

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
unsafe impl Send for SynthHolder {}

impl Drop for SynthHolder {
    fn drop(&mut self) {
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
            unsafe {
                let _: () = msg_send![prev.0.as_ptr(), stopSpeakingAtBoundary: 0i64];
            }
        }
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
            unsafe {
                let _: () = msg_send![prev.0.as_ptr(), stopSpeakingAtBoundary: 0i64];
            }
        }
    }

    fn is_speaking(&self) -> bool {
        let g = self.synth.lock().expect("speaker mutex");
        match g.as_ref() {
            None => false,
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
unsafe impl Send for PlayerHolder {}

impl Drop for PlayerHolder {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), pause];
            let _: () = msg_send![self.player.as_ptr(), release];
        }
        let _ = std::fs::remove_file(&self.temp_path);
    }
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
            log::info!("tts/11labs: AVPlayer rate={speed}");

            unsafe {
                let path_str = NSString::from_str(temp_path.to_str().unwrap_or_default());
                let nsurl: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*path_str];
                // `playerWithURL:` returns an autoreleased instance. Retain
                // so we can hold onto it across thread boundaries.
                let player: *mut AnyObject = msg_send![class!(AVPlayer), playerWithURL: nsurl];
                if player.is_null() {
                    log::warn!("tts/11labs: AVPlayer playerWithURL returned null");
                    let _ = std::fs::remove_file(&temp_path);
                    active.store(false, Ordering::Release);
                    return;
                }
                let _: () = msg_send![player, retain];

                // Pitch-preserving rate scaling lives on the player item.
                let item: *mut AnyObject = msg_send![player, currentItem];
                if !item.is_null() {
                    let algo = NSString::from_str(PITCH_ALG);
                    let _: () = msg_send![item, setAudioTimePitchAlgorithm: &*algo];
                }

                // Setting rate to a non-zero value starts playback.
                let _: () = msg_send![player, setRate: speed];

                let ptr = NonNull::new(player).expect("just checked non-null above");
                let holder = PlayerHolder {
                    player: ptr,
                    temp_path,
                };
                // Replacing drops the previous holder, which pauses+releases
                // the prior player and removes its temp file.
                let mut g = player_slot.lock().expect("player mutex");
                *g = Some(holder);
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
            let i = SPEEDS.iter().position(|s| (*s - *g).abs() < 1e-3).unwrap_or(0);
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
