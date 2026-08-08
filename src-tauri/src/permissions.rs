// macOS permission checks + System Settings deep-links, used by the first-run
// onboarding flow. All read-only status probes here are safe to call from any
// thread. Triggering the microphone *prompt* is a capture attempt and lives in
// `audio::probe_microphone`.

use objc2::{class, msg_send};
use objc2_foundation::NSString;

// AXIsProcessTrusted reports whether we hold the Accessibility grant — the one
// permission Murmur needs (it also authorizes the Fn CGEventTap). See
// `fn_key.rs` for why Input Monitoring is deliberately never requested.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// True once the user has granted Accessibility. Note: a grant made while the
/// app is running only enables the Fn tap after a relaunch (the tap installs at
/// startup), so onboarding offers a relaunch button after this flips true.
pub fn accessibility_granted() -> bool {
    // SAFETY: `AXIsProcessTrusted` takes no arguments and returns a BOOL; it is
    // documented as callable from any thread.
    unsafe { AXIsProcessTrusted() }
}

/// Microphone authorization, mirroring `AVAuthorizationStatus`:
/// 0 = notDetermined, 1 = restricted, 2 = denied, 3 = authorized.
pub fn microphone_status() -> i64 {
    // AVMediaTypeAudio's documented raw value is "soun".
    // SAFETY: `authorizationStatusForMediaType:` is a class method on
    // AVCaptureDevice taking an NSString media-type and returning an NSInteger.
    unsafe {
        let media_type = NSString::from_str("soun");
        let cls = class!(AVCaptureDevice);
        let status: i64 = msg_send![cls, authorizationStatusForMediaType: &*media_type];
        status
    }
}

/// Open a System Settings privacy pane via its `x-apple.systempreferences:`
/// URL. We shell out to `open` (LaunchServices) rather than link NSWorkspace —
/// it's simpler and equally reliable for a URL.
fn open_settings_url(url: &str) {
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        log::warn!("permissions: open '{url}' failed: {e}");
    }
}

/// Deep-link to Privacy & Security → Accessibility.
pub fn open_accessibility_settings() {
    open_settings_url(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    );
}

/// Deep-link to Privacy & Security → Microphone.
pub fn open_microphone_settings() {
    open_settings_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
}
