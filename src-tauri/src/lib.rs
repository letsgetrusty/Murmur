// Module layout per voice-tool-architecture.md §4. lib.rs is the action router:
// hotkey → recorder lifecycle → transcribe → inject. selection / tts / kb land
// in subsequent phases and remain stubs.
mod audio;
mod commands;
mod config;
mod fn_key;
mod hotkeys;
mod inject;
mod kb;
mod refine;
mod secrets;
mod selection;
mod stt;
mod tts;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tts::Speaker as _;

use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager, RunEvent, Runtime, Wry,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub enum DictationCmd {
    Start,
    /// `refine` = run the transcript through the LLM refiner before pasting
    /// (Fn+Ctrl). Plain dictation sets it false. Decided at release so Ctrl
    /// can be pressed before or after Fn.
    Stop { refine: bool },
    /// User pressed Esc — drop the in-flight recorder and don't transcribe.
    Cancel,
}

pub struct AppState {
    pub tx: UnboundedSender<DictationCmd>,
    pub speaker: Arc<dyn tts::Speaker>,
    /// Tray menu checkmarks for speed, in the same order as `tts::SPEEDS`.
    pub speed_items: Vec<CheckMenuItem<Wry>>,
    /// Tray menu checkmarks for voice, in the same order as `tts::ELEVENLABS_VOICES`.
    pub voice_items: Vec<CheckMenuItem<Wry>>,
    /// Tray menu checkmarks for the microphone picker. First entry is the
    /// system default; rest mirror `mic_names`.
    pub mic_items: Vec<CheckMenuItem<Wry>>,
    /// cpal device names parallel to `mic_items[1..]`. Index 0 of mic_items
    /// is "Default" (no name).
    pub mic_names: Vec<String>,
    /// Current mic selection. `None` = system default.
    pub mic_name: Mutex<Option<String>>,
    /// Live config, shared with the refiner and the settings-window IPC
    /// commands so edits apply without a restart.
    pub config: Arc<Mutex<config::Config>>,
}

/// State the overlay renders, emitted by the action router over the
/// `state` Tauri event. Kept narrow so the frontend doesn't need to know
/// about Rust types.
///
/// `Idle` means "render nothing"; the underlying webview window stays
/// `visible` to NSApp so AppKit doesn't terminate the process when there
/// are no on-screen windows. Hiding/showing is done in CSS, not native.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayState {
    Idle,
    Recording,
    Transcribing,
    /// Fn+Ctrl only: the transcript is being cleaned up by the LLM.
    Refining,
    Done { chars: usize },
    Error { message: String },
}

/// Tee log output to both stderr and a file. When the app is launched as a
/// bundle via `open` (the dev workflow), stderr is discarded — the file is the
/// only way to see logs. stderr stays useful for `cargo run` from a terminal.
struct Tee(std::fs::File);
impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stderr());
        self.0.flush()
    }
}

