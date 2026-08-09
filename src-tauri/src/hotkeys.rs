use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use crate::config::Config;
use crate::{
    emit_state, show_overlay, tts_speed_cycle, tts_toggle, AppState, DictationCmd, DictationMode,
    OverlayState,
};

/// The configurable global-shortcut chords. (Fn-hold dictation is a hardware
/// event tap in `fn_key`, not a plugin shortcut, so it isn't here.)
#[derive(Clone, Copy)]
pub enum HotkeyAction {
    Dictate,
    TtsToggle,
    TtsSpeed,
}

impl HotkeyAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dictate" => Some(Self::Dictate),
            "tts_toggle" => Some(Self::TtsToggle),
            "tts_speed" => Some(Self::TtsSpeed),
            _ => None,
        }
    }
}

/// Register a single action's handler under `shortcut`. The handler differs per
/// action but the registration plumbing is shared.
fn register_action<R: Runtime>(
    app: &AppHandle<R>,
    action: HotkeyAction,
    shortcut: &str,
) -> anyhow::Result<()> {
    let sc: Shortcut = shortcut
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid shortcut '{shortcut}': {e}"))?;
    let gs = app.global_shortcut();
    match action {
        HotkeyAction::Dictate => gs.on_shortcut(
            sc,
            move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| match event.state() {
                // The chord is always plain dictation; refined is Fn+Ctrl.
                ShortcutState::Pressed => on_press(app),
                ShortcutState::Released => on_release(app, DictationMode::Plain),
            },
        )?,
        HotkeyAction::TtsToggle => gs.on_shortcut(
            sc,
            move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
                if event.state() == ShortcutState::Pressed {
                    tts_toggle(app);
                }
            },
        )?,
        HotkeyAction::TtsSpeed => gs.on_shortcut(
            sc,
            move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
                if event.state() == ShortcutState::Pressed {
                    tts_speed_cycle(app);
                }
            },
        )?,
    }
    Ok(())
}

/// Register the chords from config. A bad user-supplied binding is logged and
/// skipped rather than aborting the others.
pub fn register<R: Runtime>(app: &AppHandle<R>, cfg: &Config) -> anyhow::Result<()> {
    for (action, sc) in [
        (HotkeyAction::Dictate, &cfg.hotkey_dictate),
        (HotkeyAction::TtsToggle, &cfg.hotkey_tts),
        (HotkeyAction::TtsSpeed, &cfg.hotkey_tts_speed),
    ] {
        if let Err(e) = register_action(app, action, sc) {
            log::warn!("hotkey: register '{sc}' failed: {e}");
        }
    }
    Ok(())
}

/// Chords Murmur must never register globally: they'd shadow core macOS editing
/// or the app's own synthesized Cmd+V / Cmd+C (binding one there swallows the
/// paste, so dictation silently stops working). Guards against the settings
/// recorder capturing a stray combo — including the app's own paste keystroke.
pub fn is_reserved_shortcut(sc: &str) -> bool {
    const RESERVED: &[&str] = &[
        "Cmd+V",
        "Cmd+C",
        "Cmd+X",
        "Cmd+A",
        "Cmd+Z",
        "Cmd+Q",
        "Cmd+W",
        "CmdOrCtrl+V",
        "CmdOrCtrl+C",
        "CmdOrCtrl+X",
        "CmdOrCtrl+A",
        "CmdOrCtrl+Z",
        "CmdOrCtrl+Q",
        "CmdOrCtrl+W",
    ];
    RESERVED.contains(&sc)
}

/// Swap an action's chord live: validate the new one, unregister the old, then
/// register the new — restoring the old if the new registration fails.
pub fn rebind<R: Runtime>(
    app: &AppHandle<R>,
    action: HotkeyAction,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    if is_reserved_shortcut(new) {
        return Err(anyhow::anyhow!(
            "'{new}' is reserved by macOS/Murmur — pick another combo"
        ));
    }
    // Validate before we touch the live registration.
    let _: Shortcut = new
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid shortcut '{new}': {e}"))?;
    if let Ok(old_sc) = old.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(old_sc);
    }
    if let Err(e) = register_action(app, action, new) {
        let _ = register_action(app, action, old);
        return Err(e);
    }
    log::info!("hotkey: rebound to '{new}'");
    Ok(())
}

/// Begin a dictation. Safe to call from any thread that can resolve
/// `AppHandle`; called by both the chord callback (this module) and the
/// Fn-key event tap (`fn_key`).
pub fn on_press<R: Runtime>(app: &AppHandle<R>) {
    // If the speech model is still downloading, show its progress instead of
    // recording audio we couldn't transcribe. `on_release` is a no-op because
    // `recording_armed` stays false.
    if !crate::stt_model_ready(app) {
        crate::begin_model_wait(app);
        return;
    }
    log::info!("hotkey: press");
    // Load the model now (in the background) so it's resident by the time the
    // user releases — overlapping the load with the seconds they're speaking.
    // No-op once loaded.
    app.state::<AppState>().transcriber.warm();
    app.state::<AppState>()
        .recording_armed
        .store(true, std::sync::atomic::Ordering::Release);
    show_overlay(app);
    emit_state(app, OverlayState::Recording);
    register_escape(app);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Start) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

