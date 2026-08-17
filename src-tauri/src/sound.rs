// Subtle audio cues for dictation start/stop, so the user gets non-visual
// confirmation the mic is live (like Wispr Flow).
//
// Played through **AVAudioPlayer**, not `AudioServicesPlaySystemSound`. The
// system-sound alert API is fire-and-forget: it hands the sound to coreaudiod
// and, when the default output device has idled to sleep, the sound is simply
// dropped rather than waited on — which showed up as "the first couple of
// dictation start cues after idle are silent, then it works". An AVAudioPlayer
// is a real audio client: play() acquires and wakes the output device and
// renders the buffer, so the cue plays even from a cold device. An idle,
// not-yet-played player holds no hardware, so this keeps idle cost at zero.
//
// The players are retained ObjC objects (not the old plain `u32` sound ids), so
// `Cues` owns them behind a Send/Sync wrapper — mirroring tts.rs's KokoroQueue.
// Playback is gated on the `dictation_sound` config flag by the caller.

use std::ptr::NonNull;

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

/// Preloaded players for the start/stop cues. Owns two retained ObjC objects, so
/// it is not `Copy` — it lives by value in `AppState` and is only borrowed.
#[derive(Default)]
pub struct Cues {
    start: Option<Player>,
    stop: Option<Player>,
}

impl Cues {
    /// Load the start/stop sounds. A missing/unreadable file yields `None`, so
    /// that cue silently no-ops rather than failing.
    pub fn load() -> Self {
        Cues {
            start: make_player(START_SOUND),
            stop: make_player(STOP_SOUND),
        }
    }

    pub fn play_start(&self) {
        play(self.start.as_ref());
    }

    pub fn play_stop(&self) {
        play(self.stop.as_ref());
    }
}

fn play(player: Option<&Player>) {
    if let Some(p) = player {
        // SAFETY: `p.0` is a live retained AVAudioPlayer. Rewind to the start so
        // a repeated cue replays from the top, then play — play() wakes/acquires
        // the output device and renders the buffer, so it is not dropped when
        // the device is cold.
        unsafe {
            let player = p.0.as_ptr();
            let _: () = msg_send![player, setCurrentTime: 0.0f64];
            let _: bool = msg_send![player, play];
        }
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
            Some(p) => Some(Player(p)),
            None => {
                log::warn!("sound: failed to load cue {path}");
                None
            }
        }
    }
}
