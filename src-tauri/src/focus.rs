// focus.rs — locate the *active window's* screen so the overlay pill appears
// where the user is typing, not where the mouse happens to sit.
//
// The frontmost window belongs to another app, so Tauri can't see it. We ask
// the macOS Accessibility API — already authorized, since enigo's paste needs
// the same grant (CLAUDE.md hard rule #5) — for the system-wide focused
// window's global frame, and return its center in top-left screen *points*.
// Those points line up with Tauri's per-monitor logical coordinates
// (physical ÷ scale), so the caller can match it to a monitor scale-safely.

use std::ffi::c_void;

use objc2_foundation::NSString;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

type CFTypeRef = *const c_void;
type AXUIElementRef = CFTypeRef;
type AXError = i32;

const KAX_ERROR_SUCCESS: AXError = 0;
// AXValueType tags (Foundation `AXValue.h`).
const KAXVALUE_CGPOINT_TYPE: u32 = 1;
const KAXVALUE_CGSIZE_TYPE: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFTypeRef, // CFStringRef; we pass a toll-free-bridged NSString
        value: *mut CFTypeRef,
    ) -> AXError;
    // Returns a `Boolean` (0/1); take it as u8 to stay sound.
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

/// Copy an AX attribute by name. Returns a +1-retained CF value the caller must
/// `CFRelease`, or None if the element lacks the attribute.
unsafe fn copy_attr(el: AXUIElementRef, name: &str) -> Option<CFTypeRef> {
    if el.is_null() {
        return None;
    }
    let attr = NSString::from_str(name);
    let mut out: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(el, (&*attr as *const NSString).cast(), &mut out);
    if err == KAX_ERROR_SUCCESS && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

/// Center of the focused window in global top-left screen points, or None if
/// there's no focused window (e.g. desktop/Finder has focus) or Accessibility
/// declines to answer. The caller falls back to the cursor's screen.
pub fn focused_window_center() -> Option<(f64, f64)> {
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        let result = (|| {
            let app = copy_attr(system, "AXFocusedApplicationAttribute")?;
            let win = copy_attr(app, "AXFocusedWindow");
            CFRelease(app);
            let win = win?;

            let mut point = CGPoint { x: 0.0, y: 0.0 };
            let mut size = CGSize {
                width: 0.0,
                height: 0.0,
            };
            let got_pos = copy_attr(win, "AXPosition")
                .map(|v| {
                    let ok = AXValueGetValue(
                        v,
                        KAXVALUE_CGPOINT_TYPE,
                        (&mut point as *mut CGPoint).cast(),
                    );
                    CFRelease(v);
                    ok != 0
                })
                .unwrap_or(false);
            let got_size = copy_attr(win, "AXSize")
                .map(|v| {
                    let ok =
                        AXValueGetValue(v, KAXVALUE_CGSIZE_TYPE, (&mut size as *mut CGSize).cast());
                    CFRelease(v);
                    ok != 0
                })
                .unwrap_or(false);
            CFRelease(win);

            (got_pos && got_size)
                .then(|| (point.x + size.width / 2.0, point.y + size.height / 2.0))
        })();
        CFRelease(system);
        result
    }
}
