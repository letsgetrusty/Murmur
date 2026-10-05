// Phase 3: read-aloud.
//
// Two on-device backends behind a shared `Speaker` trait, chosen at startup by
// `tts_provider` in config. The mac backend uses AVSpeechSynthesizer (free,
// offline, but the default voice is rough). The Kokoro backend (`tts_kokoro.rs`,
// absent on Intel Macs — see build.rs) runs a local neural model (ONNX via `ort`)
// and plays synthesized WAV chunks through an AVQueuePlayer with
// `audioTimePitchAlgorithm = .spectral` — pitch-preserving speed changes without
// a resampling chipmunk effect.

use std::ptr::NonNull;
use std::sync::Mutex;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

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

// Kokoro needs ONNX Runtime, which has no prebuilt x86_64-macOS binaries, so it
// only exists under `cfg(kokoro)`. Intel builds get the stubs below: no Kokoro
// voices, nothing to download, and `lib.rs` never constructs a `KokoroSpeaker`.
#[cfg(kokoro)]
#[path = "tts_kokoro.rs"]
mod kokoro_backend;
#[cfg(kokoro)]
pub use kokoro_backend::{
    ensure_kokoro_assets, install_g2p_lexicon, kokoro_assets_present, kokoro_model_path,
    kokoro_voices_dir, pin_coreml_compute_units, preview_text, KokoroSpeaker, KOKORO_VOICES,
};

#[cfg(not(kokoro))]
mod no_kokoro {
    use anyhow::{bail, Result};
    use std::path::PathBuf;

    pub const KOKORO_VOICES: &[(&str, &str)] = &[];

    pub fn kokoro_assets_present() -> bool {
        false
    }
    pub fn kokoro_model_path() -> Result<PathBuf> {
        bail!("Kokoro is not available on Intel Macs")
    }
    pub fn kokoro_voices_dir() -> Result<PathBuf> {
        bail!("Kokoro is not available on Intel Macs")
    }
    pub async fn ensure_kokoro_assets(_on_progress: impl Fn(u64, u64)) -> Result<()> {
        bail!("Kokoro is not available on Intel Macs")
    }
    pub fn install_g2p_lexicon() {}
    pub fn pin_coreml_compute_units() {}
    pub fn preview_text(friendly_name: &str) -> String {
        friendly_name.to_string()
    }
}
#[cfg(not(kokoro))]
pub use no_kokoro::*;

/// Voice choices to show in the tray + settings pickers for a given TTS
/// provider, so the picker matches the backend that will actually speak.
/// Native (AVSpeechSynthesizer) has no in-app voice selection, so it's empty.
pub fn voices_for(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        "kokoro" => KOKORO_VOICES,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voices_for_matches_provider() {
        assert_eq!(voices_for("native"), &[] as &[(&str, &str)]);
        assert_eq!(voices_for("kokoro"), KOKORO_VOICES);
    }
}
