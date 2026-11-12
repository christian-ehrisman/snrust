//! Headless UI tests: the real workspace rendered in a test window and driven by simulated
//! keystrokes and direct view calls. They assert behaviour and state, not pixels.

use std::time::Duration;

use gpui::{Modifiers, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, Point, Window,
    WindowBounds, WindowOptions, px, size,
};
use sn_api::SnClient;
use sn_cache::SnCache;
use sn_core::{ClassicTarget, FilterOperator, Joiner, NavNode};

use crate::favorites::{self, FavoriteRef};
use crate::prefs::{AppPrefs, Appearance};
use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::apply_snrust_theme;
use crate::workspace::WorkspaceShell;

fn render_frame(window: &mut Window, cx: &mut App) {
    window.refresh();
    window.draw(cx).clear(cx);
}

pub struct Harness {
    pub window: AnyWindowHandle,
    pub shell: Entity<WorkspaceShell>,
    pub services: AppServices,
    pub cache: SnCache,
    /// The Demo Sandbox client, once `open_demo` has signed in.
    client: Option<SnClient>,
    // Dropped last so background tasks outlive the views that spawned them.
    _runtime: tokio::runtime::Runtime,
}

impl Harness {
    /// Opens the workspace on an empty in-memory profile (no instance connected).
    pub fn open(cx: &mut TestAppContext) -> Self {
        // Views are woken from Tokio worker threads (network, SQLite), which the
        // deterministic test scheduler otherwise reports as an error.
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let cache = SnCache::open_in_memory().expect("in-memory cache");
        let (services, cmd_rx) = AppServices::new(runtime.handle().clone(), cache.clone());
        let shell_services = services.clone();
        let (window, shell) = cx.update(|cx| {
            gpui_kit::init(cx);
            AppPrefs::install(cache.clone(), cx);
            apply_snrust_theme(cx);
            crate::register_menus_and_keys(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1380.0), px(860.0)),
            };
            let (window, shell) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| WorkspaceShell::new(shell_services, cmd_rx, window, cx))
                },
            )
            .expect("open test window");
            (window, shell)
        });
        let harness = Self {
            window,
            shell,
            services,
            cache,
            client: None,
            _runtime: runtime,
        };
        harness.update(cx, |window, cx| {
            let focus = harness.shell.read(cx).focus_handle(cx);
            focus.focus(window, cx);
            render_frame(window, cx);
        });
        harness
    }

    /// Opens the workspace signed in to the local Demo Sandbox, with the incident list loaded.
    pub fn open_demo(cx: &mut TestAppContext) -> Self {
        let mut harness = Self::open(cx);
        let profile = harness
            ._runtime
            .block_on(harness.services.launch_demo_sandbox())
            .expect("demo sandbox starts");
        harness.client = Some(harness.services.client_for(&profile).unwrap());
        harness
            .services
            .send_cmd(WorkspaceCommand::SwitchInstance { profile });
        let shell = harness.shell.clone();
        harness.wait_until(
            cx,
            "the demo incident list and navigation to load",
            move |_, cx| {
                let shell = shell.read(cx);
                !shell.navigation().applications.is_empty()
                    && shell
                        .active_list()
                        .is_some_and(|list| !list.read(cx).records().is_empty())
            },
        );
        harness
    }

    pub fn client(&self) -> &SnClient {
        self.client.as_ref().expect("open_demo signs in")
    }

    /// What the ServiceNow side currently holds, as an indented outline of favorite titles.
    pub fn server_favorites(&self) -> Vec<String> {
        fn walk(nodes: &[NavNode], depth: usize, out: &mut Vec<String>) {
            for node in nodes {
                out.push(format!("{}{}", "> ".repeat(depth), node.title));
                walk(&node.children, depth + 1, out);
            }
        }
        let tree = self
            ._runtime
            .block_on(self.client().fetch_user_favorites())
            .expect("favorites are readable");
        let mut out = Vec::new();
        walk(&tree, 0, &mut out);
        out
    }

    /// Waits for favorite writes to land: the sidebar tree matches the server and nothing
    /// is still shown optimistically.
    pub fn wait_for_favorites(&self, cx: &mut TestAppContext, expected: &[&str]) {
        let shell = self.shell.clone();
        let mut last = Vec::new();
        for _ in 0..400 {
            cx.run_until_parked();
            let (local, pending) = self.update(cx, |window, cx| {
                render_frame(window, cx);
                fn walk(
                    nodes: &[NavNode],
                    depth: usize,
                    out: &mut Vec<String>,
                    pending: &mut bool,
                ) {
                    for node in nodes {
                        *pending |= node.id.starts_with("pending:");
                        out.push(format!("{}{}", "> ".repeat(depth), node.title));
                        walk(&node.children, depth + 1, out, pending);
                    }
                }
                let mut out = Vec::new();
                let mut pending = false;
                walk(
                    &shell.read(cx).navigation().favorites,
                    0,
                    &mut out,
                    &mut pending,
                );
                (out, pending)
            });
            if !pending && local == expected && self.server_favorites() == expected {
                return;
            }
            last = local;
            std::thread::sleep(Duration::from_millis(15));
        }
        panic!(
            "favorites did not settle.\n expected: {expected:?}\n  sidebar: {last:?}\n   server: {:?}",
            self.server_favorites()
        );
    }

    /// Clicks the centre of the element tagged with `selector` in the last rendered frame.
    pub fn click(&self, cx: &mut TestAppContext, selector: &str, modifiers: Modifiers) {
        self.update(cx, render_frame);
        let mut visual = VisualTestContext::from_window(self.window, cx);
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no element tagged {selector:?} is on screen"));
        visual.simulate_click(bounds.center(), modifiers);
        cx.run_until_parked();
        self.update(cx, render_frame);
    }

    pub fn update<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        cx.update_window(self.window, |_, window, cx| f(window, cx))
            .expect("test window is open")
    }

    /// Polls in real time, because network and SQLite work runs on Tokio threads that the
    /// GPUI test clock does not drive.
    pub fn wait_until(
        &self,
        cx: &mut TestAppContext,
        what: &str,
        mut ready: impl FnMut(&mut Window, &mut App) -> bool,
    ) {
        for _ in 0..400 {
            cx.run_until_parked();
            let done = self.update(cx, |window, cx| {
                render_frame(window, cx);
                ready(window, cx)
            });
            if done {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        panic!("timed out waiting for {what}");
    }

    /// Bounds of a tagged element in a freshly rendered frame.
    pub fn bounds(&self, cx: &mut TestAppContext, selector: &str) -> Option<Bounds<gpui::Pixels>> {
        self.update(cx, render_frame);
        let selector: &'static str = Box::leak(selector.to_string().into_boxed_str());
        VisualTestContext::from_window(self.window, cx).debug_bounds(selector)
    }

    /// Sends keystrokes through key bindings to the focused view, then renders.
    pub fn press(&self, cx: &mut TestAppContext, keys: &str) {
        cx.simulate_keystrokes(self.window, keys);
        cx.run_until_parked();
        self.update(cx, render_frame);
    }
}

#[gpui::test]
fn zoom_shortcuts_scale_the_ui_and_persist(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.update(cx, |_, cx| assert_eq!(cx.theme().font_size, px(13.0)));

    harness.press(cx, "cmd-=");
    harness.update(cx, |window, cx| {
        assert_eq!(AppPrefs::get(cx).ui_scale, 1.1);
        assert_eq!(cx.theme().font_size, px(13.0 * 1.1));
        assert_eq!(window.rem_size(), px(13.0 * 1.1));
    });

    harness.press(cx, "cmd-- cmd--");
    harness.update(cx, |_, cx| assert_eq!(AppPrefs::get(cx).ui_scale, 0.9));

    harness.press(cx, "cmd-0");
    harness.update(cx, |_, cx| {
        assert_eq!(AppPrefs::get(cx).ui_scale, 1.0);
        assert_eq!(cx.theme().font_size, px(13.0));
    });

    harness.press(cx, "cmd-=");
    let saved: AppPrefs = harness
        .cache
        .get_setting("preferences")
        .unwrap()
        .expect("preferences were persisted");
    assert_eq!(saved.ui_scale, 1.1);
}

#[gpui::test]
fn settings_dialog_opens_with_cmd_comma_and_theme_choice_persists(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.update(cx, |window, cx| assert!(!window.has_active_dialog(cx)));

    harness.press(cx, "cmd-,");
    harness.update(cx, |window, cx| {
        assert!(window.has_active_dialog(cx), "⌘, opens Settings");
        // The dialog must lay out at both ends of the scale range.
        for scale in [0.8, 1.5, 1.0] {
            AppPrefs::update(cx, |prefs| prefs.ui_scale = scale);
            render_frame(window, cx);
        }
        window.close_all_dialogs(cx);
        render_frame(window, cx);

        assert!(cx.theme().mode.is_dark());
        AppPrefs::update(cx, |prefs| prefs.appearance = Appearance::Light);
        assert!(!cx.theme().mode.is_dark());
    });
    let saved: AppPrefs = harness.cache.get_setting("preferences").unwrap().unwrap();
    assert_eq!(saved.appearance, Appearance::Light);
}

#[gpui::test]
fn list_reference_opens_in_a_tab_and_cmd_click_opens_beside(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let assignee = harness.update(cx, |_, cx| {
        let list = shell
            .read(cx)
            .active_list()
            .expect("incident list is active");
        let list = list.read(cx);
        assert!(list.column_keys().contains(&"assigned_to".to_string()));
        list.records()
            .iter()
            .map(|record| record.get_raw("assigned_to").to_string())
            .find(|raw| !raw.is_empty())
            .expect("a demo incident has an assignee")
    });
    let link = format!("ref-link:sys_user:{assignee}");
    let tab = format!("form:sys_user:{assignee}:");

    harness.click(cx, &link, Modifiers::none());
    harness.wait_until(cx, "the referenced user to open in a tab", |_, cx| {
        let (primary, split) = shell.read(cx).tab_ids();
        primary.contains(&tab) && split.is_empty() && !shell.read(cx).split_is_open()
    });

    // Back on the list, ⌘-click sends the same record to the split pane instead.
    // The list that had focus is now off screen; shortcuts must still reach the workspace.
    harness.press(cx, "cmd-w");
    harness.update(cx, |_, cx| {
        assert!(!shell.read(cx).tab_ids().0.contains(&tab));
        assert!(shell.read(cx).active_list().is_some());
    });
    harness.click(cx, &link, Modifiers::command());
    harness.wait_until(
        cx,
        "the referenced user to open beside the list",
        |_, cx| {
            let (primary, split) = shell.read(cx).tab_ids();
            !primary.contains(&tab) && split.contains(&tab) && shell.read(cx).split_is_open()
        },
    );
}

#[gpui::test]
fn form_reference_value_opens_the_record_and_the_box_opens_the_picker(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let (incident, assignee) = harness.update(cx, |_, cx| {
        let list = shell.read(cx).active_list().unwrap();
        let record = list
            .read(cx)
            .records()
            .iter()
            .find(|record| !record.get_raw("assigned_to").is_empty())
            .cloned()
            .unwrap();
        (
            record.sys_id.clone(),
            record.get_raw("assigned_to").to_string(),
        )
    });
    harness.services.send_cmd(WorkspaceCommand::OpenRecord {
        table: "incident".into(),
        sys_id: incident,
        seed_record: None,
        in_split: false,
    });
    harness.wait_until(cx, "the incident form to finish loading", |_, cx| {
        shell.read(cx).active_form().is_some_and(|form| {
            let form = form.read(cx);
            !form.is_loading() && form.raw_value("assigned_to") == assignee
        })
    });

    // The empty part of the box (its trailing search icon) still opens the picker.
    harness.update(cx, |window, cx| {
        assert!(!window.has_active_dialog(cx));
        render_frame(window, cx);
    });
    let mut visual = VisualTestContext::from_window(harness.window, cx);
    let bounds = visual
        .debug_bounds("ref-pick:assigned_to")
        .expect("assigned_to reference box is on screen");
    let icon_side = gpui_kit::point(bounds.right() - px(10.0), bounds.center().y);
    visual.simulate_click(icon_side, Modifiers::none());
    cx.run_until_parked();
    harness.update(cx, |window, cx| {
        assert!(window.has_active_dialog(cx), "picker opens from the box");
        window.close_all_dialogs(cx);
    });

    // The value itself is a link to the referenced record.
    let tab = format!("form:sys_user:{assignee}:");
    harness.click(
        cx,
        &format!("ref-link:sys_user:{assignee}"),
        Modifiers::none(),
    );
    harness.wait_until(cx, "the assignee to open in a tab", |window, cx| {
        !window.has_active_dialog(cx) && shell.read(cx).tab_ids().0.contains(&tab)
    });
}

#[gpui::test]
fn hovering_a_reference_loads_its_preview(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let (instance, assignee) = harness.update(cx, |_, cx| {
        let list = shell.read(cx).active_list().unwrap();
        let list = list.read(cx);
        let assignee = list
            .records()
            .iter()
            .map(|record| record.get_raw("assigned_to").to_string())
            .find(|raw| !raw.is_empty())
            .unwrap();
        (list.instance_id(), assignee)
    });
    let cached_user = || {
        harness
            .cache
            .get_record(&instance, "sys_user", &assignee)
            .unwrap()
    };
    assert!(
        cached_user().is_none(),
        "the user is not cached before hovering"
    );

    harness.update(cx, render_frame);
    let selector: &'static str =
        Box::leak(format!("ref-link:sys_user:{assignee}").into_boxed_str());
    let mut visual = VisualTestContext::from_window(harness.window, cx);
    let bounds = visual
        .debug_bounds(selector)
        .expect("reference link on screen");
    visual.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    // Past the card's open delay.
    cx.executor().advance_clock(Duration::from_millis(600));

    harness.wait_until(
        cx,
        "the hovered record to be fetched for the card",
        |_, _| cached_user().is_some_and(|user| !user.data.is_partial),
    );
    // The card stays open and re-renders with the loaded record without refetching.
    harness.update(cx, render_frame);
    assert!(!cached_user().unwrap().data.get_display("name").is_empty());
}

#[gpui::test]
fn column_panel_docks_beside_the_table_and_edits_columns(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let list = harness.update(cx, |_, cx| shell.read(cx).active_list().unwrap());
    let full_width = harness.bounds(cx, "list-table-area").unwrap().size.width;
    assert!(harness.bounds(cx, "column-panel-dock").is_none());

    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| list.toggle_column_panel(window, cx));
    });
    let panel = harness.update(cx, |_, cx| list.read(cx).column_panel().unwrap());

    // Docked: the table narrows and keeps its full height, so its header row stays visible.
    let table = harness.bounds(cx, "list-table-area").unwrap();
    let dock = harness.bounds(cx, "column-panel-dock").unwrap();
    assert!(table.size.width < full_width);
    assert!(
        table.right() <= dock.left(),
        "panel must not overlap the table"
    );
    assert_eq!(table.top(), dock.top());
    assert!(dock.size.width > px(200.0));

    // Add the first field that is not already a column.
    let before = harness.update(cx, |_, cx| list.read(cx).column_keys().to_vec());
    let added = harness.update(cx, |_, cx| {
        panel
            .read(cx)
            .visible_field_keys(cx)
            .into_iter()
            .find(|key| !before.contains(key))
            .expect("the demo table has a field that is not shown")
    });
    harness.click(cx, &format!("col-field:{added}"), Modifiers::none());
    harness.wait_until(cx, "the new column to appear", |_, cx| {
        list.read(cx).column_keys().last() == Some(&added)
    });
    let instance = harness.update(cx, |_, cx| list.read(cx).instance_id());
    let saved = || {
        harness
            .cache
            .get_list_columns(&instance, "incident")
            .unwrap()
            .expect("column layout is saved locally")
    };
    assert_eq!(saved().last(), Some(&added));

    // Remove it again from the Shown section.
    harness.click(cx, &format!("col-shown:{added}"), Modifiers::none());
    harness.wait_until(cx, "the column to be removed", |_, cx| {
        list.read(cx).column_keys() == before.as_slice()
    });
    assert_eq!(saved(), before);

    // ⌥↓ moves the first shown column down one place.
    harness.press(cx, "alt-down");
    harness.update(cx, |_, cx| {
        let columns = list.read(cx).column_keys().to_vec();
        assert_eq!(columns[0], before[1]);
        assert_eq!(columns[1], before[0]);
    });
    assert_eq!(saved()[1], before[0]);

    // Typing searches (focus returned to the search box after the clicks above), and ↵
    // toggles the first match: here the already-shown Priority column.
    cx.simulate_input(harness.window, "priority");
    cx.run_until_parked();
    harness.update(cx, |window, cx| {
        render_frame(window, cx);
        assert!(
            panel
                .read(cx)
                .visible_field_keys(cx)
                .iter()
                .all(|key| key.contains("priority"))
        );
        assert!(
            list.read(cx)
                .column_keys()
                .contains(&"priority".to_string())
        );
    });
    harness.press(cx, "enter");
    harness.update(cx, |_, cx| {
        assert!(
            !list
                .read(cx)
                .column_keys()
                .contains(&"priority".to_string())
        );
    });

    // Esc closes the panel and gives the table its width back.
    harness.press(cx, "escape");
    assert!(harness.bounds(cx, "column-panel-dock").is_none());
    assert_eq!(
        harness.bounds(cx, "list-table-area").unwrap().size.width,
        full_width
    );
}

