// macOS permission checks + System Settings deep-links, used by the first-run
// onboarding flow. All read-only status probes here are safe to call from any
// thread. Triggering the microphone *prompt* (`request_microphone_access`) uses
// AVCaptureDevice and blocks on the user's response, so it must run off the main
// thread.

use block2::RcBlock;
use objc2::runtime::Bool;
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

/// Trigger the macOS microphone-permission prompt via the sanctioned
/// `AVCaptureDevice.requestAccessForMediaType:completionHandler:` API and block
/// until the user answers, returning the resulting `AVAuthorizationStatus`
/// (see `microphone_status`). Unlike opening a capture stream, this reliably
/// raises the TCC dialog under the hardened runtime, and — because it awaits the
/// completion handler — returns the *real* decision instead of "not determined".
///
/// If access is already decided, the handler fires immediately with the current
/// grant. A 2-minute timeout guards against a prompt the user never answers so
/// the calling (blocking) thread can't wedge. **Must not run on the main thread**
/// — it blocks; the onboarding command dispatches it to a blocking pool thread.
pub fn request_microphone_access() -> i64 {
    // Already authorized/denied/restricted: no prompt needed, report as-is.
    let current = microphone_status();
    if current != 0 {
        log::info!("mic: request — already decided (status {current}), no prompt");
        return current;
    }
    log::info!("mic: requesting access (raising TCC prompt)…");

    let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
    // The completion handler is invoked once, on an arbitrary dispatch queue,
    // after the user responds. `tx` (SyncSender) is Send+Sync, so signaling from
    // that thread is safe; a send after our timeout just errors harmlessly.
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });

    // SAFETY: `requestAccessForMediaType:completionHandler:` is a class method on
    // AVCaptureDevice taking an NSString media type and a `void(^)(BOOL)` block.
    // The callee copies the block, so our `RcBlock` may drop when this returns.
    unsafe {
        let media_type = NSString::from_str("soun"); // AVMediaTypeAudio
        let cls = class!(AVCaptureDevice);
        let _: () = msg_send![
            cls,
            requestAccessForMediaType: &*media_type,
            completionHandler: &*handler,
        ];
    }

    // Wait for the user's decision (or give up after the timeout); then read the
    // authoritative status either way.
    let _ = rx.recv_timeout(std::time::Duration::from_secs(120));
    let result = microphone_status();
    log::info!("mic: request resolved (status {result})");
    result
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