/// `~/Library/Logs/murmur.log`, truncated each launch so it stays readable.
fn open_log_file() -> Option<std::fs::File> {
    let mut path = std::path::PathBuf::from(std::env::var_os("HOME")?);
    path.push("Library/Logs/murmur.log");
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .ok()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(file) = open_log_file() {
        builder.target(env_logger::Target::Pipe(Box::new(Tee(file))));
    }
    builder.init();

    // First-run setup: `murmur set-key` stores the Groq API key in Keychain.
    // CLAUDE.md hard rule #6: secrets never live in config files or source.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set-key") {
        // `murmur set-key [groq|openrouter|elevenlabs]`; defaults to groq.
        let which = args.get(2).map(String::as_str).unwrap_or("groq");
        return run_set_key(which);
    }

    // Enumerate cpal input devices BEFORE Tauri/NSApp takes over the main
    // thread. Calling into CoreAudio HAL from inside the
    // NSApplicationDidFinishLaunching notification handler segfaults the
    // release build (HALDeviceList::GetData on a not-yet-ready audio
    // subsystem). Querying from the bare process at startup is reliable.
    let mic_names = audio::list_input_devices();
    let cfg = config::load();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::get_options,
            commands::set_speed,
            commands::set_voice,
            commands::set_mic,
            commands::set_hotkey,
        ])
        .setup(move |app| {

            // Shared live config: the refiner reads it on each refine and the
            // settings window edits it via IPC, so changes apply without a restart.
            let config_state = Arc::new(Mutex::new(cfg.clone()));

            // GroqWhisper reads the API key from Keychain on each transcribe,
            // so the user can run `set-key` without restarting the app.
            let transcriber: Arc<dyn stt::Transcriber> = Arc::new(stt::GroqWhisper::new());

            // Refiner for Fn+Ctrl dictation. Reads model + prompt from the shared
            // config; the key is read from Keychain lazily on first refine.
            let refiner: Arc<dyn refine::Refiner> =
                Arc::new(refine::OpenRouterRefiner::new(config_state.clone()));

            // The cpal Stream is !Send on macOS; keep it owned by a dedicated
            // worker thread so we never carry it across the global-shortcut
            // callback boundary.
            let tx = spawn_dictation_worker(app.handle().clone(), transcriber, refiner);

            // Prefer ElevenLabs when the user has set a key; fall back to
            // macOS AVSpeechSynthesizer otherwise so Option+A always does
            // *something*. Hydrate the speaker from saved config.
            let speaker: Arc<dyn tts::Speaker> = match secrets::get(secrets::ELEVENLABS_API_KEY) {
                Ok(_) => {
                    log::info!("tts: ElevenLabs backend (key present)");
                    let s = tts::ElevenLabsSpeaker::new();
                    s.set_speed(cfg.tts_speed);
                    s.set_voice(&cfg.tts_voice_id);
                    Arc::new(s)
                }
                Err(_) => {
                    log::info!("tts: macOS AVSpeechSynthesizer backend (no ElevenLabs key)");
                    Arc::new(tts::MacSpeaker::new())
                }
            };

            // Build the tray menu with CheckMenuItems initialised from the
            // saved config so the user sees their previous selection as soon
            // as they open the menu. `mic_names` was enumerated above before
            // Tauri took the main thread.
            let (menu, speed_items, voice_items, mic_items) =
                build_tray_menu(app.handle(), &cfg, &mic_names)?;
            app.manage(AppState {
                tx,
                speaker,
                speed_items,
                voice_items,
                mic_items,
                mic_names,
                mic_name: Mutex::new(cfg.mic_name.clone()),
                config: config_state.clone(),
            });

            // The tray icon itself is auto-created from the `trayIcon` block
            // in `tauri.conf.json`. Attaching the menu to that single
            // instance avoids the duplicate-slot bug we hit when building a
            // second TrayIcon in setup.
            let tray = app
                .tray_by_id("main")
                .ok_or_else(|| anyhow::anyhow!("tray 'main' not found — check tauri.conf.json"))?;
            tray.set_menu(Some(menu))?;
            tray.on_menu_event(handle_tray_event);

            // Pre-position the overlay at the bottom-center of the primary
            // monitor; keep hidden until the hotkey fires.
            if let Some(win) = app.get_webview_window("overlay") {
                let _ = win.set_always_on_top(true);
                let _ = win.set_skip_taskbar(true);
                // Show on every Space/desktop, so the pill follows the user
                // instead of staying on the Space where it was created.
                let _ = win.set_visible_on_all_workspaces(true);
                position_overlay_bottom(&win);
            }

            // The main (settings/history) window hides instead of closing, so
            // the tray/dock can re-show the same instance. It starts hidden and
            // is shown on demand via `show_main_window`.
            if let Some(main) = app.get_webview_window("main") {
                let main_for_close = main.clone();
                main.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                        let _ = main_for_close.hide();
                    }
                });
            }

            // Hotkey registration MUST happen on the main thread on macOS —
            // CLAUDE.md hard rule #1. `setup` runs on the main thread. Chords
            // come from config so the settings window can rebind them.
            hotkeys::register(app.handle(), &cfg)?;

            // Install the Fn-key tap onto the main thread's CFRunLoop (same
            // run loop NSApp drives). Failure here only means "no Fn yet" —
            // the chord still works.
            fn_key::install(app.handle().clone())?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // Keep the app alive when the overlay hides. Tauri's default is to
        // exit when the last visible window goes away — but our overlay is
        // a transient pill that's hidden most of the time, with the tray icon
        // as the persistent UI. Only exit when the tray Quit menu item calls
        // `app.exit(N)` (which surfaces here as `code = Some(N)`).
        .run(|app, event| match event {
            RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            // macOS: clicking the dock icon (with no visible windows) fires
            // Reopen — bring the settings window up.
            RunEvent::Reopen { .. } => show_main_window(app),
            _ => {}
        });
}