/// Commit a dictation. Keep the overlay visible — the router flips it to
/// Done/Error and schedules the idle render once transcribe + inject return.
/// `mode` selects plain / refined / command handling of the transcript.
pub fn on_release<R: Runtime>(app: &AppHandle<R>, mode: DictationMode) {
    // Always release the Esc hijack (harmless if it was never registered, e.g.
    // the model-download gate). Only commit a transcription if this press
    // actually started a recording — skipping it when the press hit the gate or
    // after an Esc-cancel, so we don't strand the overlay on "Transcribing…".
    unregister_escape(app);
    if !app
        .state::<AppState>()
        .recording_armed
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    log::info!("hotkey: release (mode={mode:?})");
    emit_state(app, OverlayState::Transcribing);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Stop { mode }) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

const ESC: &str = "Escape";

/// Briefly hijack the Escape key while a dictation is in flight so the user can
/// cancel it; freed on release (or when the app goes idle).
///
/// `on_press`/`on_release` can run *inside* a global-shortcut callback (the chord
/// path), where the plugin holds its dispatch lock — touching the registry there
/// deadlocks the whole app. So both (un)register hop through a background thread
/// whose `run_on_main_thread` defers the work to a clean event-loop tick with the
/// lock free (same reason as [`register_tts_escape`]). From the Fn tap thread the
/// hop is simply correct too (registry ops belong on the main thread). A plain
/// `run_on_main_thread` here would NOT be enough: called from the main thread it
/// runs synchronously and would still re-enter under the held lock.
fn register_escape<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    std::thread::spawn(move || {
        let h = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            let Ok(esc) = ESC.parse::<Shortcut>() else {
                return;
            };
            let res = h.global_shortcut().on_shortcut(
                esc,
                |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
                    if event.state() == ShortcutState::Pressed {
                        log::info!("hotkey: cancel (Esc)");
                        // Disarm so the trigger release doesn't re-emit
                        // Transcribing over the Idle the Cancel produces.
                        app.state::<AppState>()
                            .recording_armed
                            .store(false, std::sync::atomic::Ordering::Release);
                        // Don't unregister here — this runs inside the
                        // global-shortcut callback and would deadlock; on_release
                        // frees Esc when the user lets go.
                        if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Cancel) {
                            log::warn!("hotkey: cancel send failed: {e}");
                        }
                    }
                },
            );
            if let Err(e) = res {
                log::debug!("hotkey: esc register failed: {e}");
            }
        });
    });
}

fn unregister_escape<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    std::thread::spawn(move || {
        let h = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Ok(esc) = ESC.parse::<Shortcut>() {
                let _ = h.global_shortcut().unregister(esc);
            }
        });
    });
}

/// Hijack Escape while a read-aloud is playing so the user can stop it mid-read
/// (stops TTS rather than cancelling a dictation).
///
/// Call only from a background thread (the idle-watcher), never from inside a
/// global-shortcut callback: touching the global-shortcut registry while the
/// plugin holds its dispatch lock deadlocks the app. The `run_on_main_thread`
/// hop then defers the actual (un)register to a clean event-loop tick with that
/// lock free.
pub fn register_tts_escape<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Ok(esc) = ESC.parse::<Shortcut>() else {
            return;
        };
        let res = handle.global_shortcut().on_shortcut(
            esc,
            |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
                if event.state() == ShortcutState::Pressed {
                    log::info!("tts: stop read-aloud (Esc)");
                    // Just stop; the idle-watcher unregisters Esc once
                    // is_speaking() flips false. Do NOT unregister here — this
                    // runs inside the global-shortcut callback and would deadlock.
                    app.state::<AppState>().speaker.stop();
                    emit_state(app, OverlayState::Idle);
                }
            },
        );
        if let Err(e) = res {
            log::debug!("hotkey: tts esc register failed: {e}");
        }
    });
}

pub fn unregister_tts_escape<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Ok(esc) = ESC.parse::<Shortcut>() {
            let _ = handle.global_shortcut().unregister(esc);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::HotkeyAction;

    // These action strings are a contract: they're the `set_hotkey` command's
    // `action` arg and the frontend's `data-action` / HOTKEY_FIELD keys.
    #[test]
    fn parse_maps_known_actions() {
        assert!(matches!(
            HotkeyAction::parse("dictate"),
            Some(HotkeyAction::Dictate)
        ));
        assert!(matches!(
            HotkeyAction::parse("tts_toggle"),
            Some(HotkeyAction::TtsToggle)
        ));
        assert!(matches!(
            HotkeyAction::parse("tts_speed"),
            Some(HotkeyAction::TtsSpeed)
        ));
    }

    #[test]
    fn parse_rejects_unknown_or_miscased() {
        assert!(HotkeyAction::parse("").is_none());
        assert!(HotkeyAction::parse("command").is_none());
        assert!(HotkeyAction::parse("Dictate").is_none());
    }

    #[test]
    fn reserved_shortcuts_are_blocked() {
        // The app synthesizes Cmd+V / Cmd+C; binding a global chord to them would
        // swallow paste/selection and break dictation.
        assert!(super::is_reserved_shortcut("Cmd+V"));
        assert!(super::is_reserved_shortcut("Cmd+C"));
        assert!(super::is_reserved_shortcut("CmdOrCtrl+V"));
        // Real defaults must stay allowed.
        assert!(!super::is_reserved_shortcut("CmdOrCtrl+Shift+R"));
        assert!(!super::is_reserved_shortcut("CmdOrCtrl+Shift+D"));
        assert!(!super::is_reserved_shortcut("Cmd+Ctrl+S"));
    }
}
