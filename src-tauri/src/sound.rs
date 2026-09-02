// Subtle audio cues for dictation start/stop, so the user gets non-visual
// confirmation the mic is live (like Wispr Flow).
//
// Played through **AVAudioPlayer**, not `AudioServicesPlaySystemSound`. The
// system-sound alert API is fire-and-forget: it hands the sound to coreaudiod
// and, when the default output device has idled to sleep, the sound is simply
// dropped rather than waited on — which showed up as "the first couple of
// dictation start cues after idle are silent, then it works". An AVAudioPlayer
// is a real audio client: play() acquires and wakes the output device and
// renders the buffer, so the cue plays even from a cold device.
//
// Each cue builds a **fresh** player at play time (see `Cues`), not one reused
// from startup: on Bluetooth (AirPods) the output route changes underneath a
// long-lived player and silently kills it. Two hard-won gotchas, both from the
// A2DP→SCO switch AirPods make when the mic opens:
//   * The START cue is played by the caller BEFORE opening the mic — once the
//     switch begins, A2DP output is torn down and any cue played after it is
//     inaudible; playing first sends it out the still-live A2DP link.
//   * A cue is retained in a slot after play() so Drop's `stop` can't cut it off
//     mid-blip.
// Playback is gated on the `dictation_sound` config flag by the caller.

use std::ptr::NonNull;
use std::sync::Mutex;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

// AVAudioPlayer lives in AVFoundation (also linked from tts.rs; declared here
// too so this module is self-contained).
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

// Built-in macOS sounds (always present in /System/Library/Sounds). "Pop" reads
// as a soft rising blip for start, "Bottle" as a lower one for stop. Swap the
// filenames to taste — loading no-ops gracefully if a file is missing.
const START_SOUND: &str = "/System/Library/Sounds/Pop.aiff";
const STOP_SOUND: &str = "/System/Library/Sounds/Bottle.aiff";

/// A retained AVAudioPlayer for one cue, released on drop.
struct Player(NonNull<AnyObject>);

// SAFETY: a Player is only messaged from the dictation worker thread, one cue
// at a time (start/stop are distinct objects, never played concurrently), and
// AVAudioPlayer's play/stop/currentTime methods are thread-safe (the
// main-thread rule is UI only) — the same basis as tts.rs's AVQueuePlayer.
unsafe impl Send for Player {}
// SAFETY: as above — access is serialized to the worker thread, and no `&Player`
// is ever shared for concurrent messaging, so Sync holds vacuously.
unsafe impl Sync for Player {}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a retained AVAudioPlayer; `stop`/`release` are
        // valid selectors and `release` balances the alloc/init exactly once.
        unsafe {
            let _: () = msg_send![self.0.as_ptr(), stop];
            let _: () = msg_send![self.0.as_ptr(), release];
        }
    }
}

/// Plays the dictation start/stop cues.
///
/// A **fresh** `AVAudioPlayer` is built at each play, not reused from startup.
/// Reusing one preloaded player fails silently on Bluetooth: opening the mic
/// switches AirPods from A2DP to the SCO/HFP voice profile, and that route
/// change invalidates a player built under the old route — `play()` still
/// returns true and reports the right output device, but renders into a dead
/// audio graph, so the cue is inaudible during dictation (yet audible in
/// isolation, where no route change happens). Building the player after the
/// route settles attaches it to the live output. The just-played player is
/// retained in a slot so it isn't dropped (which would `stop` it) mid-cue.
#[derive(Default)]
pub struct Cues {
    start_live: Mutex<Option<Player>>,
    stop_live: Mutex<Option<Player>>,
}

impl Cues {
    /// No-op constructor kept for the `AppState` call site; players are built
    /// lazily at play time now (see the struct doc).
    pub fn load() -> Self {
        Cues::default()
    }

    pub fn play_start(&self) {
        play_fresh(START_SOUND, &self.start_live);
    }

    pub fn play_stop(&self) {
        play_fresh(STOP_SOUND, &self.stop_live);
    }
}

/// Build a fresh player for `path`, play it, and park it in `slot` so it stays
/// alive for the duration (dropping the previous occupant, which stops any
/// still-ringing prior cue — harmless, they're short).
fn play_fresh(path: &str, slot: &Mutex<Option<Player>>) {
    let Some(p) = make_player(path) else {
        log::warn!("sound: play() skipped — cue {path} failed to load");
        return;
    };
    // SAFETY: `p.0` is a freshly-built, live retained AVAudioPlayer.
    unsafe {
        let ok: bool = msg_send![p.0.as_ptr(), play];
        if !ok {
            log::warn!("sound: play() returned false for cue {path}");
        }
    }
    // Retain until the next cue replaces it, so Drop's `stop` doesn't cut it off.
    if let Ok(mut g) = slot.lock() {
        *g = Some(p);
    }
}

/// Load `path` into a retained AVAudioPlayer, or `None` if it can't be created.
fn make_player(path: &str) -> Option<Player> {
    // SAFETY: `fileURLWithPath:` returns an autoreleased NSURL; `alloc` +
    // `initWithContentsOfURL:error:` returns a +1-owned AVAudioPlayer (or nil,
    // which also consumes the alloc). We pass a null NSError** (the error isn't
    // inspected) and null-check the result before wrapping it.
    unsafe {
        let ns_path = NSString::from_str(path);
        let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*ns_path];
        if url.is_null() {
            return None;
        }
        let alloc: *mut AnyObject = msg_send![class!(AVAudioPlayer), alloc];
        let err: *mut *mut AnyObject = std::ptr::null_mut();
        let player: *mut AnyObject = msg_send![alloc, initWithContentsOfURL: url, error: err];
        match NonNull::new(player) {
            Some(p) => {
                // Preload buffers so the first cue doesn't pay the lazy
                // hardware-acquire lag on its first `play()`.
                let _: bool = msg_send![p.as_ptr(), prepareToPlay];
                Some(Player(p))
            }
            None => {
                log::warn!("sound: failed to load cue {path}");
                None
            }
        }
    }
}
