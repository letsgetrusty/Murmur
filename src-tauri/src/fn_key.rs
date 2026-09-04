// Hold-to-talk dictation trigger (default: the Fn key).
//
// `tauri-plugin-global-shortcut` can't register the Fn key or a bare modifier
// held on its own — macOS delivers those only as modifier-flag changes, not key
// events. We listen for `kCGEventFlagsChanged` via Quartz Event Services, check
// the bit for the configured trigger (`config.dictation_trigger`), and dispatch
// edges into `hotkeys::on_press` / `hotkeys::on_release`. The trigger is the Fn
// key, a right-side modifier (a dedicated key for keyboards without Fn), or a
// plain modifier; it's read live so a change in Settings applies without a
// restart.
//
// The tap is installed onto the main thread's CFRunLoop, which is the same
// run loop NSApp drives. Callbacks therefore arrive on the main thread, just
// like the chord callbacks.
//
// Permissions: this tap needs event-listen authorization, which on macOS is
// satisfied by the **Accessibility** grant Murmur already requires for
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
// Generic (side-independent) modifier flag masks; one of these (per
// config.refine_modifier) held with the trigger refines the dictation. These are
// also what a *plain* modifier trigger tests against.
const KCG_FLAG_SHIFT: u64 = 0x0002_0000;
const KCG_FLAG_CONTROL: u64 = 0x0004_0000;
const KCG_FLAG_ALT: u64 = 0x0008_0000;
const KCG_FLAG_COMMAND: u64 = 0x0010_0000;
// Device-dependent (left/right) modifier masks from IOKit's IOLLEvent.h, present
// in CGEventGetFlags for keyboard events. A *right-side* modifier can thus be a
// dedicated hold-to-talk trigger that doesn't also fire on the left-hand
// modifiers people use for ordinary shortcuts.
const KCG_DEVICE_RIGHT_CONTROL: u64 = 0x0000_2000;
const KCG_DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
const KCG_DEVICE_RIGHT_ALT: u64 = 0x0000_0040;

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

/// The hold-to-talk dictation mode: refined if the configured modifier was held
/// at any point during the hold (the latch), else plain.
fn dictation_mode(refine_held: bool) -> crate::DictationMode {
    if refine_held {
        crate::DictationMode::Refine
    } else {
        crate::DictationMode::Plain
    }
}

/// The CGEvent flag bit that indicates the configured trigger is held. "Fn" → the
/// secondaryFn bit; a right-side modifier → its device bit; a plain modifier → the
/// generic (either-side) bit. Unknown → Fn (the default).
fn trigger_mask(trigger: &str) -> u64 {
    match trigger {
        "RightCtrl" => KCG_DEVICE_RIGHT_CONTROL,
        "RightCmd" | "RightCommand" => KCG_DEVICE_RIGHT_COMMAND,
        "RightAlt" | "RightOption" => KCG_DEVICE_RIGHT_ALT,
        "Ctrl" | "Control" => KCG_FLAG_CONTROL,
        "Alt" | "Option" => KCG_FLAG_ALT,
        "Cmd" | "Command" | "Super" => KCG_FLAG_COMMAND,
        _ => KCG_EVENT_FLAG_MASK_SECONDARY_FN, // "Fn" and anything unknown
    }
}

/// The generic modifier family a trigger belongs to (0 for Fn). Holding a
/// modifier trigger also sets its generic bit, so the refine latch uses this to
/// avoid a self-collision (e.g. trigger=Right Control would otherwise make a
/// Control refine modifier look permanently held).
fn trigger_generic_mask(trigger: &str) -> u64 {
    match trigger {
        "RightCtrl" | "Ctrl" | "Control" => KCG_FLAG_CONTROL,
        "RightCmd" | "RightCommand" | "Cmd" | "Command" | "Super" => KCG_FLAG_COMMAND,
        "RightAlt" | "RightOption" | "Alt" | "Option" => KCG_FLAG_ALT,
        _ => 0, // Fn occupies no modifier
    }
}

/// The configured trigger name, read live from shared config (defaults to Fn).
fn trigger_from_config<R: Runtime>(app: &AppHandle<R>) -> String {
    use tauri::Manager;
    app.try_state::<crate::AppState>()
        .and_then(|s| s.config.lock().ok().map(|c| c.dictation_trigger.clone()))
        .unwrap_or_else(|| crate::config::DEFAULT_DICTATION_TRIGGER.to_string())
}