#[gpui::test]
fn column_panel_dot_walks_through_reference_fields(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let list = harness.update(cx, |_, cx| shell.read(cx).active_list().unwrap());
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| list.toggle_column_panel(window, cx));
    });
    let panel = harness.update(cx, |_, cx| list.read(cx).column_panel().unwrap());
    harness.update(cx, |_, cx| {
        assert!(
            !panel
                .read(cx)
                .visible_field_keys(cx)
                .iter()
                .any(|key| key.contains('.'))
        );
    });

    harness.click(cx, "col-expand:assigned_to", Modifiers::none());
    harness.wait_until(cx, "the referenced table's fields to load", |_, cx| {
        let panel = panel.read(cx);
        !panel.is_loading_related()
            && panel
                .visible_field_keys(cx)
                .contains(&"assigned_to.email".to_string())
    });
    // Expanding did not toggle the reference column itself.
    harness.update(cx, |_, cx| {
        assert!(
            list.read(cx)
                .column_keys()
                .contains(&"assigned_to".to_string())
        );
    });

    harness.click(cx, "col-field:assigned_to.email", Modifiers::none());
    harness.wait_until(cx, "the dot-walked column to be added", |_, cx| {
        list.read(cx)
            .column_keys()
            .contains(&"assigned_to.email".to_string())
    });
    harness.update(cx, |_, cx| {
        assert!(
            panel
                .read(cx)
                .shown_labels()
                .contains(&"Assigned to › Email".to_string()),
            "{:?}",
            panel.read(cx).shown_labels()
        );
    });
}

