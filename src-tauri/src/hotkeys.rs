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
    Macro,
}

impl HotkeyAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dictate" => Some(Self::Dictate),
            "tts_toggle" => Some(Self::TtsToggle),
            "tts_speed" => Some(Self::TtsSpeed),
            "macro" => Some(Self::Macro),
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
        HotkeyAction::Macro => gs.on_shortcut(
            sc,
            move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| match event.state() {
                ShortcutState::Pressed => on_press(app),
                ShortcutState::Released => on_release(app, DictationMode::Macro),
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

/// Register all three chords from config. A bad user-supplied binding is logged
/// and skipped rather than aborting the others.
pub fn register<R: Runtime>(app: &AppHandle<R>, cfg: &Config) -> anyhow::Result<()> {
    for (action, sc) in [
        (HotkeyAction::Dictate, &cfg.hotkey_dictate),
        (HotkeyAction::TtsToggle, &cfg.hotkey_tts),
        (HotkeyAction::TtsSpeed, &cfg.hotkey_tts_speed),
        (HotkeyAction::Macro, &cfg.hotkey_macro),
    ] {
        if let Err(e) = register_action(app, action, sc) {
            log::warn!("hotkey: register '{sc}' failed: {e}");
        }
    }
    Ok(())
}

/// Swap an action's chord live: validate the new one, unregister the old, then
/// register the new — restoring the old if the new registration fails.
pub fn rebind<R: Runtime>(
    app: &AppHandle<R>,
    action: HotkeyAction,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
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
    log::info!("hotkey: press");
    show_overlay(app);
    emit_state(app, OverlayState::Recording);
    register_escape(app);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Start) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

/// Commit a dictation. Keep the overlay visible — the router flips it to
/// Done/Error and schedules the idle render once transcribe + inject return.
/// `mode` selects plain / refined / macro handling of the transcript.
pub fn on_release<R: Runtime>(app: &AppHandle<R>, mode: DictationMode) {
    log::info!("hotkey: release (mode={mode:?})");
    unregister_escape(app);
    emit_state(app, OverlayState::Transcribing);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Stop { mode }) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

const ESC: &str = "Escape";

/// Briefly hijack the Escape key while a dictation is in flight. The
/// shortcut is unregistered the moment the user releases the hotkey or
/// hits Esc, so we never block Esc system-wide outside of recording.
fn register_escape<R: Runtime>(app: &AppHandle<R>) {
    let Ok(esc) = ESC.parse::<Shortcut>() else {
        return;
    };
    let res = app.global_shortcut().on_shortcut(
        esc,
        |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
            if event.state() == ShortcutState::Pressed {
                log::info!("hotkey: cancel (Esc)");
                unregister_escape(app);
                if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Cancel) {
                    log::warn!("hotkey: cancel send failed: {e}");
                }
            }
        },
    );
    if let Err(e) = res {
        log::debug!("hotkey: esc register failed: {e}");
    }
}

fn unregister_escape<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(esc) = ESC.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(esc);
    }
}
