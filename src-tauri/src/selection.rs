// Phase 3: capture the user's selected text by synthesizing Cmd+C and reading
// the clipboard. Mirror of `inject` — same change-count watch, same don't-touch-
// binary policy, same main-thread-only requirement on enigo. The selection
// capture intentionally restores the clipboard so the user's existing copy
// isn't clobbered.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use objc2::rc::Retained;
use objc2_app_kit::NSPasteboard;

/// Returns `Some(text)` if the user had something selected when triggered,
/// `None` if Cmd+C produced no clipboard write within the wait window.
/// Must be called on the main thread (enigo's CGEventPost requires a
/// CFRunLoop).
pub fn capture_selection() -> Result<Option<String>> {
    let pb: Retained<NSPasteboard> = NSPasteboard::generalPasteboard();
    let count_before = pb.changeCount();

    let mut clipboard = Clipboard::new().context("open clipboard")?;
    let saved_text = clipboard.get_text().ok();

    send_cmd_c().context("synthesize Cmd+C")?;

    // Wait for the focused app to write the selection to the pasteboard.
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut copied = false;
    while Instant::now() < deadline {
        if pb.changeCount() != count_before {
            copied = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let selected = if copied {
        clipboard.get_text().ok().filter(|s| !s.is_empty())
    } else {
        None
    };

    // Restore the original text. Best-effort — if the user had something
    // non-text on the clipboard we don't replace it (matches inject policy).
    if let Some(prev) = saved_text {
        if let Err(e) = clipboard.set_text(prev) {
            log::warn!("selection: clipboard restore failed: {e}");
        }
    }

    Ok(selected)
}

/// The current clipboard text, if any. Read-aloud falls back to this when
/// there's no live selection — e.g. a mouse-capturing terminal TUI where you
/// copied with the app's own command (Claude Code's `/copy`, etc.).
/// `capture_selection` leaves the clipboard untouched when it finds no
/// selection, so this returns whatever the user last put there.
pub fn clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    clipboard.get_text().ok().filter(|s| !s.is_empty())
}

fn send_cmd_c() -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| anyhow!("init enigo (Accessibility permission?): {e}"))?;
    enigo
        .key(Key::Meta, Direction::Press)
        .map_err(|e| anyhow!("press Cmd: {e}"))?;
    enigo
        .key(Key::Unicode('c'), Direction::Click)
        .map_err(|e| anyhow!("click C: {e}"))?;
    enigo
        .key(Key::Meta, Direction::Release)
        .map_err(|e| anyhow!("release Cmd: {e}"))?;
    Ok(())
}
