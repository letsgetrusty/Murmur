// Module layout per voice-tool-architecture.md §4. Most are stubs in Phase 0;
// real implementations land in subsequent phases.
mod audio;
mod config;
mod hotkeys;
mod inject;
mod kb;
mod secrets;
mod selection;
mod stt;
mod tts;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // Tray icon with quit menu. Left-click toggles the overlay so there is
            // always a mouse-driven fallback if the hotkey is unregistered.
            let quit = MenuItem::with_id(app, "quit", "Quit murmur", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "toggle", "Show/hide overlay", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::with_id("main")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "toggle" => hotkeys::toggle_overlay(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        hotkeys::toggle_overlay(tray.app_handle());
                    }
                })
                .build(app)?;

            // Hotkey registration MUST happen on the main thread on macOS, which
            // is where `setup` runs.
            hotkeys::register(app.handle())?;

            // Pre-position the overlay; keep it hidden until the hotkey fires.
            if let Some(win) = app.get_webview_window("overlay") {
                let _ = win.set_always_on_top(true);
                let _ = win.set_skip_taskbar(true);
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