const SEEDED_FAVORITES: [&str; 4] = [
    "Create incident (compact)",
    "Queues",
    "> Critical incidents",
    "> Unassigned incidents",
];

#[gpui::test]
fn navigator_nests_modules_and_favorites_and_remembers_expansion(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);

    // Applications start collapsed; sections and modules appear as their parents open.
    assert!(harness.bounds(cx, "app:demo_desk").is_some());
    assert!(harness.bounds(cx, "app:demo_sep_lists").is_none());
    harness.click(cx, "app:demo_desk", Modifiers::none());
    assert!(harness.bounds(cx, "app:demo_new").is_some());
    assert!(
        harness.bounds(cx, "app:demo_open").is_none(),
        "section is still collapsed"
    );
    harness.click(cx, "app:demo_sep_lists", Modifiers::none());
    let section = harness.bounds(cx, "app:demo_sep_lists").unwrap();
    let module = harness.bounds(cx, "app:demo_open").unwrap();
    assert!(
        module.top() >= section.bottom(),
        "modules render under their section"
    );

    harness.click(cx, "app:demo_open", Modifiers::none());
    harness.wait_until(cx, "the module's filtered list to open", |_, cx| {
        shell
            .read(cx)
            .tab_ids()
            .0
            .contains(&"list:incident::active=true".to_string())
    });

    // Favorites groups start open; a grouped favorite opens its list and lights the star.
    harness.click(cx, "fav:demo_favorite", Modifiers::none());
    harness.wait_until(cx, "the favorite's list to open", |_, cx| {
        shell.read(cx).active_list().is_some_and(|list| {
            let list = list.read(cx);
            list.is_favorited()
                && shell
                    .read(cx)
                    .tab_ids()
                    .0
                    .contains(&"list:incident::priority=1".to_string())
        })
    });
    harness.click(cx, "fav:demo_group_queues", Modifiers::none());
    assert!(
        harness.bounds(cx, "fav:demo_favorite").is_none(),
        "group collapsed"
    );

    // Open/closed state is stored per account, keyed by sys_id.
    let instance = harness.client().profile().id.clone();
    let saved: serde_json::Value = harness
        .cache
        .get_account_metadata(&instance, "navigation.expansion")
        .unwrap()
        .expect("expansion is persisted");
    let ids = |key: &str| -> Vec<String> {
        let mut ids: Vec<String> = saved[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids("expanded"), ["demo_desk", "demo_sep_lists"]);
    assert_eq!(ids("collapsed_favorites"), ["demo_group_queues"]);
}