/// Show and focus the main settings/history window. Used by the dock-reopen
/// handler and the tray "Open Murmur…" item.
fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    match app.get_webview_window("main") {
        Some(win) => {
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
        None => log::warn!("main window not found"),
    }
}

pub fn emit_state<R: Runtime>(app: &AppHandle<R>, state: OverlayState) {
    if let Some(win) = app.get_webview_window("overlay") {
        if let Err(e) = win.emit("state", state) {
            log::debug!("emit overlay state failed: {e}");
        }
    }
}

/// Show the overlay window. After the first call, the window stays
/// `visible` for the rest of the session — we never call `hide()`,
/// because AppKit terminates the process when its last visible window
/// goes away (LSUIElement does not exempt us from that). Subsequent
/// "hides" are done by emitting `OverlayState::Idle`, which the
/// frontend renders as nothing.
/// Toggle TTS: if speaking, stop; otherwise capture the current selection
/// and read it aloud. Always called on the main thread — both the global
/// shortcut callback and the Fn tap dispatch into here from main, and both
/// `selection::capture_selection` and `tts::Speaker::speak` require it.
/// Cycle TTS playback speed. Currently 1.0 ↔ 2.0; tracked in the Speaker.
/// Routes through `apply_speed` so the tray checkmarks and the on-disk
/// config stay in lockstep with the hotkey.
pub fn tts_speed_cycle<R: Runtime>(app: &AppHandle<R>) {
    let next = app.state::<AppState>().speaker.cycle_speed();
    apply_speed(app, next);
}

pub fn tts_toggle<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    if state.speaker.is_speaking() {
        log::info!("tts: stop");
        state.speaker.stop();
        show_overlay(app);
        emit_state(app, OverlayState::Done { chars: 0 });
        idle_after(app.clone(), Duration::from_millis(400));
        return;
    }
    match selection::capture_selection() {
        Ok(Some(text)) => {
            let chars = text.chars().count();
            log::info!("tts: speak ({} chars)", chars);
            show_overlay(app);
            emit_state(app, OverlayState::Transcribing);
            state.speaker.speak(&text);
            spawn_tts_idle_watcher(app.clone());
        }
        Ok(None) => {
            log::info!("tts: nothing selected");
            show_overlay(app);
            emit_state(
                app,
                OverlayState::Error {
                    message: "no selection".into(),
                },
            );
            idle_after(app.clone(), Duration::from_millis(1600));
        }
        Err(e) => {
            log::warn!("tts: selection capture failed: {e}");
            show_overlay(app);
            emit_state(
                app,
                OverlayState::Error {
                    message: format!("selection failed: {e}"),
                },
            );
            idle_after(app.clone(), Duration::from_millis(2200));
        }
    }
}

pub fn show_overlay<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("overlay") {
        // Re-place the pill on the screen the user is currently on before
        // showing — otherwise it stays pinned to wherever it started.
        position_overlay_bottom(&win);
        let _ = win.set_always_on_top(true);
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    }
}

