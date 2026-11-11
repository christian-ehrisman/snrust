//! Main macOS application shell: transparent `TitleBar`, live AMB/Delta status pill,
//! collapsible FTS5 table navigator sidebar, multi-tab bar, and side-by-side split workspace.

use std::cell::Cell;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::theme::{ActiveTheme as _, ThemeMode};
use gpui_kit::component::{
    Icon, Sizable as _, StyledExt as _, TitleBar, WindowExt as _, h_flex, h_resizable,
    resizable_panel, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::SnClient;
use sn_cache::{CacheStats, FavoriteItem, FavoriteKind, RecentItem};
use sn_core::{InstanceProfile, SnRecord, TableDescriptor};
use sn_realtime::{RealtimeEvent, RealtimeTransportMode};

use crate::actions::*;
use crate::command_palette::CommandPaletteView;
use crate::connect_modal::ConnectModalView;
use crate::form_view::FormViewPanel;
use crate::list_view::ListViewPanel;
use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::{apply_snrust_theme, parse_hex_color};

#[derive(Clone)]
pub enum WorkspaceTab {
    List {
        id: String,
        entity: Entity<ListViewPanel>,
    },
    Form {
        id: String,
        entity: Entity<FormViewPanel>,
    },
}

impl WorkspaceTab {
    pub fn id(&self) -> &str {
        match self {
            Self::List { id, .. } | Self::Form { id, .. } => id.as_str(),
        }
    }

    pub fn title(&self, cx: &App) -> String {
        match self {
            Self::List { entity, .. } => entity.read(cx).tab_title(),
            Self::Form { entity, .. } => entity.read(cx).tab_title(),
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        match self {
            Self::List { .. } => false,
            Self::Form { entity, .. } => entity.read(cx).is_dirty(),
        }
    }

    pub fn icon(&self) -> IconName {
        match self {
            Self::List { .. } => IconName::Table,
            Self::Form { .. } => IconName::FileText,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneSlot {
    Primary,
    Split,
}

pub struct WorkspaceShell {
    focus_handle: FocusHandle,
    services: AppServices,
    active_profile: Option<InstanceProfile>,
    active_client: Option<SnClient>,
    transport_mode: RealtimeTransportMode,
    transport_latency_ms: Option<u64>,

    sidebar_open: bool,
    sidebar_filter_input: Entity<InputState>,
    sidebar_tables: Vec<TableDescriptor>,
    favorites: Vec<FavoriteItem>,
    recents: Vec<RecentItem>,
    cache_stats: CacheStats,

    primary_tabs: Vec<WorkspaceTab>,
    active_primary_ix: usize,
    split_enabled: bool,
    split_tabs: Vec<WorkspaceTab>,
    active_split_ix: usize,
    focused_pane: PaneSlot,

    _subscriptions: Vec<Subscription>,
}

impl WorkspaceShell {
    pub fn new(
        services: AppServices,
        cmd_rx: async_channel::Receiver<WorkspaceCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let sidebar_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables…"));

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &sidebar_filter_input,
            window,
            |this: &mut Self, _, evt, _, cx| {
                if matches!(evt, InputEvent::Change) {
                    this.refresh_sidebar_lists(cx);
                    cx.notify();
                }
            },
        ));

        subs.push(cx.observe_window_activation(window, |this, window, cx| {
            this.services
                .realtime
                .set_app_focused(window.is_window_active());
            cx.notify();
        }));

        let rt_rx = services.realtime.subscribe_events();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(evt) = rt_rx.recv().await {
                let _ = this.update_in(cx, |shell, window, cx| {
                    shell.handle_realtime_event(evt, window, cx);
                });
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            while let Ok(cmd) = cmd_rx.recv().await {
                let _ = this.update_in(cx, |shell, window, cx| {
                    shell.handle_workspace_command(cmd, window, cx);
                });
            }
        })
        .detach();

        // Restore previously active ServiceNow instance if present
        let initial_profile = services
            .cache
            .get_active_instance_id()
            .ok()
            .flatten()
            .and_then(|id| {
                services
                    .cache
                    .list_instances()
                    .ok()?
                    .into_iter()
                    .find(|p| p.id == id)
            })
            .or_else(|| {
                services
                    .cache
                    .list_instances()
                    .ok()
                    .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)))
            });

        let mut shell = Self {
            focus_handle,
            services,
            active_profile: None,
            active_client: None,
            transport_mode: RealtimeTransportMode::Connecting,
            transport_latency_ms: None,
            sidebar_open: true,
            sidebar_filter_input,
            sidebar_tables: Vec::new(),
            favorites: Vec::new(),
            recents: Vec::new(),
            cache_stats: CacheStats::default(),
            primary_tabs: Vec::new(),
            active_primary_ix: 0,
            split_enabled: false,
            split_tabs: Vec::new(),
            active_split_ix: 0,
            focused_pane: PaneSlot::Primary,
            _subscriptions: subs,
        };

        if let Some(profile) = initial_profile {
            shell.activate_instance(profile, window, cx);
        }

        shell
    }

    fn activate_instance(
        &mut self,
        profile: InstanceProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = match self.services.client_for(&profile) {
            Ok(c) => c,
            Err(err) => {
                window.push_notification(Notification::error(err.to_string()), cx);
                return;
            }
        };

        self.active_profile = Some(profile.clone());
        self.active_client = Some(client.clone());
        self.primary_tabs.clear();
        self.split_tabs.clear();
        self.active_primary_ix = 0;
        self.active_split_ix = 0;
        let (mode, lat) = self.services.realtime.transport_status(&profile.id);
        self.transport_mode = mode;
        self.transport_latency_ms = lat;

        self.refresh_sidebar_lists(cx);

        // Warm table catalog in background, then refresh sidebar tables!
        let services = self.services.clone();
        let client_bg = client.clone();
        let (done_tx, done_rx) = async_channel::bounded(1);
        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let _ = services.ensure_table_hierarchy(&client_bg).await;
            let _ = done_tx.send(()).await;
        });

        cx.spawn_in(window, async move |this, cx| {
            if done_rx.recv().await.is_ok() {
                let _ = this.update(cx, |shell, cx| {
                    shell.refresh_sidebar_lists(cx);
                    cx.notify();
                });
            }
        })
        .detach();

        // Open default `incident` Classic List View immediately (0ms SWR from cache if cached!)
        self.open_list_tab(
            "incident".to_string(),
            Some("Incidents".to_string()),
            String::new(),
            false,
            window,
            cx,
        );
        cx.notify();
    }

    fn refresh_sidebar_lists(&mut self, cx: &App) {
        let Some(profile) = &self.active_profile else {
            self.sidebar_tables.clear();
            self.favorites.clear();
            self.recents.clear();
            self.cache_stats = CacheStats::default();
            return;
        };

        let filter = self.sidebar_filter_input.read(cx).value().to_string();
        self.sidebar_tables = self
            .services
            .cache
            .search_tables(&profile.id, &filter, 30)
            .unwrap_or_default();
        self.favorites = self
            .services
            .cache
            .list_favorites(&profile.id)
            .unwrap_or_default();
        self.recents = self
            .services
            .cache
            .list_recent(&profile.id, 12)
            .unwrap_or_default();
        self.cache_stats = self
            .services
            .cache
            .cache_stats(Some(&profile.id))
            .unwrap_or_default();
    }

    fn handle_realtime_event(
        &mut self,
        event: RealtimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_inst) = self.active_profile.as_ref().map(|p| p.id.clone()) else {
            return;
        };

        if let RealtimeEvent::TransportChanged {
            ref instance_id,
            ref mode,
            latency_ms,
        } = event
            && *instance_id == active_inst
        {
            self.transport_mode = mode.clone();
            self.transport_latency_ms = latency_ms;
        }

        for tab in self.primary_tabs.iter().chain(self.split_tabs.iter()) {
            match tab {
                WorkspaceTab::List { entity, .. } => {
                    entity.update(cx, |panel, cx| panel.on_realtime_event(&event, window, cx));
                }
                WorkspaceTab::Form { entity, .. } => {
                    entity.update(cx, |panel, cx| panel.on_realtime_event(&event, window, cx));
                }
            }
        }
        cx.notify();
    }

    fn handle_workspace_command(
        &mut self,
        cmd: WorkspaceCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cmd {
            WorkspaceCommand::OpenList {
                table,
                label,
                encoded_query,
                in_split,
            } => {
                self.open_list_tab(table, label, encoded_query, in_split, window, cx);
            }
            WorkspaceCommand::OpenRecord {
                table,
                sys_id,
                seed_record,
                in_split,
            } => {
                let target_split = in_split || self.split_enabled;
                self.open_record_tab(table, sys_id, seed_record, target_split, window, cx);
            }
            WorkspaceCommand::OpenNewRecord { table, in_split } => {
                let target_split = in_split || self.split_enabled;
                self.open_new_record_tab(table, target_split, window, cx);
            }
            WorkspaceCommand::OpenRecordByNumber { table, number } => {
                self.lookup_and_open_record_by_number(table, number, window, cx);
            }
            WorkspaceCommand::SwitchInstance { profile } => {
                self.activate_instance(profile, window, cx);
                window.push_notification(Notification::success("Connected to instance"), cx);
            }
            WorkspaceCommand::InstanceDeleted { instance_id } => {
                if self.active_profile.as_ref().map(|p| &p.id) == Some(&instance_id) {
                    self.active_profile = None;
                    self.active_client = None;
                    self.primary_tabs.clear();
                    self.split_tabs.clear();
                }
                self.refresh_sidebar_lists(cx);
                cx.notify();
            }
            WorkspaceCommand::ShowInstanceModal => {
                self.open_instance_dialog(window, cx);
            }
            WorkspaceCommand::RecordSaved {
                record,
                close_after,
            } => {
                // Propagate freshly saved record to any open list views for instant table reflection!
                if let Some(inst_id) = self.active_profile.as_ref().map(|p| p.id.clone()) {
                    let synth = RealtimeEvent::ListDelta {
                        instance_id: inst_id,
                        table: record.table.clone(),
                        encoded_query: String::new(),
                        updated_records: vec![record.clone()],
                    };
                    for tab in self.primary_tabs.iter().chain(self.split_tabs.iter()) {
                        if let WorkspaceTab::List { entity, .. } = tab {
                            entity.update(cx, |p, cx| p.on_realtime_event(&synth, window, cx));
                        }
                    }
                }
                self.refresh_sidebar_lists(cx);
                if close_after {
                    self.close_active_tab(cx);
                }
                cx.notify();
            }
            WorkspaceCommand::ToastInfo(msg) => {
                window.push_notification(Notification::info(msg), cx);
            }
            WorkspaceCommand::ToastSuccess(msg) => {
                window.push_notification(Notification::success(msg), cx);
            }
            WorkspaceCommand::ToastError(msg) => {
                window.push_notification(Notification::error(msg), cx);
            }
            WorkspaceCommand::RefreshSidebar => {
                self.refresh_sidebar_lists(cx);
                cx.notify();
            }
        }
    }

    fn open_list_tab(
        &mut self,
        table: String,
        label: Option<String>,
        encoded_query: String,
        in_split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.active_client.clone() else {
            self.open_instance_dialog(window, cx);
            return;
        };

        let tab_id = format!("list:{table}:{encoded_query}");
        if in_split {
            self.split_enabled = true;
            if let Some(pos) = self.split_tabs.iter().position(|t| t.id() == tab_id) {
                self.active_split_ix = pos;
                self.focused_pane = PaneSlot::Split;
                cx.notify();
                return;
            }
        } else if let Some(pos) = self.primary_tabs.iter().position(|t| t.id() == tab_id) {
            self.active_primary_ix = pos;
            self.focused_pane = PaneSlot::Primary;
            cx.notify();
            return;
        }

        let services = self.services.clone();
        let tbl_recent = table.clone();
        let lbl_recent = label.clone().unwrap_or_else(|| table.clone());
        let entity = cx.new(|cx| {
            ListViewPanel::new(
                services,
                client.clone(),
                table,
                label,
                encoded_query,
                window,
                cx,
            )
        });

        let _ = self.services.cache.record_recent(
            &client.profile().id,
            &RecentItem::for_table(tbl_recent, lbl_recent, None),
        );
        self.refresh_sidebar_lists(cx);

        let tab = WorkspaceTab::List { id: tab_id, entity };
        if in_split {
            self.split_tabs.push(tab);
            self.active_split_ix = self.split_tabs.len() - 1;
            self.focused_pane = PaneSlot::Split;
        } else {
            self.primary_tabs.push(tab);
            self.active_primary_ix = self.primary_tabs.len() - 1;
            self.focused_pane = PaneSlot::Primary;
        }
        cx.notify();
    }

    fn open_record_tab(
        &mut self,
        table: String,
        sys_id: String,
        seed_record: Option<SnRecord>,
        in_split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.active_client.clone() else {
            self.open_instance_dialog(window, cx);
            return;
        };

        let tab_id = format!("form:{table}:{sys_id}");
        if let Some(pos) = self.primary_tabs.iter().position(|t| t.id() == tab_id) {
            self.active_primary_ix = pos;
            self.focused_pane = PaneSlot::Primary;
            cx.notify();
            return;
        }
        if let Some(pos) = self.split_tabs.iter().position(|t| t.id() == tab_id) {
            self.split_enabled = true;
            self.active_split_ix = pos;
            self.focused_pane = PaneSlot::Split;
            cx.notify();
            return;
        }

        let services = self.services.clone();
        let entity = cx.new(|cx| {
            FormViewPanel::open_existing(
                services,
                client.clone(),
                table.clone(),
                sys_id.clone(),
                seed_record.clone(),
                window,
                cx,
            )
        });

        let title = entity.read(cx).tab_title();
        let subtitle = seed_record.and_then(|r| r.summary_text());
        let _ = self.services.cache.record_recent(
            &client.profile().id,
            &RecentItem::for_record(table, sys_id, title, subtitle),
        );
        self.refresh_sidebar_lists(cx);

        let tab = WorkspaceTab::Form { id: tab_id, entity };
        if in_split {
            self.split_enabled = true;
            self.split_tabs.push(tab);
            self.active_split_ix = self.split_tabs.len() - 1;
            self.focused_pane = PaneSlot::Split;
        } else {
            self.primary_tabs.push(tab);
            self.active_primary_ix = self.primary_tabs.len() - 1;
            self.focused_pane = PaneSlot::Primary;
        }
        cx.notify();
    }

    fn open_new_record_tab(
        &mut self,
        table: String,
        in_split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.active_client.clone() else {
            self.open_instance_dialog(window, cx);
            return;
        };

        let tab_id = format!("new:{table}:{}", uuid::Uuid::new_v4());
        let services = self.services.clone();
        let entity = cx.new(|cx| FormViewPanel::open_new(services, client, table, window, cx));
        let tab = WorkspaceTab::Form { id: tab_id, entity };

        if in_split {
            self.split_enabled = true;
            self.split_tabs.push(tab);
            self.active_split_ix = self.split_tabs.len() - 1;
            self.focused_pane = PaneSlot::Split;
        } else {
            self.primary_tabs.push(tab);
            self.active_primary_ix = self.primary_tabs.len() - 1;
            self.focused_pane = PaneSlot::Primary;
        }
        cx.notify();
    }

    fn lookup_and_open_record_by_number(
        &mut self,
        table: String,
        number: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.active_client.clone() else {
            return;
        };
        let inst_id = client.profile().id.clone();

        // 1. Instant 0ms local cache check first!
        if let Ok(Some(cached_rec)) = self
            .services
            .cache
            .find_record_by_field(&inst_id, &table, "number", &number)
        {
            self.open_record_tab(
                table,
                cached_rec.sys_id.clone(),
                Some(cached_rec),
                self.split_enabled,
                window,
                cx,
            );
            return;
        }

        // 2. Network lookup via `number=<NUM>`
        let services = self.services.clone();
        let tbl_bg = table.clone();
        let num_bg = number.clone();
        let (tx, rx) = async_channel::bounded(1);
        services.tokio_handle.spawn(async move {
            let query = format!("number={num_bg}");
            let res = client.find_record_by_query(&tbl_bg, &query).await;
            let _ = tx.send(res).await;
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(res) = rx.recv().await {
                let _ = this.update_in(cx, |shell, window, cx| match res {
                    Ok(Some(rec)) => {
                        let _ = shell.services.cache.upsert_record(&inst_id, &rec);
                        shell.open_record_tab(
                            table,
                            rec.sys_id.clone(),
                            Some(rec),
                            shell.split_enabled,
                            window,
                            cx,
                        );
                    }
                    Ok(None) => {
                        window.push_notification(
                            Notification::warning(format!("No {table} record found for {number}")),
                            cx,
                        );
                    }
                    Err(e) => {
                        window.push_notification(Notification::error(e.to_string()), cx);
                    }
                });
            }
        })
        .detach();
    }

    fn active_tab(&self) -> Option<&WorkspaceTab> {
        match self.focused_pane {
            PaneSlot::Primary => self.primary_tabs.get(self.active_primary_ix),
            PaneSlot::Split if self.split_enabled => self
                .split_tabs
                .get(self.active_split_ix)
                .or_else(|| self.primary_tabs.get(self.active_primary_ix)),
            _ => self.primary_tabs.get(self.active_primary_ix),
        }
    }

    fn close_tab_at(&mut self, pane: PaneSlot, ix: usize, cx: &mut Context<Self>) {
        match pane {
            PaneSlot::Primary => {
                if ix < self.primary_tabs.len() {
                    self.primary_tabs.remove(ix);
                    if self.active_primary_ix >= self.primary_tabs.len() {
                        self.active_primary_ix = self.primary_tabs.len().saturating_sub(1);
                    }
                }
            }
            PaneSlot::Split => {
                if ix < self.split_tabs.len() {
                    self.split_tabs.remove(ix);
                    if self.active_split_ix >= self.split_tabs.len() {
                        self.active_split_ix = self.split_tabs.len().saturating_sub(1);
                    }
                    if self.split_tabs.is_empty() {
                        self.split_enabled = false;
                        self.focused_pane = PaneSlot::Primary;
                    }
                }
            }
        }
        cx.notify();
    }

    fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        let pane = self.focused_pane;
        let ix = match pane {
            PaneSlot::Primary => self.active_primary_ix,
            PaneSlot::Split => self.active_split_ix,
        };
        self.close_tab_at(pane, ix, cx);
    }

    pub fn open_instance_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let active_id = self.active_profile.as_ref().map(|p| p.id.clone());
        let modal_view = cx.new(|cx| ConnectModalView::new(services, active_id, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .w(px(820.0))
                .title("ServiceNow Instance Connections")
                .close_button(true)
                .overlay_closable(true)
                .child(modal_view.clone())
        });
    }

    pub fn open_command_palette_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let inst_id = self.active_profile.as_ref().map(|p| p.id.clone());
        let palette_view = cx.new(|cx| CommandPaletteView::new(services, inst_id, window, cx));
        let first_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_focus.replace(false) {
                let fh = palette_view.read(cx).input_focus_handle(cx);
                fh.focus(window, cx);
            }
            dialog
                .w(px(620.0))
                .margin_top(px(72.0))
                .title("Quick Jump & Commands")
                .close_button(true)
                .overlay_closable(true)
                .child(palette_view.clone())
        });
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let inst_label = self
            .active_profile
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Connect Instance".to_string());
        let inst_dot = self
            .active_profile
            .as_ref()
            .and_then(|p| parse_hex_color(&p.accent_hex))
            .unwrap_or(theme.muted_foreground);

        let (status_color, status_text) = if self.active_profile.is_none() {
            (theme.muted_foreground, "Disconnected".to_string())
        } else {
            let color: Hsla = match &self.transport_mode {
                RealtimeTransportMode::AmbWebSocket => rgb(0x34D399).into(),
                RealtimeTransportMode::RestDeltaPolling => rgb(0x38BDF8).into(),
                RealtimeTransportMode::Connecting => rgb(0xFBBF24).into(),
                RealtimeTransportMode::Offline { .. } => rgb(0xF87171).into(),
            };
            let txt = match self.transport_latency_ms {
                Some(ms) => format!("{} · {}ms", self.transport_mode.badge_label(), ms),
                None => self.transport_mode.badge_label().to_string(),
            };
            (color, txt)
        };

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .pr_3()
                .items_center()
                .justify_between()
                // Left: Sidebar toggle + Instance pill
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(
                            Button::new("toggle-sidebar-btn")
                                .ghost()
                                .xsmall()
                                .icon(IconName::PanelLeft)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sidebar_open = !this.sidebar_open;
                                    cx.notify();
                                })),
                        )
                        .child(
                            h_flex()
                                .id("instance-pill-btn")
                                .px_2()
                                .py_0p5()
                                .gap_1p5()
                                .items_center()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_instance_dialog(window, cx);
                                }))
                                .child(div().size(px(7.5)).rounded_full().bg(inst_dot))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .max_w(px(180.0))
                                        .truncate()
                                        .child(inst_label),
                                )
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                ),
                        ),
                )
                // Center: Command Palette Omnibox Pill
                .child(
                    h_flex()
                        .id("titlebar-omnibox-btn")
                        .w(px(340.0))
                        .px_2p5()
                        .py_0p5()
                        .justify_between()
                        .items_center()
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .border_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .hover(|s| s.border_color(theme.primary.opacity(0.6)))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_command_palette_dialog(window, cx);
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::new(IconName::Search)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Jump to table, INC number, or command…"),
                                ),
                        )
                        .child(
                            div()
                                .px_1()
                                .rounded(px(3.0))
                                .bg(theme.background)
                                .text_size(px(10.0))
                                .text_color(theme.muted_foreground)
                                .child("⌘K"),
                        ),
                )
                // Right: Live Realtime Telemetry Pill + Split + Theme
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(
                            h_flex()
                                .px_2()
                                .py_0p5()
                                .gap_1p5()
                                .items_center()
                                .rounded_full()
                                .bg(status_color.opacity(0.14))
                                .border_1()
                                .border_color(status_color.opacity(0.35))
                                .child(div().size(px(6.5)).rounded_full().bg(status_color))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_medium()
                                        .text_color(status_color)
                                        .child(status_text),
                                ),
                        )
                        .child(
                            Button::new("toggle-split-btn")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Columns2)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.split_enabled = !this.split_enabled;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("toggle-theme-btn")
                                .ghost()
                                .xsmall()
                                .icon(if theme.mode.is_dark() {
                                    IconName::Sun
                                } else {
                                    IconName::Moon
                                })
                                .on_click(|_, _, cx| {
                                    let next = if cx.theme().mode.is_dark() {
                                        ThemeMode::Light
                                    } else {
                                        ThemeMode::Dark
                                    };
                                    apply_snrust_theme(next, cx);
                                }),
                        ),
                ),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let kb_size = (self.cache_stats.db_bytes / 1024).max(1);

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .justify_between()
            .child(
                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .p_2p5()
                    .gap_2p5()
                    .child(
                        Input::new(&self.sidebar_filter_input)
                            .small()
                            .prefix(
                                Icon::new(IconName::Search)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .cleanable(true),
                    )
                    .child(
                        v_flex()
                            .id("sidebar-scroll-body")
                            .flex_1()
                            .overflow_y_scroll()
                            .gap_3()
                            // Pinned Favorites
                            .when(!self.favorites.is_empty(), |el| {
                                el.child(
                                    v_flex()
                                        .gap_0p5()
                                        .child(sidebar_section_header("FAVORITES", &theme))
                                        .children(
                                            self.favorites.iter().cloned().enumerate().map(
                                                |(ix, fav)| {
                                                    let fav_click = fav.clone();
                                                    h_flex()
                                                        .id(("fav-item", ix))
                                                        .px_2()
                                                        .py_1()
                                                        .gap_2()
                                                        .items_center()
                                                        .rounded(theme.radius)
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(theme.accent))
                                                        .on_click(cx.listener(
                                                            move |this, evt: &ClickEvent, window, cx| {
                                                                let split = evt.modifiers().platform;
                                                                match &fav_click.kind {
                                                                    FavoriteKind::Table {
                                                                        table,
                                                                        label,
                                                                        query,
                                                                    } => {
                                                                        this.open_list_tab(
                                                                            table.clone(),
                                                                            Some(label.clone()),
                                                                            query.clone(),
                                                                            split,
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    }
                                                                    FavoriteKind::Record {
                                                                        table,
                                                                        sys_id,
                                                                        ..
                                                                    } => {
                                                                        this.open_record_tab(
                                                                            table.clone(),
                                                                            sys_id.clone(),
                                                                            None,
                                                                            split || this.split_enabled,
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    }
                                                                }
                                                            },
                                                        ))
                                                        .child(
                                                            Icon::new(IconName::Star)
                                                                .xsmall()
                                                                .text_color(rgb(0xFBBF24)),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .truncate()
                                                                .child(fav.title),
                                                        )
                                                },
                                            ),
                                        ),
                                )
                            })
                            // Tables Catalog
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(sidebar_section_header("TABLES", &theme))
                                    .children(
                                        self.sidebar_tables.iter().cloned().enumerate().map(
                                            |(ix, tbl)| {
                                                let t_name = tbl.name.clone();
                                                let t_lbl = tbl.label.clone();
                                                let t_new = tbl.name.clone();
                                                h_flex()
                                                    .id(("sidebar-tbl", ix))
                                                    .px_2()
                                                    .py_1()
                                                    .justify_between()
                                                    .items_center()
                                                    .rounded(theme.radius)
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(theme.accent))
                                                    .on_click(cx.listener(
                                                        move |this, evt: &ClickEvent, window, cx| {
                                                            this.open_list_tab(
                                                                t_name.clone(),
                                                                Some(t_lbl.clone()),
                                                                String::new(),
                                                                evt.modifiers().platform,
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    ))
                                                    .child(
                                                        v_flex()
                                                            .overflow_hidden()
                                                            .child(
                                                                div()
                                                                    .text_xs()
                                                                    .font_medium()
                                                                    .truncate()
                                                                    .child(tbl.label),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_size(px(10.5))
                                                                    .font_family(
                                                                        theme
                                                                            .mono_font_family
                                                                            .clone(),
                                                                    )
                                                                    .text_color(
                                                                        theme.muted_foreground,
                                                                    )
                                                                    .truncate()
                                                                    .child(tbl.name),
                                                            ),
                                                    )
                                                    .child(
                                                        Button::new(("new-rec-side", ix))
                                                            .ghost()
                                                            .xsmall()
                                                            .icon(IconName::Plus)
                                                            .on_click(cx.listener(
                                                                move |this, _, window, cx| {
                                                                    cx.stop_propagation();
                                                                    this.open_new_record_tab(
                                                                        t_new.clone(),
                                                                        this.split_enabled,
                                                                        window,
                                                                        cx,
                                                                    );
                                                                },
                                                            )),
                                                    )
                                            },
                                        ),
                                    ),
                            )
                            // Recent Records
                            .when(!self.recents.is_empty(), |el| {
                                el.child(
                                    v_flex()
                                        .gap_0p5()
                                        .child(sidebar_section_header("RECENT", &theme))
                                        .children(
                                            self.recents
                                                .iter()
                                                .filter(|r| r.sys_id.is_some())
                                                .take(8)
                                                .cloned()
                                                .enumerate()
                                                .map(|(ix, rec)| {
                                                    let tbl = rec.table.clone();
                                                    let sid = rec.sys_id.clone().unwrap_or_default();
                                                    h_flex()
                                                        .id(("recent-rec", ix))
                                                        .px_2()
                                                        .py_1()
                                                        .gap_2()
                                                        .items_center()
                                                        .rounded(theme.radius)
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(theme.accent))
                                                        .on_click(cx.listener(
                                                            move |this, evt: &ClickEvent, window, cx| {
                                                                this.open_record_tab(
                                                                    tbl.clone(),
                                                                    sid.clone(),
                                                                    None,
                                                                    evt.modifiers().platform
                                                                        || this.split_enabled,
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(
                                                            Icon::new(IconName::Clock)
                                                                .xsmall()
                                                                .text_color(theme.muted_foreground),
                                                        )
                                                        .child(
                                                            v_flex()
                                                                .overflow_hidden()
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .font_family(
                                                                            theme
                                                                                .mono_font_family
                                                                                .clone(),
                                                                        )
                                                                        .truncate()
                                                                        .child(rec.title),
                                                                )
                                                                .when_some(
                                                                    rec.subtitle,
                                                                    |el, sub| {
                                                                        el.child(
                                                                            div()
                                                                                .text_size(px(10.5))
                                                                                .text_color(
                                                                                    theme
                                                                                        .muted_foreground,
                                                                                )
                                                                                .truncate()
                                                                                .child(sub),
                                                                        )
                                                                    },
                                                                ),
                                                        )
                                                }),
                                        ),
                                )
                            }),
                    ),
            )
            // Cache Telemetry Footer
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Cache: {} rows · {} KB",
                                self.cache_stats.record_count, kb_size
                            )),
                    )
                    .when(self.active_profile.is_some(), |el| {
                        el.child(
                            Button::new("purge-cache-btn")
                                .ghost()
                                .xsmall()
                                .icon(IconName::RotateCw)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(p) = &this.active_profile {
                                        let _ = this.services.cache.clear_instance_cache(&p.id);
                                        this.refresh_sidebar_lists(cx);
                                        window.push_notification(
                                            Notification::info(
                                                "Cleared cached records & schemas (custom layouts preserved)",
                                            ),
                                            cx,
                                        );
                                        cx.notify();
                                    }
                                })),
                        )
                    }),
            )
    }

    fn render_pane(
        &self,
        pane: PaneSlot,
        tabs: &[WorkspaceTab],
        active_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let is_pane_focused = self.focused_pane == pane;

        if tabs.is_empty() {
            return self.render_welcome_empty(cx).into_any_element();
        }

        let active_tab = tabs.get(active_ix).cloned();

        v_flex()
            .id(match pane {
                PaneSlot::Primary => "primary-pane-root",
                PaneSlot::Split => "split-pane-root",
            })
            .size_full()
            .bg(theme.background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.focused_pane != pane {
                        this.focused_pane = pane;
                        cx.notify();
                    }
                }),
            )
            // Sleek Tab Strip
            .child(
                h_flex()
                    .w_full()
                    .h(px(34.0))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .bg(theme.tab_bar)
                    .border_b_1()
                    .border_color(if is_pane_focused {
                        theme.border
                    } else {
                        theme.border.opacity(0.6)
                    })
                    .child(
                        h_flex()
                            .id(match pane {
                                PaneSlot::Primary => "primary-tabs-scroll",
                                PaneSlot::Split => "split-tabs-scroll",
                            })
                            .flex_1()
                            .overflow_x_scroll()
                            .gap_1()
                            .children(tabs.iter().enumerate().map(|(ix, tab)| {
                                let is_sel = ix == active_ix;
                                let dirty = tab.is_dirty(cx);
                                let title = tab.title(cx);
                                let icon = tab.icon();

                                h_flex()
                                    .id((
                                        match pane {
                                            PaneSlot::Primary => "ptab",
                                            PaneSlot::Split => "stab",
                                        },
                                        ix,
                                    ))
                                    .h(px(26.0))
                                    .px_2p5()
                                    .gap_1p5()
                                    .items_center()
                                    .rounded(theme.radius)
                                    .cursor_pointer()
                                    .bg(if is_sel {
                                        theme.background
                                    } else {
                                        theme.transparent
                                    })
                                    .border_1()
                                    .border_color(if is_sel {
                                        theme.border
                                    } else {
                                        theme.transparent
                                    })
                                    .hover(|s| s.bg(theme.secondary))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.focused_pane = pane;
                                        match pane {
                                            PaneSlot::Primary => this.active_primary_ix = ix,
                                            PaneSlot::Split => this.active_split_ix = ix,
                                        }
                                        cx.notify();
                                    }))
                                    .child(Icon::new(icon).xsmall().text_color(if is_sel {
                                        theme.primary
                                    } else {
                                        theme.muted_foreground
                                    }))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(if is_sel {
                                                FontWeight::MEDIUM
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .max_w(px(170.0))
                                            .truncate()
                                            .child(title),
                                    )
                                    .when(dirty, |el| {
                                        el.child(
                                            div().size(px(6.0)).rounded_full().bg(rgb(0xFBBF24)),
                                        )
                                    })
                                    .child(
                                        div()
                                            .id((
                                                match pane {
                                                    PaneSlot::Primary => "ptab-x",
                                                    PaneSlot::Split => "stab-x",
                                                },
                                                ix,
                                            ))
                                            .p_0p5()
                                            .rounded(px(3.0))
                                            .hover(|s| s.bg(theme.accent))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.close_tab_at(pane, ix, cx);
                                            }))
                                            .child(
                                                Icon::new(IconName::X)
                                                    .xsmall()
                                                    .text_color(theme.muted_foreground),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        Button::new(match pane {
                            PaneSlot::Primary => "add-tab-primary",
                            PaneSlot::Split => "add-tab-split",
                        })
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_command_palette_dialog(window, cx);
                        })),
                    ),
            )
            // Active Panel Content
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .map(|container| match active_tab {
                        Some(WorkspaceTab::List { entity, .. }) => container.child(entity),
                        Some(WorkspaceTab::Form { entity, .. }) => container.child(entity),
                        None => container,
                    }),
            )
            .into_any_element()
    }

    fn render_welcome_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let connected = self.active_profile.is_some();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .gap_4()
            .child(
                v_flex()
                    .max_w(px(460.0))
                    .p_6()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .bg(theme.sidebar)
                    .border_1()
                    .border_color(theme.border)
                    .items_center()
                    .text_center()
                    .child(
                        div()
                            .p_3()
                            .rounded_full()
                            .bg(theme.primary.opacity(0.15))
                            .child(Icon::new(IconName::Zap).large().text_color(theme.primary)),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child("snrust — Native 120fps ServiceNow Client"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if connected {
                                "Press ⌘K to jump to any table (incident.list), create a record (incident.do), or open a ticket number directly."
                            } else {
                                "Connect to your ServiceNow instance via OAuth 2.0 + PKCE (stored in macOS Keychain) or launch the built-in Live Demo Sandbox in one click."
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2p5()
                            .pt_1()
                            .child(if connected {
                                Button::new("empty-open-incidents")
                                    .primary()
                                    .icon(IconName::Table)
                                    .label("Open Incidents")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_list_tab(
                                            "incident".into(),
                                            Some("Incidents".into()),
                                            String::new(),
                                            false,
                                            window,
                                            cx,
                                        );
                                    }))
                            } else {
                                Button::new("empty-connect-oauth")
                                    .primary()
                                    .icon(IconName::KeyRound)
                                    .label("Connect ServiceNow / Demo Sandbox")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_instance_dialog(window, cx);
                                    }))
                            })
                            .child(
                                Button::new("empty-cmd-k")
                                    .secondary()
                                    .icon(IconName::Search)
                                    .label("Command Palette (⌘K)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_command_palette_dialog(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl Focusable for WorkspaceShell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let primary_pane = self.render_pane(
            PaneSlot::Primary,
            &self.primary_tabs,
            self.active_primary_ix,
            cx,
        );
        let split_pane = self
            .split_enabled
            .then(|| self.render_pane(PaneSlot::Split, &self.split_tabs, self.active_split_ix, cx));

        v_flex()
            .id("snrust-workspace-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.open_command_palette_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenInstanceManager, window, cx| {
                this.open_instance_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveActiveForm, window, cx| {
                if let Some(WorkspaceTab::Form { entity, .. }) = this.active_tab().cloned() {
                    entity.update(cx, |form, cx| form.save(false, window, cx));
                }
            }))
            .on_action(
                cx.listener(|this, _: &UpdateAndCloseActiveForm, window, cx| {
                    if let Some(WorkspaceTab::Form { entity, .. }) = this.active_tab().cloned() {
                        entity.update(cx, |form, cx| form.save(true, window, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &RefreshActiveView, window, cx| {
                match this.active_tab().cloned() {
                    Some(WorkspaceTab::List { entity, .. }) => {
                        entity.update(cx, |list, cx| list.force_refresh(window, cx));
                    }
                    Some(WorkspaceTab::Form { entity, .. }) => {
                        entity.update(cx, |form, cx| form.force_refresh(window, cx));
                    }
                    None => {}
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleCustomizeGrid, _, cx| {
                if let Some(WorkspaceTab::Form { entity, .. }) = this.active_tab().cloned() {
                    entity.update(cx, |form, cx| form.toggle_customize_grid(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSplitPane, _, cx| {
                this.split_enabled = !this.split_enabled;
                cx.notify();
            }))
            .on_action(|_: &ToggleThemeMode, _, cx| {
                let next = if cx.theme().mode.is_dark() {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                apply_snrust_theme(next, cx);
            })
            .on_action(cx.listener(|this, _: &CloseActiveTab, _, cx| {
                this.close_active_tab(cx);
            }))
            .on_action(cx.listener(|this, _: &NewRecordInActiveTable, window, cx| {
                let tbl = match this.active_tab() {
                    Some(WorkspaceTab::List { entity, .. }) => {
                        entity.read(cx).table_name().to_string()
                    }
                    Some(WorkspaceTab::Form { entity, .. }) => {
                        entity.read(cx).table_name().to_string()
                    }
                    None => "incident".to_string(),
                };
                this.open_new_record_tab(tbl, this.split_enabled, window, cx);
            }))
            .child(self.render_title_bar(cx))
            .child(
                div().flex_1().overflow_hidden().child(
                    h_resizable("main-shell-resizable")
                        .child(
                            resizable_panel()
                                .size(px(236.0))
                                .size_range(px(180.0)..px(380.0))
                                .visible(self.sidebar_open)
                                .child(self.render_sidebar(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(360.0)..px(4000.0))
                                .child(primary_pane),
                        )
                        .when_some(split_pane, |grp, sp| {
                            grp.child(
                                resizable_panel()
                                    .size(px(580.0))
                                    .size_range(px(360.0)..px(2400.0))
                                    .child(sp),
                            )
                        }),
                ),
            )
    }
}

fn sidebar_section_header(
    label: &'static str,
    theme: &gpui_kit::component::theme::Theme,
) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .text_size(px(10.0))
        .font_semibold()
        .text_color(theme.muted_foreground)
        .child(label)
}
