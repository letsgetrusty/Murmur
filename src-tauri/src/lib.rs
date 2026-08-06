// Module layout per docs/voice-tool-architecture.md §4. lib.rs is the action router:
// hotkey → recorder lifecycle → transcribe → inject.
mod audio;
mod commands;
mod config;
mod fn_key;
mod focus;
mod history;
mod hotkeys;
mod inject;
mod llm;
mod local_llm;
mod secrets;
mod selection;
mod stt;
mod tts;
mod usage;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tts::Speaker as _;

use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewUrl, WebviewWindowBuilder, Wry,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// What to do with a committed recording. Decided at release (not press) so the
/// refine modifier can be pressed before or after Fn, and so the same recorder
/// lifecycle serves plain dictation, refined dictation, and voice commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationMode {
    /// Paste the transcript verbatim.
    Plain,
    /// Run the transcript through the LLM refiner, then paste (Fn+Ctrl).
    Refine,
    /// Classify the transcript into a command and paste its response (command
    /// chord).
    Command,
}

pub enum DictationCmd {
    Start,
    /// Commit the in-flight recording and process it per `mode`.
    Stop {
        mode: DictationMode,
    },
    /// User pressed Esc — drop the in-flight recorder and don't transcribe.
    Cancel,
}

pub struct AppState {
    pub tx: UnboundedSender<DictationCmd>,
    pub speaker: Arc<dyn tts::Speaker>,
    /// Tray menu checkmarks for speed, in the same order as `tts::SPEEDS`.
    pub speed_items: Vec<CheckMenuItem<Wry>>,
    /// Tray menu checkmarks for voice, in the same order as
    /// `tts::voices_for(tts_provider)` (the active backend's voice list).
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
    /// Cumulative refinement token/cost totals, shared with the refiner.
    pub usage: Arc<Mutex<usage::UsageStats>>,
    /// Dictation history database (SQLite). `Connection` is `!Sync`, so it's
    /// behind a Mutex; access is infrequent (once per dictation / UI action).
    pub history: Arc<Mutex<rusqlite::Connection>>,
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
    /// Command chord only: the spoken phrase is being matched to a command.
    Interpreting,
    /// Read-aloud in progress; `progress` is the fraction [0.0, 1.0] spoken,
    /// used to fill the overlay pill.
    Reading {
        progress: f32,
    },
    Done {
        chars: usize,
    },
    Error {
        message: String,
    },
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

/// `~/Library/Logs/openwispr.log`, truncated each launch so it stays readable.
fn open_log_file() -> Option<std::fs::File> {
    let mut path = std::path::PathBuf::from(std::env::var_os("HOME")?);
    path.push("Library/Logs/openwispr.log");
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

    // First-run setup: `openwispr set-key` stores the Groq API key in Keychain.
    // CLAUDE.md hard rule #6: secrets never live in config files or source.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set-key") {
        // `openwispr set-key [groq|openrouter|elevenlabs]`; defaults to groq.
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
            commands::set_refine_modifier,
            commands::list_keys,
            commands::reveal_key,
            commands::save_key,
            commands::delete_key,
            commands::get_usage,
            commands::reset_usage,
            commands::get_openrouter_spend,
            commands::list_history,
            commands::delete_history,
            commands::clear_history,
            commands::history_stats,
            commands::copy_text,
            commands::open_url,
            commands::relaunch_app,
        ])
        .setup(move |app| {
            // Shared live config: the refiner reads it on each refine and the
            // settings window edits it via IPC, so changes apply without a restart.
            let config_state = Arc::new(Mutex::new(cfg.clone()));

            // Speech-to-text backend, selected by config. Default is local
            // on-device Whisper (whisper-rs); "groq" uses the cloud endpoint,
            // reading its key from Keychain lazily so `set-key` needs no restart.
            let transcriber: Arc<dyn stt::Transcriber> = match cfg.stt_provider.as_str() {
                "groq" => {
                    log::info!("stt: Groq cloud Whisper backend");
                    Arc::new(stt::GroqWhisper::new())
                }
                _ => {
                    log::info!("stt: local Whisper backend (model '{}')", cfg.stt_model);
                    // Fetch the model in the background so the first dictation
                    // isn't blocked on a ~0.5 GB download.
                    let model = cfg.stt_model.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = stt::ensure_local_model(&model).await {
                            log::warn!("stt: local model prefetch failed: {e}");
                        }
                    });
                    Arc::new(stt::WhisperStt::new(cfg.stt_model.clone()))
                }
            };

            // Cumulative refinement usage, persisted across restarts.
            let usage_state = Arc::new(Mutex::new(usage::load()));

            // Dictation history database.
            let history_state = Arc::new(Mutex::new(history::open()));

            // LLM backend for Fn+Ctrl refinement + command classification,
            // selected by config. Default is a single embedded local model (Qwen3
            // via llama.cpp) shared by both; "openrouter" uses the cloud (key read
            // from Keychain lazily). Prompts/commands come from the shared config.
            // One chat seam drives both refinement (transform) and voice commands
            // (classify). "local" shares the embedded llama.cpp engine (loaded on
            // first use); "openrouter" is the cloud backend (key read lazily).
            let chat: Arc<dyn llm::LlmChat> = if cfg.llm_provider == "local" {
                if local_llm::assets_present(&cfg.llm_model) {
                    log::info!("llm: local backend (model '{}')", cfg.llm_model);
                } else {
                    log::info!("llm: local backend; model missing — downloading…");
                }
                // Download the model in the background so the first refine/command
                // isn't blocked on a ~1 GB fetch; it loads on first use.
                let model = cfg.llm_model.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = local_llm::ensure_local_llm(&model).await {
                        log::warn!("llm: model prefetch failed: {e}");
                    }
                });
                let engine = Arc::new(local_llm::LocalLlm::new(
                    local_llm::model_path(&cfg.llm_model).unwrap_or_default(),
                ));
                Arc::new(llm::LocalChat::new(engine))
            } else {
                log::info!("llm: OpenRouter cloud backend");
                Arc::new(llm::OpenRouterChat::new())
            };

            // The cpal Stream is !Send on macOS; keep it owned by a dedicated
            // worker thread so we never carry it across the global-shortcut
            // callback boundary.
            let tx = spawn_dictation_worker(app.handle().clone(), transcriber, chat);

            // Text-to-speech backend, selected by config. Default is the native
            // on-device AVSpeechSynthesizer; "kokoro" is local neural TTS;
            // "elevenlabs" is cloud (falls back to native without a key).
            let speaker: Arc<dyn tts::Speaker> = match cfg.tts_provider.as_str() {
                "elevenlabs" if secrets::get(secrets::ELEVENLABS_API_KEY).is_ok() => {
                    log::info!("tts: ElevenLabs backend");
                    let s = tts::ElevenLabsSpeaker::new();
                    s.set_speed(cfg.tts_speed);
                    s.set_voice(&cfg.tts_voice_id);
                    Arc::new(s)
                }
                "kokoro" => {
                    if tts::kokoro_assets_present() {
                        log::info!("tts: Kokoro local neural backend");
                    } else {
                        log::info!("tts: Kokoro backend; assets missing — downloading…");
                    }
                    // Fetch model + voices in the background so the first
                    // read-aloud isn't blocked on a ~310 MB download.
                    tauri::async_runtime::spawn(async {
                        if let Err(e) = tts::ensure_kokoro_assets().await {
                            log::warn!("tts/kokoro: asset prefetch failed: {e}");
                        }
                    });
                    let s = tts::KokoroSpeaker::new(
                        tts::kokoro_model_path().unwrap_or_default(),
                        tts::kokoro_voices_dir().unwrap_or_default(),
                    );
                    s.set_speed(cfg.tts_speed);
                    // Honor the saved voice if it's a valid Kokoro voice; a stale
                    // id from another provider is ignored (keeps the default).
                    s.set_voice(&cfg.tts_voice_id);
                    Arc::new(s)
                }
                other => {
                    if other == "elevenlabs" {
                        log::info!("tts: ElevenLabs selected but no key in Keychain; using native");
                    } else {
                        log::info!("tts: native macOS AVSpeechSynthesizer backend");
                    }
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
                usage: usage_state.clone(),
                history: history_state.clone(),
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

            // The main (settings/history) window is created lazily on first
            // open (tray item / dock reopen) and destroyed on close, so an
            // idle Open Wispr carries no WebKit content process for a window the
            // user may never open. See `show_main_window`.

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

/// Show the main settings/history window, creating it if needed. Used by the
/// dock-reopen handler and the tray "Settings…" item.
///
/// The window is not declared in `tauri.conf.json`; it's built here on first
/// open and destroyed when the user closes it (Tauri's default), so an idle
/// Open Wispr keeps no WebKit content process for a window that's rarely opened.
/// It self-populates from the backend on load (`settings.js` `init`), so a
/// fresh instance needs no restored state.
fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }
    match WebviewWindowBuilder::new(app, "main", WebviewUrl::App("settings.html".into()))
        .title("Open Wispr")
        .inner_size(900.0, 640.0)
        .min_inner_size(640.0, 460.0)
        .center()
        .build()
    {
        Ok(win) => {
            let _ = win.set_focus();
        }
        Err(e) => log::warn!("failed to create settings window: {e}"),
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
        // Esc is released by the idle-watcher once is_speaking() flips false;
        // don't unregister from this shortcut callback (it would deadlock).
        show_overlay(app);
        emit_state(app, OverlayState::Done { chars: 0 });
        idle_after(app.clone(), Duration::from_millis(400));
        return;
    }
    // Prefer the live selection; fall back to the clipboard when there's none,
    // so text you can't mouse-select (a mouse-capturing terminal TUI, etc.) can
    // still be read after copying it with the app's own command.
    let text = match selection::capture_selection() {
        Ok(Some(text)) => Some(text),
        Ok(None) => selection::clipboard_text(),
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
            return;
        }
    };
    match text {
        Some(text) => {
            let chars = text.chars().count();
            log::info!("tts: speak ({} chars)", chars);
            record_usage(app, |u| u.record_tts(chars as u64));
            show_overlay(app);
            emit_state(app, OverlayState::Reading { progress: 0.0 });
            state.speaker.speak(&text);
            // The watcher registers the Esc-to-stop shortcut (from its own
            // thread, to avoid a global-shortcut re-lock deadlock) and releases
            // it when the read ends.
            spawn_tts_idle_watcher(app.clone());
        }
        None => {
            log::info!("tts: nothing to read (no selection or clipboard text)");
            show_overlay(app);
            emit_state(
                app,
                OverlayState::Error {
                    message: "nothing to read".into(),
                },
            );
            idle_after(app.clone(), Duration::from_millis(1600));
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

/// Find the monitor whose bounds contain `pt`, a global top-left point in
/// *logical* screen points. Each monitor's physical bounds are converted to
/// points by dividing by its own scale factor, so this stays correct on
/// mixed-DPI multi-monitor setups.
fn monitor_containing_point<R: Runtime>(
    win: &tauri::WebviewWindow<R>,
    pt: (f64, f64),
) -> Option<tauri::Monitor> {
    let monitors = win.available_monitors().ok()?;
    monitors.into_iter().find(|m| {
        let s = m.scale_factor();
        let pos = m.position();
        let size = m.size();
        let lx = pos.x as f64 / s;
        let ly = pos.y as f64 / s;
        let lw = size.width as f64 / s;
        let lh = size.height as f64 / s;
        pt.0 >= lx && pt.0 < lx + lw && pt.1 >= ly && pt.1 < ly + lh
    })
}

fn position_overlay_bottom<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    // Target the screen the user is actually working on: the one holding the
    // active window they're dictating into. Ask Accessibility for the focused
    // window's center (global top-left points) and match it to a monitor. If
    // there's no focused window, fall back to the monitor under the mouse
    // cursor, then the window's current monitor, then primary. Wrap in
    // early-returns so a single missing monitor query doesn't tank it.
    let monitor = focus::focused_window_center()
        .and_then(|pt| monitor_containing_point(win, pt))
        .or_else(|| {
            win.cursor_position()
                .ok()
                .and_then(|p| win.monitor_from_point(p.x, p.y).ok().flatten())
        })
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
                hotkeys::unregister_tts_escape(&app);
                emit_state(&app, OverlayState::Idle);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Read-aloud is playing — let Esc stop it. Registered from this
        // background thread (not the shortcut callback) to avoid a deadlock;
        // see hotkeys::register_tts_escape.
        hotkeys::register_tts_escape(&app);
        // Phase 2: wait for it to flip false (end-of-media or stop()), emitting
        // read-aloud progress so the overlay pill fills as it speaks.
        loop {
            std::thread::sleep(Duration::from_millis(120));
            let state = app.state::<AppState>();
            if !state.speaker.is_speaking() {
                hotkeys::unregister_tts_escape(&app);
                emit_state(&app, OverlayState::Idle);
                return;
            }
            if let Some(progress) = state.speaker.progress() {
                emit_state(&app, OverlayState::Reading { progress });
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
/// The tray menu plus its checkable items, in `(menu, speed, voice, mic)` order,
/// so callers can flip the checkmarks when the user changes a setting.
type TrayMenu = (
    Menu<Wry>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
);

fn build_tray_menu(
    app: &AppHandle<Wry>,
    cfg: &config::Config,
    mic_names: &[String],
) -> tauri::Result<TrayMenu> {
    let open_main = MenuItem::with_id(app, "open_main", "Settings…", true, None::<&str>)?;
    let read = MenuItem::with_id(app, "tts_read", "Read selection (⌥A)", true, None::<&str>)?;
    let stop_read = MenuItem::with_id(app, "tts_stop", "Stop reading", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Open Wispr", true, None::<&str>)?;

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
    let speed_refs: Vec<&dyn IsMenuItem<Wry>> = speed_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
    let speed_menu = Submenu::with_id_and_items(app, "speed", "Speed", true, &speed_refs)?;

    // Voice submenu — the list matches the active TTS provider so the picker
    // reflects the backend that will actually speak.
    let voice_items: Vec<CheckMenuItem<Wry>> = tts::voices_for(&cfg.tts_provider)
        .iter()
        .map(|(id, name)| {
            let item_id = format!("voice_{id}");
            let checked = *id == cfg.tts_voice_id;
            CheckMenuItem::with_id(app, item_id, *name, true, checked, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let voice_refs: Vec<&dyn IsMenuItem<Wry>> = voice_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
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
    let mic_refs: Vec<&dyn IsMenuItem<Wry>> = mic_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
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
    let provider = state
        .config
        .lock()
        .map(|c| c.tts_provider.clone())
        .unwrap_or_default();
    for (item, (id, _)) in state
        .voice_items
        .iter()
        .zip(tts::voices_for(&provider).iter())
    {
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
    chat: Arc<dyn llm::LlmChat>,
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
                DictationCmd::Stop { mode } => {
                    let Some(r) = rec.take() else {
                        log::debug!("dictation: Stop without active recording");
                        continue;
                    };
                    match r.stop() {
                        Ok(recording) => {
                            handle_recording(
                                app.clone(),
                                transcriber.clone(),
                                chat.clone(),
                                recording,
                                mode,
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

/// Record local usage (STT audio-seconds / TTS characters), persist, and push
/// the updated totals to the settings window.
fn record_usage<R: Runtime>(app: &AppHandle<R>, update: impl FnOnce(&mut usage::UsageStats)) {
    if let Ok(mut u) = app.state::<AppState>().usage.lock() {
        update(&mut u);
        let snapshot = u.clone();
        drop(u);
        if let Err(e) = usage::save(&snapshot) {
            log::warn!("usage: save failed: {e}");
        }
        let _ = app.emit("usage", snapshot);
    }
}

/// Save a committed dictation to the history database, honoring the enable
/// toggle and retention cap. `refined` is set only for Fn-refined dictations.
fn record_history<R: Runtime>(app: &AppHandle<R>, raw: &str, refined: Option<&str>) {
    // Clone the Arcs out of the managed state so the locks below don't borrow
    // the temporary `State` guard.
    let (config, history) = {
        let state = app.state::<AppState>();
        (state.config.clone(), state.history.clone())
    };
    let (enabled, limit) = config
        .lock()
        .map(|c| (c.history_enabled, c.history_limit))
        .unwrap_or((true, 1000));
    if !enabled {
        return;
    }
    let conn = match history.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Err(e) = history::insert(&conn, raw, refined) {
        log::warn!("history: insert failed: {e}");
    }
    let _ = history::prune(&conn, limit);
    drop(conn);
    let _ = app.emit("history", ()); // nudge the History tab to refresh if open
}

fn handle_recording<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    chat: Arc<dyn llm::LlmChat>,
    recording: audio::Recording,
    mode: DictationMode,
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
        let secs = recording.duration_ms as f64 / 1000.0;
        let transcribe_result = transcriber.transcribe(&recording.wav).await;
        // Groq billed us for the audio if the request succeeded.
        if transcribe_result.is_ok() {
            record_usage(&app, |u| u.record_stt(secs));
        }
        let (state, dwell_ms) = match transcribe_result {
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
                match mode {
                    DictationMode::Command => run_command(&app, chat.as_ref(), &text).await,
                    _ => {
                        run_dictation(&app, chat.as_ref(), mode == DictationMode::Refine, text)
                            .await
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

/// Plain / refined dictation: optionally clean the transcript with the LLM, log
/// it to history, and paste. Returns the overlay state + dwell for the caller
/// to emit. On refine failure we fall back to the raw transcript so a dropped
/// API call never loses the user's dictation.
async fn run_dictation<R: Runtime>(
    app: &AppHandle<R>,
    chat: &dyn llm::LlmChat,
    refine: bool,
    text: String,
) -> (OverlayState, u64) {
    let raw = text.clone(); // keep the original transcript for history
    let mut was_refined = false;
    let final_text = if refine {
        emit_state(app, OverlayState::Refining);
        // The refine prompt + cloud model come from live config (local ignores
        // the model). Refinement is the built-in Transform command.
        let (prompt, model) = match app.state::<AppState>().config.lock() {
            Ok(c) => (c.refine_prompt.clone(), c.refine_model.clone()),
            Err(_) => (String::new(), String::new()),
        };
        match llm::transform(chat, &model, &prompt, &text).await {
            Ok(res) => {
                log::info!(
                    "dictation: refined ({} chars): {}",
                    res.content.chars().count(),
                    res.content
                );
                // Record OpenRouter token/cost usage (local reports none); either
                // way refresh the settings window's usage view if it's open.
                if let Some((p, c, t, cost)) = res.usage {
                    record_usage(app, |u| u.record(p, c, t, cost));
                } else if let Ok(u) = app.state::<AppState>().usage.lock() {
                    let _ = app.emit("usage", u.clone());
                }
                was_refined = true;
                res.content
            }
            Err(e) => {
                log::warn!("dictation: refine failed, pasting raw transcript: {e}");
                text
            }
        }
    } else {
        text
    };
    record_history(
        app,
        &raw,
        if was_refined { Some(&final_text) } else { None },
    );
    let chars = final_text.chars().count();
    // `inject::paste_text` calls enigo, which posts CGEvents via Quartz.
    // CGEventPost requires a CFRunLoop on the calling thread — tokio workers
    // don't have one, and calling from a bare worker exits the process
    // silently. Run on the main thread (NSApp's run loop) instead.
    match paste_on_main_thread(app, final_text).await {
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

/// Command-chord mode: classify the transcript into one of the user's Paste
/// commands and paste its response. When nothing matches (or none are
/// configured) we paste nothing and surface it — pasting a wrong snippet would
/// be worse than pasting nothing. Command runs are intentionally not recorded to
/// dictation history (they're canned output, not dictation).
async fn run_command<R: Runtime>(
    app: &AppHandle<R>,
    chat: &dyn llm::LlmChat,
    transcript: &str,
) -> (OverlayState, u64) {
    let (commands, model) = match app.state::<AppState>().config.lock() {
        Ok(c) => (c.commands.clone(), c.command_model.clone()),
        Err(_) => (Vec::new(), String::new()),
    };
    // Only Paste commands take part in voice matching (Transform commands, like
    // refinement, are triggered by their own chord, not classified).
    let paste: Vec<&config::Command> = commands
        .iter()
        .filter(|c| matches!(c.action, config::Action::Paste { .. }))
        .collect();
    if paste.is_empty() {
        log::info!("command: no voice commands configured");
        return (
            OverlayState::Error {
                message: "no commands configured".into(),
            },
            2200,
        );
    }
    emit_state(app, OverlayState::Interpreting);
    match llm::classify(chat, &model, transcript, &paste).await {
        Ok(Some(i)) => {
            let cmd = paste[i];
            let response = match &cmd.action {
                config::Action::Paste { response } => response.clone(),
                _ => unreachable!("only Paste commands are classified"),
            };
            let chars = response.chars().count();
            log::info!("command: matched '{}' → pasting {} chars", cmd.name, chars);
            match paste_on_main_thread(app, response).await {
                Ok(()) => (OverlayState::Done { chars }, 800),
                Err(e) => {
                    log::warn!("command: paste failed: {e}");
                    (
                        OverlayState::Error {
                            message: format!("paste failed: {e}"),
                        },
                        2500,
                    )
                }
            }
        }
        Ok(None) => {
            log::info!("command: no match for '{transcript}'");
            (
                OverlayState::Error {
                    message: "no matching command".into(),
                },
                2000,
            )
        }
        Err(e) => {
            log::warn!("command: classify failed: {e}");
            (
                OverlayState::Error {
                    message: format!("command failed: {e}"),
                },
                3000,
            )
        }
    }
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
