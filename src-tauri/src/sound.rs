// Subtle audio cues for dictation start/stop, so the user gets non-visual
// confirmation the mic is live (like Wispr Flow). Played via AudioServices
// system sounds: fire-and-forget and thread-safe, and a `SystemSoundID` is a
// plain u32 that lives happily in the shared `AppState` — no `!Send` objc
// object to juggle. We register two built-in macOS sounds up front; playback is
// gated on the `dictation_sound` config flag by the caller.

use std::ffi::c_void;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

type SystemSoundID = u32;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioServicesCreateSystemSoundID(
        in_file_url: *const c_void,
        out_sound_id: *mut SystemSoundID,
    ) -> i32;
    fn AudioServicesPlaySystemSound(in_sound_id: SystemSoundID);
}

// Built-in macOS sounds (always present in /System/Library/Sounds). "Pop" reads
// as a soft rising blip for start, "Bottle" as a lower one for stop. Swap the
// filenames to taste — the registration no-ops gracefully if a file is missing.
const START_SOUND: &str = "/System/Library/Sounds/Pop.aiff";
const STOP_SOUND: &str = "/System/Library/Sounds/Bottle.aiff";

/// Registered system-sound ids for the start/stop cues. `Copy` + `Send`/`Sync`
/// (just two u32s), so it drops into `AppState` with no locking.
#[derive(Clone, Copy, Default)]
pub struct Cues {
    start: Option<SystemSoundID>,
    stop: Option<SystemSoundID>,
}

impl Cues {
    /// Register the start/stop sounds. A missing/unreadable file yields `None`,
    /// so that cue silently no-ops rather than failing.
    pub fn load() -> Self {
        Cues {
            start: register(START_SOUND),
            stop: register(STOP_SOUND),
        }
    }

    pub fn play_start(&self) {
        play(self.start);
    }

    pub fn play_stop(&self) {
        play(self.stop);
    }
}

fn play(id: Option<SystemSoundID>) {
    if let Some(id) = id {
        // SAFETY: `id` came from a successful AudioServicesCreateSystemSoundID.
        // AudioServicesPlaySystemSound is fire-and-forget and thread-safe.
        unsafe { AudioServicesPlaySystemSound(id) };
    }
}

fn register(path: &str) -> Option<SystemSoundID> {
    // SAFETY: `fileURLWithPath:` returns an autoreleased NSURL (toll-free bridged
    // to the CFURLRef the C API wants); AudioServices reads it synchronously
    // during the create call, so the autorelease lifetime is sufficient. A
    // non-zero OSStatus means failure → None.
    unsafe {
        let ns_path = NSString::from_str(path);
        let url: *const AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*ns_path];
        if url.is_null() {
            return None;
        }
        let mut id: SystemSoundID = 0;
        let status = AudioServicesCreateSystemSoundID(url as *const c_void, &mut id);
        if status == 0 {
            Some(id)
        } else {
            log::warn!("sound: failed to register cue {path} (OSStatus {status})");
            None
        }
    }
}
