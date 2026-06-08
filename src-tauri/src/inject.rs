// Phase 1: text injection. Sets the clipboard, simulates Cmd+V, then restores
// the previous text. The restore is gated on NSPasteboard's `changeCount` —
// per CLAUDE.md hard rule #2, never a fixed timer.
//
// We intentionally do not snapshot binary clipboard contents. Clipboard
// managers like Paste/Alfred routinely re-snapshot pasteboard changes; if we
// were to restore an image or file URL we'd race them and cause double-pastes.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use objc2::rc::Retained;
use objc2_app_kit::NSPasteboard;

/// Paste text at the cursor by way of the clipboard.
pub fn paste_text(text: &str) -> Result<()> {
    if text.is_empty() {
        log::info!("inject: empty text, skipping");
        return Ok(());
    }

    let mut clipboard = Clipboard::new().context("open clipboard")?;
    // Best-effort: only preserve text contents. See module comment.
    let saved_text = clipboard.get_text().ok();

    let pb: Retained<NSPasteboard> = NSPasteboard::generalPasteboard();
    let count_before = pb.changeCount();

    clipboard.set_text(text).context("set clipboard text")?;
    let count_ours = pb.changeCount();

    send_cmd_v().context("synthesize Cmd+V")?;

    // Wait for the destination app to consume the pasteboard before restoring.
    // changeCount changes ONLY on writes — paste is a read, so the count stays
    // at `count_ours` while the paste is in flight. If a clipboard manager (or
    // any other app) writes during the window, the count moves and we bail out:
    // their copy wins, we don't clobber it.
    let deadline = Instant::now() + Duration::from_millis(180);
    while Instant::now() < deadline {
        let now = pb.changeCount();
        if now != count_ours {
            log::info!(
                "inject: pasteboard changed during paste window (was {count_ours}, now {now}); skipping restore"
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Restore previous text contents if we had any.
    if let Some(prev) = saved_text {
        if let Err(e) = clipboard.set_text(prev) {
            log::warn!("inject: clipboard restore failed: {e}");
        }
    } else {
        log::debug!("inject: no previous text to restore");
    }

    let _ = count_before; // kept for log/debug context if we want it later
    Ok(())
}

fn send_cmd_v() -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| anyhow!("init enigo (Accessibility permission?): {e}"))?;
    enigo
        .key(Key::Meta, Direction::Press)
        .map_err(|e| anyhow!("press Cmd: {e}"))?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| anyhow!("click V: {e}"))?;
    enigo
        .key(Key::Meta, Direction::Release)
        .map_err(|e| anyhow!("release Cmd: {e}"))?;
    Ok(())
}