/// The flag mask for the configured refine modifier (defaults to Control), read
/// live so a settings change applies without a restart. Returns 0 (refine
/// disabled) when the modifier is the same key family as the trigger, so holding
/// the trigger can't be misread as "refine held".
fn refine_mask<R: Runtime>(app: &AppHandle<R>) -> u64 {
    use tauri::Manager;
    let (modifier, trigger) = app
        .try_state::<crate::AppState>()
        .and_then(|s| {
            s.config
                .lock()
                .ok()
                .map(|c| (c.refine_modifier.clone(), c.dictation_trigger.clone()))
        })
        .unwrap_or_else(|| ("Ctrl".to_string(), "Fn".to_string()));
    let mask = modifier_mask(&modifier);
    if mask == trigger_generic_mask(&trigger) {
        0
    } else {
        mask
    }
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
    trigger_down: AtomicBool,
    /// Whether the configured refine modifier was held at any point during the
    /// current hold. Seeded at trigger-down and OR'd on every flags change while
    /// the trigger is down, so it refines regardless of which key is pressed first.
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
    // Read the trigger live so a Settings change applies without a restart.
    let trigger_down_now = (flags & trigger_mask(&trigger_from_config(&state.app))) != 0;
    let was_down = state.trigger_down.swap(trigger_down_now, Ordering::AcqRel);
    if trigger_down_now != was_down {
        if trigger_down_now {
            // Trigger just went down: seed the refine latch with the current state
            // of the configured refine modifier (handles modifier-then-trigger).
            let mask = refine_mask(&state.app);
            state
                .refine_latch
                .store((flags & mask) != 0, Ordering::Release);
            hotkeys::on_press(&state.app);
        } else {
            // Trigger released: refine if the modifier was held at any point.
            let mode = dictation_mode(state.refine_latch.load(Ordering::Acquire));
            // The Fn trigger is always the primary language; the alternate one
            // has its own chord.
            let lang = hotkeys::language_for_action(&state.app, hotkeys::HotkeyAction::Dictate);
            hotkeys::on_release(&state.app, mode, lang);
        }
    } else if trigger_down_now && (flags & refine_mask(&state.app)) != 0 {
        // Modifier pressed while the trigger is already held (handles
        // trigger-then-modifier), so the order of the two keys doesn't matter.
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
/// Whether the Fn CGEventTap is installed AND enabled. The tap installs at
/// startup gated on Accessibility; if the grant lands later (during onboarding),
/// [`try_activate`] re-runs [`install`] so Fn goes live without a relaunch. Set
/// once and left set — the tap is leaked for the process lifetime, never torn
/// down.
static TAP_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether we've already created a tap object this process. Guards against
/// leaking a second tap when a first came up "born disabled" after a fresh grant
/// — that case needs a relaunch, not another tap.
static TAP_CREATED: AtomicBool = AtomicBool::new(false);

/// Is the Fn tap installed and live? False when Accessibility wasn't granted at
/// install time (or the rare born-disabled case). Onboarding reads this to
/// decide whether Fn can be tested now or the app must relaunch first.
pub fn is_active() -> bool {
    TAP_ACTIVE.load(Ordering::Acquire)
}

/// Try to bring the Fn tap live now — for when Accessibility is granted
/// mid-session (onboarding), so Fn activates without a relaunch. No-op once the
/// tap is active or a born-disabled attempt already happened. The tap attaches to
/// the main thread's run loop, so installation hops there.
pub fn try_activate<R: Runtime>(app: &AppHandle<R>) {
    if TAP_ACTIVE.load(Ordering::Acquire) || TAP_CREATED.load(Ordering::Acquire) {
        return;
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = install(handle);
    });
}

pub fn install<R: Runtime>(app: AppHandle<R>) -> anyhow::Result<()> {
    // Idempotent: once the tap is live, re-invocations (via `try_activate` when
    // Accessibility is granted mid-session) are no-ops.
    if TAP_ACTIVE.load(Ordering::Acquire) {
        return Ok(());
    }

    // SAFETY: the CoreGraphics/CoreFoundation/ApplicationServices functions are
    // called with the signatures declared for the linked frameworks; `state` is
    // a valid `Box::into_raw` pointer used only as the tap's opaque `user_info`,
    // and the tap/source/state are deliberately leaked for the process lifetime.
    unsafe {
        // Preflight Input Monitoring. If it isn't granted, the tap below is
        // born disabled and never delivers events, so trigger the system
        // prompt (which also adds `murmur` to the Input Monitoring list). The
        // grant only takes effect on the next launch.
        let ax_trusted = crate::permissions::accessibility_granted();
        let access = IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT);
        log::info!("Fn-key: Accessibility(AXIsProcessTrusted)={ax_trusted}  InputMonitoring(IOHIDCheckAccess)={access} [0=granted,1=denied,2=unknown]");
        // Accessibility is the ONE permission Murmur needs: it's required for
        // paste-injection AND it authorizes the Fn CGEventTap (an app trusted
        // for Accessibility satisfies the event-listen check, so a separate
        // Input Monitoring grant is unnecessary). We deliberately do NOT call
        // IOHIDRequestAccess — it records an explicit Input Monitoring *denial*
        // that then overrides the Accessibility grant and wedges the tap off.
        if !ax_trusted {
            log::warn!(
                "Fn-key: Accessibility not granted — Fn-hold disabled. It installs live once you grant Murmur in System Settings → Privacy & Security → Accessibility (or on the next launch)."
            );
            return Ok(());
        }
        // A prior attempt already made a tap the system left disabled — that
        // needs a relaunch, not another (leaked) tap. Bail before allocating.
        if TAP_CREATED.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let state = Box::into_raw(Box::new(TapState {
            app,
            trigger_down: AtomicBool::new(false),
            refine_latch: AtomicBool::new(false),
        })) as *mut c_void;

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
                "Fn-key tap unavailable. Grant Input Monitoring in System Settings → Privacy & Security → Input Monitoring (add `murmur`), then restart. Cmd+Shift+D still works."
            );
            return Ok(());
        }
        let source = CFMachPortCreateRunLoopSource(ptr::null_mut(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        CGEventTapEnable(tap, true);
        if CGEventTapIsEnabled(tap) {
            TAP_ACTIVE.store(true, Ordering::Release);
            log::info!("Fn-key tap installed and enabled");
        } else {
            log::warn!(
                "Fn-key tap created but disabled by the system — Input Monitoring likely missing. Grant `murmur` and restart."
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

    #[test]
    fn trigger_mask_maps_each_accepted_trigger() {
        // Fn (default) and unknown → the secondaryFn bit.
        assert_eq!(trigger_mask("Fn"), KCG_EVENT_FLAG_MASK_SECONDARY_FN);
        assert_eq!(trigger_mask("bogus"), KCG_EVENT_FLAG_MASK_SECONDARY_FN);
        assert_eq!(
            trigger_mask(crate::config::DEFAULT_DICTATION_TRIGGER),
            KCG_EVENT_FLAG_MASK_SECONDARY_FN
        );
        // Right-side modifiers → their device bits (not the generic ones).
        assert_eq!(trigger_mask("RightCtrl"), KCG_DEVICE_RIGHT_CONTROL);
        assert_eq!(trigger_mask("RightAlt"), KCG_DEVICE_RIGHT_ALT);
        assert_eq!(trigger_mask("RightCmd"), KCG_DEVICE_RIGHT_COMMAND);
        // Plain modifiers → the generic (either-side) bits.
        assert_eq!(trigger_mask("Ctrl"), KCG_FLAG_CONTROL);
        assert_eq!(trigger_mask("Alt"), KCG_FLAG_ALT);
        assert_eq!(trigger_mask("Cmd"), KCG_FLAG_COMMAND);
    }

    // The refine latch is disabled (mask forced to 0) when the refine modifier is
    // the same key family as the trigger — otherwise holding e.g. Right Control
    // would set the generic Control bit and make Ctrl-refine fire on every hold.
    #[test]
    fn refine_collides_with_same_family_trigger() {
        assert_eq!(modifier_mask("Ctrl"), trigger_generic_mask("RightCtrl"));
        assert_eq!(modifier_mask("Ctrl"), trigger_generic_mask("Ctrl"));
        assert_eq!(modifier_mask("Cmd"), trigger_generic_mask("RightCmd"));
        // Fn occupies no modifier, so nothing collides with it.
        assert_eq!(trigger_generic_mask("Fn"), 0);
        assert_ne!(modifier_mask("Shift"), trigger_generic_mask("RightCtrl"));
    }
}
