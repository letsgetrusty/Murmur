// Fn-key dictation trigger.
//
// `tauri-plugin-global-shortcut` can't register the literal Fn key — macOS
// delivers it only as a modifier-flag change, not a key event. We listen for
// `kCGEventFlagsChanged` via Quartz Event Services, check the `secondaryFn`
// bit, and dispatch edges into `hotkeys::on_press` / `hotkeys::on_release`.
//
// The tap is installed onto the main thread's CFRunLoop, which is the same
// run loop NSApp drives. Callbacks therefore arrive on the main thread, just
// like the chord callbacks.
//
// Permissions: this tap needs event-listen authorization, which on macOS is
// satisfied by the **Accessibility** grant Open Wispr already requires for
// paste-injection — so it needs NO separate Input Monitoring grant. We gate
// tap creation on `AXIsProcessTrusted()` and deliberately never call
// `IOHIDRequestAccess`: doing so records an explicit Input Monitoring *denial*
// that overrides the Accessibility coupling and wedges the tap off. (Verified
// on macOS 26; see docs/macos-signing-and-permissions.md.)

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Runtime};

use crate::hotkeys;

// CGEventTapLocation
const KCG_SESSION_EVENT_TAP: u32 = 1;
// CGEventTapPlacement
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
// CGEventTapOptions — kCGEventTapOptionListenOnly (no event mutation, less risk).
const KCG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
// kCGEventFlagsChanged = 12; the event mask is `1 << event_type`.
const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_MASK_FLAGS_CHANGED: u64 = 1 << KCG_EVENT_FLAGS_CHANGED;
// kCGEventFlagMaskSecondaryFn
const KCG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 0x00800000;
// Modifier flag masks; one of these (per config.refine_modifier) held with Fn
// triggers refined dictation.
const KCG_FLAG_SHIFT: u64 = 0x0002_0000;
const KCG_FLAG_CONTROL: u64 = 0x0004_0000;
const KCG_FLAG_ALT: u64 = 0x0008_0000;
const KCG_FLAG_COMMAND: u64 = 0x0010_0000;

/// Map a configured refine-modifier name to its CGEvent flag mask. The accepted
/// names mirror the settings-window `<select>` values and
/// `config::DEFAULT_REFINE_MODIFIER`; anything unknown falls back to Control.
fn modifier_mask(modifier: &str) -> u64 {
    match modifier {
        "Shift" => KCG_FLAG_SHIFT,
        "Alt" | "Option" => KCG_FLAG_ALT,
        "Cmd" | "Command" | "Super" => KCG_FLAG_COMMAND,
        _ => KCG_FLAG_CONTROL,
    }
}

/// The Fn-hold dictation mode: refined if the configured modifier was held at
/// any point during the hold (the latch), else plain.
fn dictation_mode(refine_held: bool) -> crate::DictationMode {
    if refine_held {
        crate::DictationMode::Refine
    } else {
        crate::DictationMode::Plain
    }
}

/// The flag mask for the configured refine modifier (defaults to Control).
/// Read from the shared config so a change in the settings window applies live.
fn refine_mask<R: Runtime>(app: &AppHandle<R>) -> u64 {
    use tauri::Manager;
    let modifier = app
        .try_state::<crate::AppState>()
        .and_then(|s| s.config.lock().ok().map(|c| c.refine_modifier.clone()))
        .unwrap_or_else(|| "Ctrl".to_string());
    modifier_mask(&modifier)
}

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFAllocatorRef = *mut c_void;
type CFStringRef = *mut c_void;

type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

// Accessibility trust. On macOS 26 an app trusted for Accessibility also
// satisfies Input Monitoring's IOHIDCheckAccess, so Accessibility is the one
// permission that matters. We log it to see the real state.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
}

// IOKit's HID access API is the only reliable way to check/prompt for Input
// Monitoring. `CGEventTapCreate` returns a non-null port even when the
// permission is missing — the tap is simply created disabled — so a null
// check alone silently degrades. We preflight here so the log is truthful and
// the user actually gets the system prompt.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
}
// kIOHIDRequestTypeListenEvent — the "monitor input" (Input Monitoring) grant.
// Only used for the diagnostic IOHIDCheckAccess log; the tap is gated on
// Accessibility, not this value.
const KIOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    static kCFRunLoopDefaultMode: CFStringRef;
}

struct TapState<R: Runtime> {
    app: AppHandle<R>,
    fn_down: AtomicBool,
    /// Whether the configured refine modifier was held at any point during the
    /// current Fn hold. Seeded at Fn-down and OR'd on every flags change while
    /// Fn is down, so it refines regardless of which key is pressed first.
    refine_latch: AtomicBool,
}

unsafe extern "C" fn tap_callback<R: Runtime>(
    _proxy: CGEventTapProxy,
    _event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = &*(user_info as *const TapState<R>);
    let flags = CGEventGetFlags(event);
    let fn_down_now = (flags & KCG_EVENT_FLAG_MASK_SECONDARY_FN) != 0;
    let was_down = state.fn_down.swap(fn_down_now, Ordering::AcqRel);
    if fn_down_now != was_down {
        if fn_down_now {
            // Fn just went down: seed the refine latch with the current state of
            // the configured refine modifier (handles modifier-then-Fn).
            let mask = refine_mask(&state.app);
            state
                .refine_latch
                .store((flags & mask) != 0, Ordering::Release);
            hotkeys::on_press(&state.app);
        } else {
            // Fn released: refine if the modifier was held at any point.
            let mode = dictation_mode(state.refine_latch.load(Ordering::Acquire));
            hotkeys::on_release(&state.app, mode);
        }
    } else if fn_down_now && (flags & refine_mask(&state.app)) != 0 {
        // Modifier pressed while Fn is already held (handles Fn-then-modifier),
        // so the order of the two keys doesn't matter.
        state.refine_latch.store(true, Ordering::Release);
    }
    event
}