fn position_overlay_bottom<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    // Target the screen the user is actually working on: the monitor under the
    // mouse cursor. Fall back to the window's current monitor, then primary.
    // Wrap in early-returns so a single missing monitor query doesn't tank it.
    let monitor = win
        .cursor_position()
        .ok()
        .and_then(|p| win.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| win.current_monitor().ok().flatten())
        .or_else(|| win.primary_monitor().ok().flatten());
    let monitor = match monitor {
        Some(m) => m,
        None => {
            log::warn!("overlay: no monitor found; leaving position as-is");
            return;
        }
    };
    let mon_pos = monitor.position();
    let mon_size = monitor.size();
    let win_size = win
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(320, 80));
    let x = mon_pos.x + (mon_size.width as i32 - win_size.width as i32) / 2;
    // Sit ~20px above the screen edge so the pill is unmistakably at the
    // bottom. If the user has the Dock at the bottom and it overlaps, they
    // can move the Dock or hide it.
    let y = mon_pos.y + mon_size.height as i32 - win_size.height as i32 - 20;
    if let Err(e) = win.set_position(tauri::PhysicalPosition::new(x, y)) {
        log::warn!("overlay: set_position failed: {e}");
    }
}

/// Tell the overlay frontend to render nothing after a dwell. Uses a
/// std::thread so we don't block any tokio worker.
/// Poll the speaker after a `speak()` and emit `Idle` when playback ends —
/// either because the audio finished naturally or because the user pressed
/// Option+A again to stop. Without this the overlay would stay stuck on
/// "Transcribing…" forever.
fn spawn_tts_idle_watcher<R: Runtime>(app: AppHandle<R>) {
    std::thread::spawn(move || {
        let started_by = std::time::Instant::now() + Duration::from_secs(15);
        // Phase 1: wait for `is_speaking` to flip true (the AVPlayer has
        // started and is reading). Bail out if it never does — most likely
        // an API failure that already logged its own warning.
        loop {
            if app.state::<AppState>().speaker.is_speaking() {
                break;
            }
            if std::time::Instant::now() > started_by {
                emit_state(&app, OverlayState::Idle);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Phase 2: wait for it to flip false (end-of-media or stop()).
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if !app.state::<AppState>().speaker.is_speaking() {
                emit_state(&app, OverlayState::Idle);
                return;
            }
        }
    });
}

fn idle_after<R: Runtime>(app: AppHandle<R>, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        emit_state(&app, OverlayState::Idle);
    });
}

