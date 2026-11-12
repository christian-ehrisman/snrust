//! macOS-native 120fps ServiceNow desktop client (`snrust`) built on Rust + GPUI.

mod actions;
mod column_panel;
mod command_palette;
mod connect_modal;
mod demo_server;
mod favorites;
mod field_picker;
mod filter_builder;
mod form_view;
mod list_view;
mod preferences;
mod prefs;
mod reference_preview;
mod state;
mod theme;
#[cfg(test)]
mod ui_tests;
mod workspace;

use gpui_kit::assets::AllAssets;
use gpui_kit::component::TitleBar;
use gpui_kit::*;
use sn_cache::SnCache;
use tracing_subscriber::fmt::writer::MakeWriterExt as _;

use crate::actions::*;
use crate::prefs::AppPrefs;
use crate::state::AppServices;
use crate::theme::apply_snrust_theme;
use crate::workspace::WorkspaceShell;

fn main() {
    init_logging();

    // Dedicated multi-threaded Tokio runtime powering HTTP/2 REST, OAuth loopback,
    // AMB WebSockets, adaptive delta-polling, and background SQLite tasks off the UI thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("snrust-tokio")
        .build()
        .expect("failed starting Tokio runtime");

    let tokio_handle = runtime.handle().clone();
    let cache = SnCache::open_default()
        .or_else(|_| SnCache::open_in_memory())
        .expect("failed opening snrust SQLite cache");

    gpui_kit::application()
        .with_assets(AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            AppPrefs::install(cache.clone(), cx);
            apply_snrust_theme(cx);
            register_menus_and_keys(cx);

            let (services, cmd_rx) = AppServices::new(tokio_handle.clone(), cache.clone());
            let bounds = Bounds::centered(None, size(px(1380.0), px(860.0)), cx);

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(920.0), px(600.0))),
                ..TitleBar::window_options()
            };

            let (_window_handle, shell) =
                gpui_kit::open_window(window_options, cx, move |window, cx| {
                    cx.new(|cx| WorkspaceShell::new(services, cmd_rx, window, cx))
                })
                .expect("failed opening snrust macOS window");

            cx.activate(true);
            let _ = shell;
        });
}

fn register_menus_and_keys(cx: &mut App) {
    cx.on_action(|_: &QuitApp, cx| cx.quit());

    cx.bind_keys([
        KeyBinding::new("cmd-q", QuitApp, None),
        KeyBinding::new("cmd-k", OpenCommandPalette, None),
        KeyBinding::new("cmd-,", OpenPreferences, None),
        KeyBinding::new("cmd-shift-,", OpenInstanceManager, None),
        KeyBinding::new("cmd-s", SaveActiveForm, None),
        KeyBinding::new("cmd-shift-s", UpdateAndCloseActiveForm, None),
        KeyBinding::new("cmd-r", RefreshActiveView, None),
        KeyBinding::new("cmd-shift-e", ToggleCustomizeGrid, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSplitPane, None),
        KeyBinding::new("cmd-w", CloseActiveTab, None),
        KeyBinding::new("cmd-n", NewRecordInActiveTable, None),
        KeyBinding::new("cmd-=", IncreaseUiScale, None),
        KeyBinding::new("cmd-+", IncreaseUiScale, None),
        KeyBinding::new("cmd--", DecreaseUiScale, None),
        KeyBinding::new("cmd-0", ResetUiScale, None),
    ]);

    cx.set_menus(vec![
        Menu {
            name: "snrust".into(),
            items: vec![
                MenuItem::action("Settings…", OpenPreferences),
                MenuItem::action("Instance Connections…", OpenInstanceManager),
                MenuItem::separator(),
                MenuItem::action("Toggle Dark / Light Theme", ToggleThemeMode),
                MenuItem::separator(),
                MenuItem::action("Quit snrust", QuitApp),
            ],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Record", NewRecordInActiveTable),
                MenuItem::action("Command Palette…", OpenCommandPalette),
                MenuItem::separator(),
                MenuItem::action("Save Record", SaveActiveForm),
                MenuItem::action("Update & Close Record", UpdateAndCloseActiveForm),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseActiveTab),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Refresh View", RefreshActiveView),
                MenuItem::action("Customize Form Auto-Grid", ToggleCustomizeGrid),
                MenuItem::separator(),
                MenuItem::action("Zoom In", IncreaseUiScale),
                MenuItem::action("Zoom Out", DecreaseUiScale),
                MenuItem::action("Actual Size", ResetUiScale),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Side-by-Side Split", ToggleSplitPane),
            ],
            disabled: false,
        },
    ]);
}

/// Size at which the log rotates to `snrust.log.1`, bounding disk use to two files.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Logs to stderr and `~/Library/Logs/snrust/snrust.log` (also browsable in Console.app).
///
/// A Finder-launched `.app` discards stderr, so the file is the only record of rejected
/// ServiceNow requests there. Verbosity is overridable with `RUST_LOG`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "snrust=info,sn_api=info,sn_realtime=info".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match open_log_file() {
        Some(file) => builder
            .with_ansi(false)
            .with_writer(std::io::stderr.and(std::sync::Mutex::new(file)))
            .init(),
        None => builder.init(),
    }
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Logs/snrust");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("snrust.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join("snrust.log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}