/// Install the Fn-key tap onto the current thread's CFRunLoop. Must be called
/// on the main thread (Tauri's `setup` closure satisfies that).
///
/// Returns `Ok(())` on success and on permission-denied (we log and degrade
/// to chord-only). Returns `Err` only for ownership-leak conditions worth
/// failing setup over.
pub fn install<R: Runtime>(app: AppHandle<R>) -> anyhow::Result<()> {
    let state = Box::into_raw(Box::new(TapState {
        app,
        fn_down: AtomicBool::new(false),
        refine_latch: AtomicBool::new(false),
    })) as *mut c_void;

    // SAFETY: the CoreGraphics/CoreFoundation/ApplicationServices functions are
    // called with the signatures declared for the linked frameworks; `state` is
    // a valid `Box::into_raw` pointer used only as the tap's opaque `user_info`,
    // and the tap/source/state are deliberately leaked for the process lifetime.
    unsafe {
        // Preflight Input Monitoring. If it isn't granted, the tap below is
        // born disabled and never delivers events, so trigger the system
        // prompt (which also adds `openwispr` to the Input Monitoring list). The
        // grant only takes effect on the next launch.
        let ax_trusted = AXIsProcessTrusted();
        let access = IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT);
        log::info!("Fn-key: Accessibility(AXIsProcessTrusted)={ax_trusted}  InputMonitoring(IOHIDCheckAccess)={access} [0=granted,1=denied,2=unknown]");
        // Accessibility is the ONE permission Open Wispr needs: it's required for
        // paste-injection AND it authorizes the Fn CGEventTap (an app trusted
        // for Accessibility satisfies the event-listen check, so a separate
        // Input Monitoring grant is unnecessary). We deliberately do NOT call
        // IOHIDRequestAccess — it records an explicit Input Monitoring *denial*
        // that then overrides the Accessibility grant and wedges the tap off.
        if !ax_trusted {
            log::warn!(
                "Fn-key: Accessibility not granted — Fn-hold disabled. Grant Open Wispr in System Settings → Privacy & Security → Accessibility, then relaunch."
            );
            let _ = Box::from_raw(state as *mut TapState<R>);
            return Ok(());
        }

        let tap = CGEventTapCreate(
            KCG_SESSION_EVENT_TAP,
            KCG_HEAD_INSERT_EVENT_TAP,
            KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
            EVENT_MASK_FLAGS_CHANGED,
            tap_callback::<R>,
            state,
        );
        if tap.is_null() {
            // Reclaim the leaked state so we don't drop it on the floor.
            let _ = Box::from_raw(state as *mut TapState<R>);
            log::warn!(
                "Fn-key tap unavailable. Grant Input Monitoring in System Settings → Privacy & Security → Input Monitoring (add `openwispr`), then restart. Cmd+Shift+Space still works."
            );
            return Ok(());
        }
        let source = CFMachPortCreateRunLoopSource(ptr::null_mut(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        CGEventTapEnable(tap, true);
        if CGEventTapIsEnabled(tap) {
            log::info!("Fn-key tap installed and enabled");
        } else {
            log::warn!(
                "Fn-key tap created but disabled by the system — Input Monitoring likely missing. Grant `openwispr` and restart."
            );
        }
        // Intentionally leak the tap + source + state for the lifetime of the
        // app. macOS releases them on process exit.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DictationMode;

    #[test]
    fn modifier_mask_maps_each_configured_name() {
        assert_eq!(modifier_mask("Shift"), KCG_FLAG_SHIFT);
        assert_eq!(modifier_mask("Alt"), KCG_FLAG_ALT);
        assert_eq!(modifier_mask("Option"), KCG_FLAG_ALT);
        assert_eq!(modifier_mask("Cmd"), KCG_FLAG_COMMAND);
        assert_eq!(modifier_mask("Command"), KCG_FLAG_COMMAND);
        assert_eq!(modifier_mask("Super"), KCG_FLAG_COMMAND);
        assert_eq!(modifier_mask("Ctrl"), KCG_FLAG_CONTROL);
    }

    #[test]
    fn modifier_mask_unknown_falls_back_to_control() {
        assert_eq!(modifier_mask(""), KCG_FLAG_CONTROL);
        assert_eq!(modifier_mask("Meta"), KCG_FLAG_CONTROL);
    }

    // The config default must resolve to a real mask (Control), or Fn+default
    // would silently never refine.
    #[test]
    fn default_refine_modifier_maps_to_control() {
        assert_eq!(
            modifier_mask(crate::config::DEFAULT_REFINE_MODIFIER),
            KCG_FLAG_CONTROL
        );
    }

    #[test]
    fn dictation_mode_reflects_refine_latch() {
        assert_eq!(dictation_mode(true), DictationMode::Refine);
        assert_eq!(dictation_mode(false), DictationMode::Plain);
    }
}