#[gpui::test]
fn starring_a_list_writes_the_favorite_to_servicenow_and_back(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    let list = harness.update(cx, |_, cx| shell.read(cx).active_list().unwrap());
    harness.update(cx, |_, cx| assert!(!list.read(cx).is_favorited()));

    harness.update(cx, |_, cx| {
        list.update(cx, |list, cx| list.toggle_favorite(cx))
    });
    let mut with_star = SEEDED_FAVORITES.to_vec();
    with_star.push("Incident");
    harness.wait_for_favorites(cx, &with_star);
    harness.wait_until(cx, "the star to light", |_, cx| {
        list.read(cx).is_favorited()
    });

    harness.update(cx, |_, cx| {
        list.update(cx, |list, cx| list.toggle_favorite(cx))
    });
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    harness.wait_until(cx, "the star to clear", |_, cx| {
        !list.read(cx).is_favorited()
    });
}

#[gpui::test]
fn favorites_can_be_regrouped_reordered_renamed_and_ungrouped(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    let (services, client) = (harness.services.clone(), harness.client().clone());

    // Move the top-level favorite into the group, ahead of "Unassigned incidents".
    favorites::place(
        &services,
        &client,
        "demo_new_favorite".into(),
        Some("demo_group_queues".into()),
        Some("demo_fav_unassigned".into()),
    );
    harness.wait_for_favorites(
        cx,
        &[
            "Queues",
            "> Critical incidents",
            "> Create incident (compact)",
            "> Unassigned incidents",
        ],
    );

    favorites::rename(
        &services,
        &client,
        "demo_group_queues".into(),
        "My queues".into(),
        true,
    );
    favorites::rename(
        &services,
        &client,
        "demo_favorite".into(),
        "P1".into(),
        false,
    );
    harness.wait_for_favorites(
        cx,
        &[
            "My queues",
            "> P1",
            "> Create incident (compact)",
            "> Unassigned incidents",
        ],
    );

    // A new group can take a favorite with it.
    favorites::create_group(
        &services,
        &client,
        "Later".into(),
        Some("demo_fav_unassigned".into()),
    );
    harness.wait_for_favorites(
        cx,
        &[
            "My queues",
            "> P1",
            "> Create incident (compact)",
            "Later",
            "> Unassigned incidents",
        ],
    );

    // Removing a group keeps the favorites that were inside it.
    favorites::delete_group(&services, &client, "demo_group_queues".into());
    harness.wait_for_favorites(
        cx,
        &[
            "P1",
            "Create incident (compact)",
            "Later",
            "> Unassigned incidents",
        ],
    );

    favorites::remove(
        &services,
        &client,
        FavoriteRef::Synced("demo_favorite".into()),
    );
    harness.wait_for_favorites(
        cx,
        &[
            "Create incident (compact)",
            "Later",
            "> Unassigned incidents",
        ],
    );
}

