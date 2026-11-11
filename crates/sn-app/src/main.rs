//! macOS-native 120fps ServiceNow desktop client (`snrust`) built on Rust + GPUI.

mod actions;
mod command_palette;
mod connect_modal;
mod demo_server;
mod form_view;
mod list_view;
mod state;
mod theme;
mod workspace;

use gpui_kit::assets::AllAssets;
use gpui_kit::component::TitleBar;
use gpui_kit::component::theme::ThemeMode;
use gpui_kit::*;
use sn_cache::SnCache;

use crate::actions::*;
use crate::state::AppServices;
use crate::theme::apply_snrust_theme;
use crate::workspace::WorkspaceShell;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "snrust=info,sn_api=info,sn_realtime=info".into()),
        )
        .init();

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
            apply_snrust_theme(ThemeMode::Dark, cx);
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
        KeyBinding::new("cmd-,", OpenInstanceManager, None),
        KeyBinding::new("cmd-s", SaveActiveForm, None),
        KeyBinding::new("cmd-shift-s", UpdateAndCloseActiveForm, None),
        KeyBinding::new("cmd-r", RefreshActiveView, None),
        KeyBinding::new("cmd-shift-e", ToggleCustomizeGrid, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSplitPane, None),
        KeyBinding::new("cmd-w", CloseActiveTab, None),
        KeyBinding::new("cmd-n", NewRecordInActiveTable, None),
    ]);

    cx.set_menus(vec![
        Menu {
            name: "snrust".into(),
            items: vec![
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
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Side-by-Side Split", ToggleSplitPane),
            ],
            disabled: false,
        },
    ]);
}
