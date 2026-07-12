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
// Requires **Input Monitoring** permission. If the user hasn't granted it,
// `CGEventTapCreate` returns null; we log a clear message and continue with
// the chord as the only trigger.

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
    fn IOHIDRequestAccess(request: u32) -> bool;
}
// kIOHIDRequestTypeListenEvent — the "monitor input" (Input Monitoring) grant.
const KIOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
// kIOHIDAccessTypeGranted
const KIOHID_ACCESS_TYPE_GRANTED: u32 = 0;

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
            hotkeys::on_press(&state.app);
        } else {
            hotkeys::on_release(&state.app);
        }
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
    })) as *mut c_void;

    unsafe {
        // Preflight Input Monitoring. If it isn't granted, the tap below is
        // born disabled and never delivers events, so trigger the system
        // prompt (which also adds `murmur` to the Input Monitoring list). The
        // grant only takes effect on the next launch.
        let access = IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT);
        if access != KIOHID_ACCESS_TYPE_GRANTED {
            log::warn!(
                "Fn-key: Input Monitoring not granted (access={access}). Prompting; grant `murmur` in System Settings → Privacy & Security → Input Monitoring, then restart. Fn-hold is disabled until then."
            );
            IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT);
            // Reclaim the state we allocated for the (now-pointless) tap and
            // bail — creating a dead tap only produces the misleading
            // "installed" log we're fixing.
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
                "Fn-key tap unavailable. Grant Input Monitoring in System Settings → Privacy & Security → Input Monitoring (add `murmur`), then restart. Cmd+Shift+Space still works."
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
                "Fn-key tap created but disabled by the system — Input Monitoring likely missing. Grant `murmur` and restart."
            );
        }
        // Intentionally leak the tap + source + state for the lifetime of the
        // app. macOS releases them on process exit.
    }
    Ok(())
}