#[gpui::test]
fn a_refused_favorite_is_kept_locally_and_can_be_uploaded_later(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    let list = harness.update(cx, |_, cx| shell.read(cx).active_list().unwrap());

    harness.services.set_demo_rejects_ui_writes(true);
    harness.update(cx, |_, cx| {
        list.update(cx, |list, cx| list.toggle_favorite(cx))
    });
    harness.wait_until(cx, "the refused favorite to be saved locally", |_, cx| {
        shell.read(cx).local_favorites().len() == 1 && list.read(cx).is_favorited()
    });
    // ServiceNow is unchanged and the optimistic entry has been rolled back.
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    harness.update(cx, |_, cx| {
        let local = &shell.read(cx).local_favorites()[0];
        assert_eq!(local.title, "Incident");
        assert_eq!(
            favorites::local_target(local),
            ClassicTarget::List {
                table: "incident".into(),
                query: String::new(),
                view: String::new(),
            }
        );
    });

    // Once writes are allowed, uploading moves it to ServiceNow and drops the local copy.
    harness.services.set_demo_rejects_ui_writes(false);
    favorites::upload_local(&harness.services, harness.client());
    let mut uploaded = SEEDED_FAVORITES.to_vec();
    uploaded.push("Incident");
    harness.wait_for_favorites(cx, &uploaded);
    harness.wait_until(cx, "the local copy to be removed", |_, cx| {
        shell.read(cx).local_favorites().is_empty() && list.read(cx).is_favorited()
    });
}

