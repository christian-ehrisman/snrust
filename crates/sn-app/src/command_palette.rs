//! `Cmd+K` Omnibox / Command Palette with SN Utils shortcuts (`incident.list`, `incident.do`,
//! `INC0010001` record jump), FTS5 instant table search, recent history, and workspace actions.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_core::InstanceId;

use crate::actions::{ToggleCustomizeGrid, ToggleSplitPane, ToggleThemeMode};
use crate::state::{AppServices, WorkspaceCommand};

#[derive(Clone)]
enum PaletteAction {
    OpenList {
        table: String,
        label: String,
        query: String,
    },
    OpenNewRecord {
        table: String,
    },
    OpenRecordByNumber {
        table: String,
        number: String,
    },
    OpenRecordById {
        table: String,
        sys_id: String,
    },
    SwitchInstancesModal,
    ToggleTheme,
    ToggleSplit,
    CustomizeFormGrid,
}

#[derive(Clone)]
struct PaletteEntry {
    icon: IconName,
    title: String,
    subtitle: String,
    shortcut_hint: Option<&'static str>,
    action: PaletteAction,
}

pub struct CommandPaletteView {
    services: AppServices,
    instance_id: Option<InstanceId>,
    query_input: Entity<InputState>,
    entries: Vec<PaletteEntry>,
    selected_ix: usize,
    _sub: Subscription,
}