/// Build the tray menu. Returns the assembled menu plus parallel vectors of
/// the speed, voice, and microphone `CheckMenuItem`s so the click handler can
/// toggle their checked state when the user picks one.
fn build_tray_menu(
    app: &AppHandle<Wry>,
    cfg: &config::Config,
    mic_names: &[String],
) -> tauri::Result<(
    Menu<Wry>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
)> {
    let open_main = MenuItem::with_id(app, "open_main", "Open Murmur…", true, None::<&str>)?;
    let read = MenuItem::with_id(app, "tts_read", "Read selection (⌥A)", true, None::<&str>)?;
    let stop_read = MenuItem::with_id(app, "tts_stop", "Stop reading", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit murmur", true, None::<&str>)?;

    // Speed submenu — CheckMenuItems with the current selection pre-checked.
    let speed_items: Vec<CheckMenuItem<Wry>> = tts::SPEEDS
        .iter()
        .map(|&s| {
            let id = format!("speed_{s}");
            let label = format!("{s:.1}×");
            let checked = (s - cfg.tts_speed).abs() < 1e-3;
            CheckMenuItem::with_id(app, id, label, true, checked, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let speed_refs: Vec<&dyn IsMenuItem<Wry>> =
        speed_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
    let speed_menu = Submenu::with_id_and_items(app, "speed", "Speed", true, &speed_refs)?;

    // Voice submenu — same treatment.
    let voice_items: Vec<CheckMenuItem<Wry>> = tts::ELEVENLABS_VOICES
        .iter()
        .map(|(id, name)| {
            let item_id = format!("voice_{id}");
            let checked = *id == cfg.tts_voice_id;
            CheckMenuItem::with_id(app, item_id, *name, true, checked, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let voice_refs: Vec<&dyn IsMenuItem<Wry>> =
        voice_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
    let voice_menu = Submenu::with_id_and_items(app, "voice", "Voice", true, &voice_refs)?;

    // Microphone submenu — first entry is the system default, rest are
    // whatever cpal enumerated.
    let mut mic_items: Vec<CheckMenuItem<Wry>> = Vec::with_capacity(mic_names.len() + 1);
    mic_items.push(CheckMenuItem::with_id(
        app,
        "mic_default",
        "System default",
        true,
        cfg.mic_name.is_none(),
        None::<&str>,
    )?);
    for (i, name) in mic_names.iter().enumerate() {
        let id = format!("mic_{i}");
        let checked = cfg.mic_name.as_deref() == Some(name.as_str());
        mic_items.push(CheckMenuItem::with_id(
            app,
            id,
            name.as_str(),
            true,
            checked,
            None::<&str>,
        )?);
    }
    let mic_refs: Vec<&dyn IsMenuItem<Wry>> =
        mic_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
    let mic_menu = Submenu::with_id_and_items(app, "mic", "Microphone", true, &mic_refs)?;

    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;

    let sep0 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_main as &dyn IsMenuItem<Wry>,
            &sep0,
            &read,
            &stop_read,
            &sep1,
            &speed_menu,
            &voice_menu,
            &mic_menu,
            &sep2,
            &quit,
        ],
    )?;
    Ok((menu, speed_items, voice_items, mic_items))
}

fn handle_tray_event<R: Runtime>(app: &AppHandle<R>, event: tauri::menu::MenuEvent) {
    let id = event.id.as_ref();
    match id {
        "open_main" => show_main_window(app),
        "quit" => app.exit(0),
        "tts_read" => tts_toggle(app),
        "tts_stop" => {
            app.state::<AppState>().speaker.stop();
        }
        _ => {
            if let Some(s) = id.strip_prefix("speed_") {
                if let Ok(speed) = s.parse::<f32>() {
                    apply_speed(app, speed);
                }
            } else if let Some(v) = id.strip_prefix("voice_") {
                apply_voice(app, v);
            } else if id == "mic_default" {
                apply_mic(app, None);
            } else if let Some(idx_str) = id.strip_prefix("mic_") {
                if let Ok(idx) = idx_str.parse::<usize>() {
                    let name = app.state::<AppState>().mic_names.get(idx).cloned();
                    apply_mic(app, name);
                }
            }
        }
    }
}

/// Apply a speed selection: update the speaker, flip tray checkmarks so the
/// chosen one is the only check, and persist to disk.
pub(crate) fn apply_speed<R: Runtime>(app: &AppHandle<R>, speed: f32) {
    let state = app.state::<AppState>();
    state.speaker.set_speed(speed);
    for (item, &s) in state.speed_items.iter().zip(tts::SPEEDS.iter()) {
        let _ = item.set_checked((s - speed).abs() < 1e-3);
    }
    log::info!("tts: speed → {speed}x");
    persist_config(app);
}

/// Apply a voice selection: update the speaker, flip tray checkmarks, persist.
pub(crate) fn apply_voice<R: Runtime>(app: &AppHandle<R>, voice_id: &str) {
    let state = app.state::<AppState>();
    state.speaker.set_voice(voice_id);
    for (item, (id, _)) in state.voice_items.iter().zip(tts::ELEVENLABS_VOICES.iter()) {
        let _ = item.set_checked(*id == voice_id);
    }
    log::info!("tts: voice → {voice_id}");
    persist_config(app);
}

/// Apply a microphone selection: store the name, flip tray checkmarks (the
/// "System default" item is index 0), persist. The new device takes effect
/// on the next `DictationCmd::Start`.
pub(crate) fn apply_mic<R: Runtime>(app: &AppHandle<R>, name: Option<String>) {
    let state = app.state::<AppState>();
    *state.mic_name.lock().expect("mic name mutex") = name.clone();
    if let Some(item) = state.mic_items.first() {
        let _ = item.set_checked(name.is_none());
    }
    for (i, item) in state.mic_items.iter().skip(1).enumerate() {
        let matches = state
            .mic_names
            .get(i)
            .map(|n| Some(n.as_str()) == name.as_deref())
            .unwrap_or(false);
        let _ = item.set_checked(matches);
    }
    log::info!("audio: mic → {}", name.as_deref().unwrap_or("(default)"));
    persist_config(app);
}

fn persist_config<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    // Update the shared live config in place (preserving refine/history fields
    // the tray doesn't own), then write that snapshot to disk.
    let snapshot = {
        let Ok(mut cfg) = state.config.lock() else {
            log::warn!("config: lock poisoned; skipping save");
            return;
        };
        cfg.tts_speed = state.speaker.current_speed();
        cfg.tts_voice_id = state.speaker.current_voice().unwrap_or_default();
        cfg.mic_name = state.mic_name.lock().ok().and_then(|g| g.clone());
        cfg.clone()
    };
    if let Err(e) = config::save(&snapshot) {
        log::warn!("config: save failed: {e}");
    }
}

fn run_set_key(which: &str) {
    use std::io::{self, Write};
    let (key_name, label) = match which {
        "groq" => (secrets::GROQ_API_KEY, "Groq"),
        "openrouter" => (secrets::OPENROUTER_API_KEY, "OpenRouter"),
        "elevenlabs" => (secrets::ELEVENLABS_API_KEY, "ElevenLabs"),
        other => {
            eprintln!("unknown key '{other}'. Use: groq | openrouter | elevenlabs");
            return;
        }
    };
    eprint!("Enter {label} API key: ");
    let _ = io::stderr().flush();
    let mut key = String::new();
    if let Err(e) = io::stdin().read_line(&mut key) {
        eprintln!("read failed: {e}");
        return;
    }
    let key = key.trim();
    if key.is_empty() {
        eprintln!("empty key, aborting");
        return;
    }
    match secrets::set(key_name, key) {
        Ok(()) => eprintln!("{label} key saved to Keychain."),
        Err(e) => eprintln!("save failed: {e}"),
    }
}

fn spawn_dictation_worker<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    refiner: Arc<dyn refine::Refiner>,
) -> UnboundedSender<DictationCmd> {
    let (tx, mut rx) = unbounded_channel::<DictationCmd>();
    std::thread::spawn(move || {
        let mut rec: Option<audio::Recorder> = None;
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                DictationCmd::Start => {
                    if rec.is_some() {
                        log::debug!("dictation: already recording, ignoring Start");
                        continue;
                    }
                    let mic = app
                        .state::<AppState>()
                        .mic_name
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    // Forward audio levels to the overlay frontend.
                    let app_for_level = app.clone();
                    let on_level: audio::LevelFn = Box::new(move |level: f32| {
                        let _ = app_for_level.emit("audio:level", level);
                    });
                    match audio::Recorder::start(mic.as_deref(), Some(on_level)) {
                        Ok(r) => {
                            log::info!("dictation: recording started");
                            rec = Some(r);
                        }
                        Err(e) => {
                            log::warn!("dictation: failed to start recording: {e}");
                            emit_state(
                                &app,
                                OverlayState::Error {
                                    message: format!("mic error: {e}"),
                                },
                            );
                            idle_after(app.clone(), Duration::from_millis(2200));
                        }
                    }
                }
                DictationCmd::Cancel => {
                    if rec.take().is_some() {
                        log::info!("dictation: cancelled");
                    }
                    emit_state(&app, OverlayState::Idle);
                    continue;
                }
                DictationCmd::Stop { refine } => {
                    let Some(r) = rec.take() else {
                        log::debug!("dictation: Stop without active recording");
                        continue;
                    };
                    match r.stop() {
                        Ok(recording) => {
                            handle_recording(
                                app.clone(),
                                transcriber.clone(),
                                refiner.clone(),
                                recording,
                                refine,
                            );
                        }
                        Err(e) => {
                            log::warn!("dictation: stop failed: {e}");
                            emit_state(
                                &app,
                                OverlayState::Error {
                                    message: format!("audio stop failed: {e}"),
                                },
                            );
                            idle_after(app.clone(), Duration::from_millis(2000));
                        }
                    }
                }
            }
        }
        log::info!("dictation worker exiting");
    });
    tx
}

fn handle_recording<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    refiner: Arc<dyn refine::Refiner>,
    recording: audio::Recording,
    refine: bool,
) {
    if recording.duration_ms < 200 {
        log::info!(
            "dictation: discarded short clip ({} ms)",
            recording.duration_ms
        );
        emit_state(&app, OverlayState::Done { chars: 0 });
        idle_after(app, Duration::from_millis(250));
        return;
    }
    // CLAUDE.md hard rule #7: silent audio almost always means missing mic
    // permission. Surface it explicitly.
    if recording.mean_abs < 1e-4 {
        log::warn!(
            "dictation: clip is silent (mean|amp|={:.6}). Grant Microphone access in System Settings → Privacy & Security → Microphone, then try again.",
            recording.mean_abs
        );
        emit_state(
            &app,
            OverlayState::Error {
                message: "no mic input — grant Microphone access".into(),
            },
        );
        idle_after(app, Duration::from_millis(2500));
        return;
    }

    tauri::async_runtime::spawn(async move {
        let (state, dwell_ms) = match transcriber.transcribe(&recording.wav).await {
            Ok(text) if text.is_empty() => {
                log::info!("dictation: empty transcript");
                (OverlayState::Done { chars: 0 }, 400)
            }
            Ok(text) => {
                log::info!(
                    "dictation: transcript ({} chars): {}",
                    text.chars().count(),
                    text
                );
                // Fn+Ctrl: run the transcript through the LLM refiner before
                // pasting. On refine failure, fall back to the raw transcript
                // so a dropped API call never loses the user's dictation.
                let final_text = if refine {
                    emit_state(&app, OverlayState::Refining);
                    match refiner.refine(&text).await {
                        Ok(refined) => {
                            log::info!(
                                "dictation: refined ({} chars): {}",
                                refined.chars().count(),
                                refined
                            );
                            refined
                        }
                        Err(e) => {
                            log::warn!("dictation: refine failed, pasting raw transcript: {e}");
                            text
                        }
                    }
                } else {
                    text
                };
                let chars = final_text.chars().count();
                // `inject::paste_text` calls enigo, which posts CGEvents via
                // Quartz. CGEventPost requires a CFRunLoop on the calling
                // thread — tokio workers don't have one, and calling from a
                // bare worker exits the process silently. Run on the main
                // thread (NSApp's run loop) instead.
                let result = paste_on_main_thread(&app, final_text).await;
                match result {
                    Ok(()) => (OverlayState::Done { chars }, 600),
                    Err(e) => {
                        log::warn!("dictation: paste failed: {e}");
                        (
                            OverlayState::Error {
                                message: format!("paste failed: {e}"),
                            },
                            2500,
                        )
                    }
                }
            }
            Err(e) => {
                log::warn!("dictation: transcribe failed: {e}");
                (
                    OverlayState::Error {
                        message: e.to_string(),
                    },
                    3000,
                )
            }
        };
        emit_state(&app, state);
        idle_after(app, Duration::from_millis(dwell_ms));
    });
}

async fn paste_on_main_thread<R: Runtime>(app: &AppHandle<R>, text: String) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(inject::paste_text(&text));
    })
    .map_err(|e| anyhow::anyhow!("dispatch to main thread failed: {e}"))?;
    rx.await
        .map_err(|e| anyhow::anyhow!("paste task cancelled: {e}"))?
}