#[gpui::test]
fn dragging_a_favorite_onto_a_group_files_it_there(cx: &mut TestAppContext) {
    let harness = Harness::open_demo(cx);
    harness.wait_for_favorites(cx, &SEEDED_FAVORITES);
    let from = harness
        .bounds(cx, "fav:demo_new_favorite")
        .unwrap()
        .center();
    let to = harness
        .bounds(cx, "fav:demo_group_queues")
        .unwrap()
        .center();

    let mut visual = VisualTestContext::from_window(harness.window, cx);
    visual.simulate_mouse_down(from, gpui::MouseButton::Left, Modifiers::none());
    // A drag only starts once the pointer has travelled, so move in two steps.
    visual.simulate_mouse_move(
        gpui_kit::point(from.x, (from.y + to.y) / 2.0),
        gpui::MouseButton::Left,
        Modifiers::none(),
    );
    visual.simulate_mouse_move(to, gpui::MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(to, gpui::MouseButton::Left, Modifiers::none());

    harness.wait_for_favorites(
        cx,
        &[
            "Queues",
            "> Critical incidents",
            "> Unassigned incidents",
            "> Create incident (compact)",
        ],
    );
}

#[gpui::test]
fn the_name_prompt_submits_on_enter_and_ignores_blank_names(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    let submitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let sink = submitted.clone();
    harness.update(cx, |window, cx| {
        crate::workspace::prompt_for_name(
            "New favorites group",
            "",
            "Create",
            move |name, _, _| sink.borrow_mut().push(name),
            window,
            cx,
        );
        render_frame(window, cx);
        assert!(window.has_active_dialog(cx));
    });

    harness.press(cx, "enter");
    harness.update(cx, |window, cx| {
        assert!(window.has_active_dialog(cx), "a blank name is not accepted");
    });
    cx.simulate_input(harness.window, "  Queues ");
    harness.press(cx, "enter");
    harness.update(cx, |window, cx| assert!(!window.has_active_dialog(cx)));
    assert_eq!(*submitted.borrow(), ["Queues"]);
}

/// Opens the demo incident list and waits for its saved-filter pills.
fn open_demo_list(cx: &mut TestAppContext) -> (Harness, Entity<crate::list_view::ListViewPanel>) {
    let harness = Harness::open_demo(cx);
    let shell = harness.shell.clone();
    let list = harness.update(cx, |_, cx| shell.read(cx).active_list().unwrap());
    harness.wait_until(cx, "saved filters to load", |_, cx| {
        list.read(cx).saved_filters().len() == 2
    });
    (harness, list)
}

fn settle(
    harness: &Harness,
    cx: &mut TestAppContext,
    list: &Entity<crate::list_view::ListViewPanel>,
    query: &str,
) {
    let query = query.to_string();
    let list = list.clone();
    harness.wait_until(cx, "the list to reload for its new filter", move |_, cx| {
        list.read(cx).query_string() == query && !list.read(cx).is_loading()
    });
}

#[gpui::test]
fn saved_filter_pills_apply_and_breadcrumb_chips_edit_the_filter(cx: &mut TestAppContext) {
    let (harness, list) = open_demo_list(cx);
    harness.update(cx, |_, cx| {
        let list = list.read(cx);
        // Global filters come from the parent table and are read-only; mine is editable.
        let pills: Vec<(&str, bool)> = list
            .saved_filters()
            .iter()
            .map(|saved| (saved.title.as_str(), saved.editable))
            .collect();
        assert_eq!(pills, [("Active", false), ("High priority", true)]);
        assert!(list.chip_texts().is_empty());
    });
    let all_count = harness.update(cx, |_, cx| list.read(cx).total_count().unwrap());

    harness.click(cx, "pill:High priority", Modifiers::none());
    settle(&harness, cx, &list, "active=true^priority<=2");
    harness.update(cx, |_, cx| {
        let list = list.read(cx);
        // Chips use field labels and choice labels, not raw names and codes.
        assert_eq!(list.chip_texts(), ["Active = true", "Priority ≤ 2 - High"]);
        assert!(!list.records().is_empty() && list.total_count().unwrap() < all_count);
        assert!(
            list.records()
                .iter()
                .all(|record| record.get_raw("priority") <= "2")
        );
    });

    // ✕ on a chip removes just that condition.
    harness.click(cx, "chip-remove:0", Modifiers::none());
    settle(&harness, cx, &list, "priority<=2");

    // A filter with OR used to show no chips at all; it now reads as one chip.
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| {
            list.set_query(
                sn_core::EncodedQuery::parse("active=true^priority=1^ORpriority=2^state=2"),
                window,
                cx,
            );
        });
    });
    settle(
        &harness,
        cx,
        &list,
        "active=true^priority=1^ORpriority=2^state=2",
    );
    harness.update(cx, |_, cx| {
        assert_eq!(
            list.read(cx).chip_texts(),
            [
                "Active = true",
                "Priority = 1 - Critical  or  Priority = 2 - High",
                "State = In Progress"
            ]
        );
    });
    // Clicking the separator after the second chip keeps the first two conditions.
    harness.click(cx, "crumb-sep:2", Modifiers::none());
    settle(&harness, cx, &list, "active=true^priority=1^ORpriority=2");

    harness.click(cx, "pill-all", Modifiers::none());
    settle(&harness, cx, &list, "");
    harness.update(cx, |_, cx| {
        assert_eq!(list.read(cx).total_count(), Some(all_count))
    });
}