impl CommandPaletteView {
    pub fn new(
        services: AppServices,
        instance_id: Option<InstanceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(
                "Type table (incident), shortcut (incident.list, incident.do), or record (INC0010001)…",
            )
        });

        let sub = cx.subscribe_in(
            &query_input,
            window,
            |this: &mut Self, _, evt, window, cx| match evt {
                InputEvent::Change => {
                    this.rebuild_entries(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { secondary, .. } => {
                    this.execute_selected(*secondary, window, cx);
                }
                _ => {}
            },
        );

        let mut this = Self {
            services,
            instance_id,
            query_input,
            entries: Vec::new(),
            selected_ix: 0,
            _sub: sub,
        };
        this.rebuild_entries(cx);
        this
    }

    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_input.focus_handle(cx)
    }

    fn rebuild_entries(&mut self, cx: &App) {
        let raw = self.query_input.read(cx).value().trim().to_string();
        let lower = raw.to_ascii_lowercase();
        let mut out = Vec::new();

        // 1. SN Utils dot-commands: `<table>.list`, `<table>.filter`, `<table>.do`, `<table>.form`
        if let Some((tbl_raw, suffix)) = lower.split_once('.') {
            let tbl = tbl_raw.trim();
            if !tbl.is_empty() && tbl.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                match suffix {
                    "list" | "l" => {
                        out.push(PaletteEntry {
                            icon: IconName::Table,
                            title: format!("Open {tbl} List"),
                            subtitle: format!("{tbl}.list — All records"),
                            shortcut_hint: Some("↵ Open · ⌘↵ Split"),
                            action: PaletteAction::OpenList {
                                table: tbl.to_string(),
                                label: tbl.to_string(),
                                query: String::new(),
                            },
                        });
                    }
                    "filter" | "f" => {
                        out.push(PaletteEntry {
                            icon: IconName::Funnel,
                            title: format!("Open {tbl} Active Filter"),
                            subtitle: format!("{tbl}.filter — active=true"),
                            shortcut_hint: Some("↵ Open"),
                            action: PaletteAction::OpenList {
                                table: tbl.to_string(),
                                label: tbl.to_string(),
                                query: "active=true".to_string(),
                            },
                        });
                    }
                    "do" | "new" | "form" => {
                        out.push(PaletteEntry {
                            icon: IconName::FilePlus,
                            title: format!("Create New {tbl} Record"),
                            subtitle: format!("{tbl}.do — New form"),
                            shortcut_hint: Some("↵ Create"),
                            action: PaletteAction::OpenNewRecord {
                                table: tbl.to_string(),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        // 2. Direct Record Number resolution (`INC0010001`, `CHG0030001`, etc.)
        if let Some(inst_id) = &self.instance_id {
            let hierarchy = self
                .services
                .cache
                .get_table_hierarchy(inst_id)
                .unwrap_or_default();
            if let Some((table, normalized_num)) = hierarchy.resolve_record_number(&raw) {
                out.push(PaletteEntry {
                    icon: IconName::ArrowUpRight,
                    title: format!("Jump to {normalized_num}"),
                    subtitle: format!("Open record directly from {table}"),
                    shortcut_hint: Some("↵ Jump"),
                    action: PaletteAction::OpenRecordByNumber {
                        table,
                        number: normalized_num,
                    },
                });
            }

            // 3. Recent items matching query (or top recents when query is empty)
            if let Ok(recents) = self.services.cache.list_recent(inst_id, 10) {
                for r in recents {
                    if !lower.is_empty()
                        && !r.title.to_ascii_lowercase().contains(&lower)
                        && !r.table.to_ascii_lowercase().contains(&lower)
                    {
                        continue;
                    }
                    if let Some(sys_id) = r.sys_id {
                        out.push(PaletteEntry {
                            icon: IconName::Clock,
                            title: r.title.clone(),
                            subtitle: r
                                .subtitle
                                .unwrap_or_else(|| format!("Recent {} record", r.table)),
                            shortcut_hint: Some("Recent"),
                            action: PaletteAction::OpenRecordById {
                                table: r.table,
                                sys_id,
                            },
                        });
                    }
                    if out.len() >= 4 && raw.is_empty() {
                        break;
                    }
                }
            }

            // 4. FTS5 Table Catalog Search
            if let Ok(tables) = self.services.cache.search_tables(inst_id, &raw, 10) {
                for t in tables {
                    out.push(PaletteEntry {
                        icon: IconName::Table,
                        title: format!("{} ({})", t.label, t.name),
                        subtitle: format!("Open classic list view · {}.list", t.name),
                        shortcut_hint: Some("↵ List · ⌘↵ Split"),
                        action: PaletteAction::OpenList {
                            table: t.name.clone(),
                            label: t.label.clone(),
                            query: String::new(),
                        },
                    });
                }
            }
        }

        // 5. Built-in App Commands
        let commands = [
            (
                IconName::LayoutGrid,
                "Customize Active Form Auto-Grid",
                "Reorder fields, toggle 1x/2x column spans, or hide/add columns",
                Some("⇧⌘E"),
                PaletteAction::CustomizeFormGrid,
            ),
            (
                IconName::Columns2,
                "Toggle Side-by-Side Split View",
                "Keep List View on the left and Live Form View on the right",
                Some("⌘\\"),
                PaletteAction::ToggleSplit,
            ),
            (
                IconName::SunMoon,
                "Switch Dark / Light Theme",
                "Toggle between Linear Obsidian Dark and Crisp Daylight palettes",
                None,
                PaletteAction::ToggleTheme,
            ),
            (
                IconName::Server,
                "Switch or Connect ServiceNow Instance",
                "Manage OAuth 2.0 PKCE profiles, Keychain tokens, or Demo Sandbox",
                Some("⌘,"),
                PaletteAction::SwitchInstancesModal,
            ),
        ];

        for (icon, title, sub, hint, act) in commands {
            if lower.is_empty()
                || title.to_ascii_lowercase().contains(&lower)
                || sub.to_ascii_lowercase().contains(&lower)
            {
                out.push(PaletteEntry {
                    icon,
                    title: title.to_string(),
                    subtitle: sub.to_string(),
                    shortcut_hint: hint,
                    action: act,
                });
            }
        }

        self.entries = out;
        if self.selected_ix >= self.entries.len() {
            self.selected_ix = 0;
        }
    }

    fn execute_selected(&mut self, in_split: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(self.selected_ix).cloned() else {
            return;
        };
        self.execute_entry(entry, in_split, window, cx);
    }

    fn execute_entry(
        &mut self,
        entry: PaletteEntry,
        in_split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_dialog(cx);
        match entry.action {
            PaletteAction::OpenList {
                table,
                label,
                query,
            } => {
                self.services.send_cmd(WorkspaceCommand::OpenList {
                    table,
                    label: Some(label),
                    encoded_query: query,
                    in_split,
                });
            }
            PaletteAction::OpenNewRecord { table } => {
                self.services
                    .send_cmd(WorkspaceCommand::OpenNewRecord { table, in_split });
            }
            PaletteAction::OpenRecordByNumber { table, number } => {
                self.services
                    .send_cmd(WorkspaceCommand::OpenRecordByNumber { table, number });
            }
            PaletteAction::OpenRecordById { table, sys_id } => {
                self.services.send_cmd(WorkspaceCommand::OpenRecord {
                    table,
                    sys_id,
                    seed_record: None,
                    in_split,
                });
            }
            PaletteAction::SwitchInstancesModal => {
                self.services.send_cmd(WorkspaceCommand::ShowInstanceModal);
            }
            PaletteAction::ToggleTheme => {
                window.dispatch_action(Box::new(ToggleThemeMode), cx);
            }
            PaletteAction::ToggleSplit => {
                window.dispatch_action(Box::new(ToggleSplitPane), cx);
            }
            PaletteAction::CustomizeFormGrid => {
                window.dispatch_action(Box::new(ToggleCustomizeGrid), cx);
            }
        }
    }
}

impl Render for CommandPaletteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        v_flex()
            .w_full()
            .gap_2p5()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if key == "down" && !this.entries.is_empty() {
                    this.selected_ix = (this.selected_ix + 1) % this.entries.len();
                    cx.notify();
                } else if key == "up" && !this.entries.is_empty() {
                    this.selected_ix = if this.selected_ix == 0 {
                        this.entries.len() - 1
                    } else {
                        this.selected_ix - 1
                    };
                    cx.notify();
                } else if key == "enter" {
                    let in_split = event.keystroke.modifiers.platform;
                    this.execute_selected(in_split, window, cx);
                }
            }))
            .child(
                Input::new(&self.query_input)
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .cleanable(true),
            )
            .child(
                v_flex()
                    .id("palette-results-scroll")
                    .max_h(px(360.0))
                    .overflow_y_scroll()
                    .gap_1()
                    .children(self.entries.iter().cloned().enumerate().map(|(ix, entry)| {
                        let is_sel = ix == self.selected_ix;
                        let entry_click = entry.clone();

                        h_flex()
                            .id(("palette-row", ix))
                            .px_3()
                            .py_2()
                            .justify_between()
                            .items_center()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .bg(if is_sel {
                                theme.accent
                            } else {
                                theme.transparent
                            })
                            .hover(|s| s.bg(theme.accent))
                            .on_click(cx.listener(move |this, evt: &ClickEvent, window, cx| {
                                let split = evt.modifiers().platform;
                                this.execute_entry(entry_click.clone(), split, window, cx);
                            }))
                            .child(
                                h_flex()
                                    .gap_2p5()
                                    .items_center()
                                    .overflow_hidden()
                                    .child(Icon::new(entry.icon).small().text_color(if is_sel {
                                        theme.primary
                                    } else {
                                        theme.muted_foreground
                                    }))
                                    .child(
                                        v_flex()
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_medium()
                                                    .truncate()
                                                    .child(entry.title),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(entry.subtitle),
                                            ),
                                    ),
                            )
                            .when_some(entry.shortcut_hint, |el, hint| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(px(4.0))
                                        .bg(theme.secondary)
                                        .text_size(px(10.5))
                                        .text_color(theme.muted_foreground)
                                        .child(hint),
                                )
                            })
                    })),
            )
    }
}