#[gpui::test]
fn filter_builder_edits_conditions_inline_and_runs_them(cx: &mut TestAppContext) {
    let (harness, list) = open_demo_list(cx);
    let table_top = harness.bounds(cx, "list-table-area").unwrap().top();
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| {
            list.set_query(sn_core::EncodedQuery::parse("active=true"), window, cx);
            list.toggle_filter_builder(window, cx);
        });
    });
    settle(&harness, cx, &list, "active=true");
    let builder = harness.update(cx, |_, cx| list.read(cx).filter_builder().unwrap());

    // Inline: the builder sits above the table and pushes it down instead of covering it.
    let builder_bounds = harness.bounds(cx, "filter-builder").unwrap();
    let table = harness.bounds(cx, "list-table-area").unwrap();
    assert!(table.top() > table_top);
    assert!(builder_bounds.bottom() <= table.top());

    // AND a choice condition, then OR an alternative on the same field.
    harness.update(cx, |window, cx| {
        builder.update(cx, |builder, cx| {
            assert_eq!(
                builder.editor_kinds(),
                ["pick"],
                "boolean value is a true/false list"
            );
            builder.add_condition(Joiner::And, None, window, cx);
            let state = *builder.row_ids().last().unwrap();
            builder.choose_field(state, "state", window, cx);
            let state = *builder.row_ids().last().unwrap();
            builder.choose_value(state, "2", cx);
            builder.add_condition(Joiner::Or, Some(state), window, cx);
            let alternative = *builder.row_ids().last().unwrap();
            builder.choose_value(alternative, "1", cx);
            assert_eq!(builder.editor_kinds(), ["pick", "pick", "pick"]);
            builder.run_now(cx);
        });
    });
    settle(&harness, cx, &list, "active=true^state=2^ORstate=1");

    // The value editor follows the field type and operator.
    harness.update(cx, |window, cx| {
        builder.update(cx, |builder, cx| {
            builder.add_condition(Joiner::NewQuery, None, window, cx);
            let row = *builder.row_ids().last().unwrap();
            builder.choose_field(row, "assigned_to", window, cx);
            let row = *builder.row_ids().last().unwrap();
            assert_eq!(builder.editor_kinds().last(), Some(&"reference"));
            builder.choose_operator(row, FilterOperator::Contains, window, cx);
            assert_eq!(builder.editor_kinds().last(), Some(&"text"));
            let row = *builder.row_ids().last().unwrap();
            builder.choose_operator(row, FilterOperator::IsEmpty, window, cx);
            assert_eq!(builder.editor_kinds().last(), Some(&"none"));
            builder.run_now(cx);
        });
    });
    settle(
        &harness,
        cx,
        &list,
        "active=true^state=2^ORstate=1^NQassigned_toISEMPTY",
    );
    harness.update(cx, |_, cx| {
        let list = list.read(cx);
        assert!(!list.records().is_empty());
        assert!(list.records().iter().all(|record| {
            record.get_raw("assigned_to").is_empty()
                || (record.get_raw("active") == "true"
                    && ["1", "2"].contains(&record.get_raw("state")))
        }));
    });

    // Removing the condition an OR hung from keeps a valid query.
    harness.update(cx, |_, cx| {
        builder.update(cx, |builder, cx| {
            let first_state = builder.row_ids()[1];
            builder.delete_row(first_state, cx);
            builder.run_now(cx);
        });
    });
    settle(
        &harness,
        cx,
        &list,
        "active=true^state=1^NQassigned_toISEMPTY",
    );

    // Changing the filter elsewhere (a pill) is reflected in the open builder.
    harness.click(cx, "pill:High priority", Modifiers::none());
    settle(&harness, cx, &list, "active=true^priority<=2");
    harness.update(cx, |_, cx| assert_eq!(builder.read(cx).row_ids().len(), 2));
}

#[gpui::test]
fn saved_filters_are_written_to_servicenow_or_kept_locally_when_refused(cx: &mut TestAppContext) {
    let (harness, list) = open_demo_list(cx);
    let chain = vec!["incident".to_string(), "task".to_string()];
    let server_titles = || -> Vec<String> {
        harness
            ._runtime
            .block_on(harness.client().fetch_saved_filters(&chain))
            .unwrap()
            .into_iter()
            .map(|saved| saved.title)
            .collect()
    };
    let titles = |cx: &mut TestAppContext| -> Vec<String> {
        harness.update(cx, |_, cx| {
            list.read(cx)
                .saved_filters()
                .iter()
                .map(|saved| saved.title.clone())
                .collect()
        })
    };

    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| {
            list.save_filter("Unassigned".into(), "assigned_toISEMPTY".into(), window, cx);
        });
    });
    harness.wait_until(cx, "the new pill", |_, cx| {
        list.read(cx).saved_filters().len() == 3
    });
    assert_eq!(server_titles(), ["Active", "High priority", "Unassigned"]);

    let id = harness.update(cx, |_, cx| {
        let list = list.read(cx);
        let saved = list
            .saved_filters()
            .iter()
            .find(|s| s.title == "Unassigned")
            .unwrap();
        assert!(saved.editable && !saved.id.starts_with("local:"));
        saved.id.clone()
    });
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| {
            list.update_filter(
                id.clone(),
                Some("Nobody's".into()),
                Some("assigned_toISEMPTY^active=true".into()),
                window,
                cx,
            );
        });
    });
    harness.wait_until(cx, "the rename", |_, cx| {
        list.read(cx)
            .saved_filters()
            .iter()
            .any(|s| s.title == "Nobody's" && s.filter == "assigned_toISEMPTY^active=true")
    });
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| list.delete_filter(id, window, cx))
    });
    harness.wait_until(cx, "the delete", |_, cx| {
        list.read(cx).saved_filters().len() == 2
    });
    assert_eq!(server_titles(), ["Active", "High priority"]);

    // Without write access the pill is still saved, on this Mac, and can be removed again.
    harness.services.set_demo_rejects_ui_writes(true);
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| {
            list.save_filter("Local only".into(), "state=1".into(), window, cx)
        });
    });
    harness.wait_until(cx, "the local pill", |_, cx| {
        list.read(cx)
            .saved_filters()
            .iter()
            .any(|s| s.title == "Local only" && s.id.starts_with("local:"))
    });
    assert_eq!(server_titles(), ["Active", "High priority"]);
    assert_eq!(titles(cx), ["Active", "High priority", "Local only"]);
    let local_id = harness.update(cx, |_, cx| list.read(cx).saved_filters()[2].id.clone());
    harness.update(cx, |window, cx| {
        list.update(cx, |list, cx| list.delete_filter(local_id, window, cx))
    });
    harness.wait_until(cx, "the local pill to go", |_, cx| {
        list.read(cx).saved_filters().len() == 2
    });
}
