//! 120fps GPU-virtualized Classic List View (`ListViewPanel`) backed by `gpui-component` `DataTable`,
//! 0ms Stale-While-Revalidate (`SnCache`) rendering, live `sn-realtime` delta updates, visual filter
//! chips, raw ServiceNow encoded query bar, and customizable dictionary columns.

use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState,
};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _, Size, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::{ListRecordsParams, SnClient};
use sn_cache::FavoriteItem;
use sn_core::{
    EncodedQuery, FieldType, FilterClause, FilterOperator, SnRecord, SortSpec, TableSchema,
};
use sn_realtime::{RealtimeEvent, SubscriptionGuard, WatchTarget};

use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::{priority_badge_colors, state_badge_colors};

const PAGE_LIMIT: usize = 100;

/// Delegate feeding virtualized rows and semantic ServiceNow cells into `DataTable`.
pub struct SnListTableDelegate {
    services: AppServices,
    table: String,
    columns: Vec<Column>,
    column_keys: Vec<String>,
    records: Vec<SnRecord>,
    schema: Option<TableSchema>,
    sort_tx: async_channel::Sender<(String, bool)>,
}

impl SnListTableDelegate {
    fn new(
        services: AppServices,
        table: String,
        column_keys: Vec<String>,
        schema: Option<TableSchema>,
        records: Vec<SnRecord>,
        active_sort: Option<&SortSpec>,
        sort_tx: async_channel::Sender<(String, bool)>,
    ) -> Self {
        let mut this = Self {
            services,
            table,
            columns: Vec::new(),
            column_keys: Vec::new(),
            records,
            schema,
            sort_tx,
        };
        this.rebuild_columns(column_keys, active_sort);
        this
    }

    fn rebuild_columns(&mut self, keys: Vec<String>, active_sort: Option<&SortSpec>) {
        self.column_keys = keys;
        self.columns = self
            .column_keys
            .iter()
            .map(|key| {
                let label = self
                    .schema
                    .as_ref()
                    .and_then(|s| s.fields.get(key))
                    .map(|f| f.column_label.clone())
                    .unwrap_or_else(|| prettify_column_key(key));

                let width = match key.as_str() {
                    "number" => px(128.0),
                    "short_description" | "description" => px(340.0),
                    "priority" | "state" | "incident_state" => px(136.0),
                    "active" => px(84.0),
                    "sys_updated_on" | "sys_created_on" => px(162.0),
                    _ => px(165.0),
                };

                let mut col = Column::new(key.clone(), label).sortable();
                col.width = width;
                col.min_width = px(68.0);
                if let Some(s) = active_sort
                    && s.field == *key
                {
                    col.sort = Some(if s.descending {
                        ColumnSort::Descending
                    } else {
                        ColumnSort::Ascending
                    });
                }
                col
            })
            .collect();
    }

    fn move_column(&mut self, from: usize, to: usize) -> Option<Vec<String>> {
        if from >= self.column_keys.len() || to >= self.column_keys.len() || from == to {
            return None;
        }
        let k = self.column_keys.remove(from);
        self.column_keys.insert(to, k);
        let c = self.columns.remove(from);
        self.columns.insert(to, c);
        Some(self.column_keys.clone())
    }
}

impl TableDelegate for SnListTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.records.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns
            .get(col_ix)
            .cloned()
            .unwrap_or_else(|| Column::new("col", ""))
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        if let Some(key) = self.column_keys.get(col_ix).cloned() {
            let desc = matches!(sort, ColumnSort::Descending);
            let _ = self.sort_tx.try_send((key, desc));
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let rec = self.records.get(row_ix)?;
        let key = self.column_keys.get(col_ix)?;
        Some(rec.get_display(key).to_string().into())
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(rec) = self.records.get(row_ix) else {
            return div().into_any_element();
        };
        let Some(key) = self.column_keys.get(col_ix) else {
            return div().into_any_element();
        };

        let theme = cx.theme().clone();
        let disp = rec.get_display(key).to_string();
        let raw = rec.get_raw(key).to_string();

        // 1. Primary record identifier (`number` or first column): crisp monospace link!
        if key == "number" || (col_ix == 0 && !rec.sys_id.is_empty()) {
            let services = self.services.clone();
            let tbl = self.table.clone();
            let sid = rec.sys_id.clone();
            let seed = rec.clone();
            let text = if disp.is_empty() {
                rec.primary_identifier()
            } else {
                disp
            };

            return h_flex()
                .id(("open-rec-cell", row_ix * 100 + col_ix))
                .h_full()
                .items_center()
                .cursor_pointer()
                .on_click(move |evt: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    services.send_cmd(WorkspaceCommand::OpenRecord {
                        table: tbl.clone(),
                        sys_id: sid.clone(),
                        seed_record: Some(seed.clone()),
                        in_split: evt.modifiers().platform,
                    });
                })
                .child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(theme.primary.opacity(0.12))
                        .hover(|s| s.bg(theme.primary.opacity(0.24)))
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .font_medium()
                        .text_color(theme.primary)
                        .child(text),
                )
                .into_any_element();
        }

        // 2. Priority semantic badge
        if key == "priority" && !disp.is_empty() {
            let (bg, border, fg) = priority_badge_colors(&disp, cx);
            return h_flex()
                .h_full()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(bg)
                        .border_1()
                        .border_color(border)
                        .text_size(px(11.0))
                        .font_medium()
                        .text_color(fg)
                        .child(disp),
                )
                .into_any_element();
        }

        // 3. State / Lifecycle semantic badge
        if matches!(key.as_str(), "state" | "incident_state") && !disp.is_empty() {
            let (bg, border, fg) = state_badge_colors(&disp, cx);
            return h_flex()
                .h_full()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(bg)
                        .border_1()
                        .border_color(border)
                        .text_size(px(11.0))
                        .font_medium()
                        .text_color(fg)
                        .child(disp),
                )
                .into_any_element();
        }

        // 4. Boolean status pill
        let field_type = self
            .schema
            .as_ref()
            .and_then(|s| s.fields.get(key))
            .map(|f| f.field_type.clone());

        if key == "active" || matches!(field_type, Some(FieldType::Boolean)) {
            let truthy = raw == "true" || raw == "1" || disp.eq_ignore_ascii_case("true");
            let dot_col: Hsla = if truthy {
                rgb(0x34D399).into()
            } else {
                theme.muted_foreground.opacity(0.45)
            };
            return h_flex()
                .h_full()
                .gap_1p5()
                .items_center()
                .child(div().size(px(7.0)).rounded_full().bg(dot_col))
                .child(
                    div()
                        .text_xs()
                        .text_color(if truthy {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(if truthy { "True" } else { "False" }),
                )
                .into_any_element();
        }

        // 5. Reference field link (`caller_id`, `assigned_to`, `cmdb_ci`, etc.)
        if let Some(FieldType::Reference { target_table }) = field_type
            && !raw.is_empty()
            && !disp.is_empty()
        {
            let services = self.services.clone();
            return h_flex()
                .id(("ref-link-cell", row_ix * 100 + col_ix))
                .h_full()
                .items_center()
                .cursor_pointer()
                .on_click(move |evt: &ClickEvent, _, cx| {
                    if evt.modifiers().platform {
                        cx.stop_propagation();
                        services.send_cmd(WorkspaceCommand::OpenRecord {
                            table: target_table.clone(),
                            sys_id: raw.clone(),
                            seed_record: None,
                            in_split: true,
                        });
                    }
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .hover(|s| s.text_color(theme.primary))
                        .truncate()
                        .child(disp),
                )
                .into_any_element();
        }

        div()
            .h_full()
            .flex()
            .items_center()
            .text_xs()
            .text_color(if key.starts_with("sys_") {
                theme.muted_foreground
            } else {
                theme.foreground
            })
            .truncate()
            .child(disp)
            .into_any_element()
    }
}

pub struct ListViewPanel {
    services: AppServices,
    client: SnClient,
    table: String,
    label: String,
    schema: Option<TableSchema>,
    column_keys: Vec<String>,

    encoded_query: EncodedQuery,
    offset: usize,
    total_count: Option<usize>,
    records: Vec<SnRecord>,

    is_loading_network: bool,
    served_from_cache_ms: Option<f64>,
    network_sync_ms: Option<u64>,
    show_raw_query_bar: bool,
    is_favorited: bool,

    quick_search_input: Entity<InputState>,
    raw_query_input: Entity<InputState>,
    table_state: Entity<TableState<SnListTableDelegate>>,
    _realtime_guard: Option<SubscriptionGuard>,
    _subs: Vec<Subscription>,
}

impl ListViewPanel {
    pub fn new(
        services: AppServices,
        client: SnClient,
        table: String,
        label: Option<String>,
        initial_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let resolved_label = label.unwrap_or_else(|| prettify_column_key(&table));
        let inst_id = client.profile().id.clone();

        // Instant Tier-1 & Tier-2 synchronous cache lookup (<1ms SWR paint!)
        let swr_started = Instant::now();
        let cached_schema = services
            .cache
            .get_table_schema(&inst_id, &table)
            .ok()
            .flatten()
            .map(|c| c.data);

        let custom_cols = services
            .cache
            .get_list_columns(&inst_id, &table)
            .ok()
            .flatten();

        let column_keys = custom_cols
            .or_else(|| {
                cached_schema
                    .as_ref()
                    .map(|s| s.default_list_columns.clone())
            })
            .unwrap_or_else(|| {
                vec![
                    "number".into(),
                    "short_description".into(),
                    "priority".into(),
                    "state".into(),
                    "assigned_to".into(),
                    "sys_updated_on".into(),
                ]
            });

        let parsed_query = EncodedQuery::parse(&initial_query);
        let canonical_q = parsed_query.to_encoded_string();
        let cached_page = services
            .cache
            .get_list_page(&inst_id, &table, &canonical_q, 0, PAGE_LIMIT)
            .ok()
            .flatten();

        let (initial_records, initial_total, cache_ms) = match cached_page {
            Some(item) => (
                item.data.records,
                item.data.total_count,
                Some(swr_started.elapsed().as_secs_f64() * 1000.0),
            ),
            None => (Vec::new(), None, None),
        };

        let (sort_tx, sort_rx) = async_channel::bounded::<(String, bool)>(8);
        let delegate = SnListTableDelegate::new(
            services.clone(),
            table.clone(),
            column_keys.clone(),
            cached_schema.clone(),
            initial_records.clone(),
            parsed_query.sorts.first(),
            sort_tx,
        );

        let table_state = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true)
        });

        let quick_search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by keyword (↵ to apply)…"));
        let raw_query_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(canonical_q.clone())
                .placeholder(
                    "ServiceNow Encoded Query, e.g. active=true^priority<=2^ORDERBYDESCnumber",
                )
        });

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &quick_search_input,
            window,
            |this: &mut Self, _, evt, window, cx| {
                if matches!(evt, InputEvent::PressEnter { .. }) {
                    this.apply_quick_search(window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &raw_query_input,
            window,
            |this: &mut Self, _, evt, window, cx| {
                if matches!(evt, InputEvent::PressEnter { .. }) {
                    let raw = this.raw_query_input.read(cx).value().to_string();
                    this.set_encoded_query_str(&raw, window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &table_state,
            window,
            |this: &mut Self, _, evt: &TableEvent, _, cx| match evt {
                TableEvent::DoubleClickedRow(row_ix) => {
                    if let Some(rec) = this.records.get(*row_ix).cloned() {
                        this.services.send_cmd(WorkspaceCommand::OpenRecord {
                            table: this.table.clone(),
                            sys_id: rec.sys_id.clone(),
                            seed_record: Some(rec),
                            in_split: false,
                        });
                    }
                }
                TableEvent::MoveColumn(from, to) => {
                    let updated_cols = this.table_state.update(cx, |ts, cx| {
                        let out = ts.delegate_mut().move_column(*from, *to);
                        ts.refresh(cx);
                        out
                    });
                    if let Some(cols) = updated_cols {
                        this.column_keys = cols.clone();
                        let _ = this.services.cache.save_list_columns(
                            &this.client.profile().id,
                            &this.table,
                            &cols,
                        );
                        cx.notify();
                    }
                }
                _ => {}
            },
        ));

        cx.spawn_in(window, async move |this, cx| {
            while let Ok((field, desc)) = sort_rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    panel.encoded_query.sorts = vec![SortSpec {
                        field,
                        descending: desc,
                    }];
                    panel.offset = 0;
                    let q_str = panel.encoded_query.to_encoded_string();
                    panel
                        .raw_query_input
                        .update(cx, |s, cx| s.set_value(q_str, window, cx));
                    panel.reload_data(false, window, cx);
                });
            }
        })
        .detach();

        let fav_id = format!("fav:list:{table}:{canonical_q}");
        let is_favorited = services
            .cache
            .list_favorites(&inst_id)
            .unwrap_or_default()
            .iter()
            .any(|f| f.id == fav_id);

        let mut panel = Self {
            services,
            client,
            table,
            label: resolved_label,
            schema: cached_schema,
            column_keys,
            encoded_query: parsed_query,
            offset: 0,
            total_count: initial_total,
            records: initial_records,
            is_loading_network: false,
            served_from_cache_ms: cache_ms,
            network_sync_ms: None,
            show_raw_query_bar: false,
            is_favorited,
            quick_search_input,
            raw_query_input,
            table_state,
            _realtime_guard: None,
            _subs: subs,
        };

        panel.reload_data(false, window, cx);
        panel
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }

    pub fn tab_title(&self) -> String {
        match self.total_count {
            Some(cnt) => format!("{} ({cnt})", self.label),
            None => self.label.clone(),
        }
    }

    pub fn force_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_data(true, window, cx);
    }

    pub fn on_realtime_event(
        &mut self,
        event: &RealtimeEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let my_inst = &self.client.profile().id;
        match event {
            RealtimeEvent::RecordUpdated {
                instance_id,
                table,
                sys_id,
                record,
                ..
            } if instance_id == my_inst && table == &self.table => {
                if let Some(existing) = self.records.iter_mut().find(|r| r.sys_id == *sys_id) {
                    existing.merge_from(record);
                    self.sync_delegate_rows(cx);
                    cx.notify();
                }
            }
            RealtimeEvent::ListDelta {
                instance_id,
                table,
                updated_records,
                ..
            } if instance_id == my_inst && table == &self.table => {
                let mut changed = false;
                for incoming in updated_records {
                    if let Some(existing) = self
                        .records
                        .iter_mut()
                        .find(|r| r.sys_id == incoming.sys_id)
                    {
                        existing.merge_from(incoming);
                        changed = true;
                    } else if self.offset == 0 {
                        self.records.insert(0, incoming.clone());
                        if let Some(c) = &mut self.total_count {
                            *c += 1;
                        }
                        changed = true;
                    }
                }
                if changed {
                    self.sync_delegate_rows(cx);
                    cx.notify();
                }
            }
            RealtimeEvent::RecordDeleted {
                instance_id,
                table,
                sys_id,
            } if instance_id == my_inst && table == &self.table => {
                let before = self.records.len();
                self.records.retain(|r| r.sys_id != *sys_id);
                if self.records.len() != before {
                    self.sync_delegate_rows(cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn sync_delegate_rows(&mut self, cx: &mut Context<Self>) {
        let rows = self.records.clone();
        let schema = self.schema.clone();
        let cols = self.column_keys.clone();
        let sort = self.encoded_query.sorts.first().cloned();
        self.table_state.update(cx, |ts, cx| {
            let d = ts.delegate_mut();
            d.schema = schema;
            d.records = rows;
            d.rebuild_columns(cols, sort.as_ref());
            ts.refresh(cx);
        });
    }

    fn apply_quick_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let term = self.quick_search_input.read(cx).value().trim().to_string();
        let search_field = self
            .schema
            .as_ref()
            .and_then(|s| {
                s.fields
                    .contains_key("short_description")
                    .then_some("short_description".to_string())
                    .or_else(|| Some(s.display_field.clone()))
            })
            .unwrap_or_else(|| "short_description".to_string());

        self.encoded_query
            .clauses
            .retain(|c| !(c.field == search_field && c.operator == FilterOperator::Contains));

        if !term.is_empty() {
            self.encoded_query.clauses.push(FilterClause::new(
                search_field,
                FilterOperator::Contains,
                term,
            ));
        }

        self.offset = 0;
        let q = self.encoded_query.to_encoded_string();
        self.raw_query_input
            .update(cx, |s, cx| s.set_value(q, window, cx));
        self.reload_data(false, window, cx);
    }

    fn set_encoded_query_str(&mut self, q_str: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.encoded_query = EncodedQuery::parse(q_str);
        self.offset = 0;
        let canonical = self.encoded_query.to_encoded_string();
        self.raw_query_input
            .update(cx, |s, cx| s.set_value(canonical, window, cx));
        self.reload_data(false, window, cx);
    }

    fn reload_data(&mut self, force_schema: bool, window: &mut Window, cx: &mut Context<Self>) {
        let inst_id = self.client.profile().id.clone();
        let q_str = self.encoded_query.to_encoded_string();

        // 1. Check SWR cache for instant paint on filter/page switches!
        let swr_t0 = Instant::now();
        if let Ok(Some(cached)) = self.services.cache.get_list_page(
            &inst_id,
            &self.table,
            &q_str,
            self.offset,
            PAGE_LIMIT,
        ) {
            self.records = cached.data.records;
            self.total_count = cached.data.total_count;
            self.served_from_cache_ms = Some(swr_t0.elapsed().as_secs_f64() * 1000.0);
            self.sync_delegate_rows(cx);
        }

        self.is_loading_network = true;
        cx.notify();

        let services = self.services.clone();
        let client = self.client.clone();
        let table = self.table.clone();
        let offset = self.offset;
        let current_custom_cols = services
            .cache
            .get_list_columns(&inst_id, &table)
            .ok()
            .flatten();
        let fallback_cols = self.column_keys.clone();

        let (tx, rx) =
            async_channel::bounded::<Result<(TableSchema, sn_core::ListPage, u64), String>>(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let net_t0 = Instant::now();
            let schema = match services
                .ensure_table_schema(&client, &table, force_schema)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            let fetch_cols = current_custom_cols.unwrap_or_else(|| {
                if !schema.default_list_columns.is_empty() {
                    schema.default_list_columns.clone()
                } else {
                    fallback_cols
                }
            });

            let params = ListRecordsParams {
                encoded_query: q_str.clone(),
                fields: fetch_cols,
                offset,
                limit: PAGE_LIMIT,
                include_total_count: true,
            };

            match client.list_records(&table, &params).await {
                Ok(page) => {
                    let elapsed_ms = net_t0.elapsed().as_millis() as u64;
                    let _ = services
                        .cache
                        .save_list_page_async(client.profile().id.clone(), page.clone())
                        .await;
                    let _ = tx.send(Ok((schema, page, elapsed_ms))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(res) = rx.recv().await {
                let _ = this.update(cx, |panel, cx| {
                    panel.is_loading_network = false;
                    match res {
                        Ok((schema, page, net_ms)) => {
                            if panel
                                .services
                                .cache
                                .get_list_columns(&panel.client.profile().id, &panel.table)
                                .ok()
                                .flatten()
                                .is_none()
                                && !schema.default_list_columns.is_empty()
                            {
                                panel.column_keys = schema.default_list_columns.clone();
                            }
                            panel.label = schema.label.clone();
                            panel.schema = Some(schema);
                            panel.total_count = page.total_count;
                            panel.records = page.records;
                            panel.network_sync_ms = Some(net_ms);
                            panel.sync_delegate_rows(cx);

                            // Register live delta watch for this list!
                            panel._realtime_guard = Some(panel.services.realtime.watch(
                                panel.client.clone(),
                                WatchTarget::List {
                                    table: panel.table.clone(),
                                    encoded_query: panel.encoded_query.to_encoded_string(),
                                    fields: panel.column_keys.clone(),
                                    high_water_updated_on: page.max_updated_on,
                                },
                            ));
                            panel.services.send_cmd(WorkspaceCommand::RefreshSidebar);
                        }
                        Err(err_msg) => {
                            panel
                                .services
                                .send_cmd(WorkspaceCommand::ToastError(format!(
                                    "Failed syncing {}: {err_msg}",
                                    panel.table
                                )));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        let inst_id = &self.client.profile().id;
        let q_str = self.encoded_query.to_encoded_string();
        let fav_id = format!("fav:list:{}:{q_str}", self.table);

        if self.is_favorited {
            let _ = self.services.cache.remove_favorite(inst_id, &fav_id);
            self.is_favorited = false;
        } else {
            let title = if q_str.is_empty() {
                self.label.clone()
            } else {
                format!("{} · Filtered", self.label)
            };
            let item = FavoriteItem::for_table(
                fav_id,
                self.table.clone(),
                title,
                q_str.clone(),
                (!q_str.is_empty()).then_some(q_str),
            );
            let _ = self.services.cache.upsert_favorite(inst_id, &item);
            self.is_favorited = true;
        }
        self.services.send_cmd(WorkspaceCommand::RefreshSidebar);
        cx.notify();
    }

    fn open_column_chooser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let panel_entity = cx.entity().clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme().clone();
            let active_cols = panel_entity.read(cx).column_keys.clone();
            let mut entries: Vec<_> = schema.fields.values().cloned().collect();
            entries.sort_by(|a, b| a.column_label.cmp(&b.column_label));

            let panel_reset = panel_entity.clone();

            dialog
                .w(px(500.0))
                .title(format!("Configure List Columns — {}", schema.label))
                .close_button(true)
                .overlay_closable(true)
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Toggle columns below, or drag column headers directly in the table to reorder."),
                                )
                                .child(
                                    Button::new("reset-list-cols")
                                        .secondary()
                                        .xsmall()
                                        .label("Reset Defaults")
                                        .on_click(move |_, window, cx| {
                                            panel_reset.update(cx, |p, cx| {
                                                if let Some(s) = &p.schema {
                                                    p.column_keys = s.default_list_columns.clone();
                                                    let _ = p.services.cache.save_list_columns(
                                                        &p.client.profile().id,
                                                        &p.table,
                                                        &p.column_keys,
                                                    );
                                                    p.reload_data(false, window, cx);
                                                }
                                            });
                                        }),
                                ),
                        )
                        .child(
                            v_flex()
                                .id("col-chooser-scroll")
                                .max_h(px(340.0))
                                .overflow_y_scroll()
                                .gap_1()
                                .children(entries.into_iter().enumerate().map(|(ix, entry)| {
                                    let checked = active_cols.contains(&entry.element);
                                    let elem_name = entry.element.clone();
                                    let panel_toggle = panel_entity.clone();
                                    h_flex()
                                        .id(("col-opt-row", ix))
                                        .px_2p5()
                                        .py_1p5()
                                        .justify_between()
                                        .items_center()
                                        .rounded(theme.radius)
                                        .hover(|s| s.bg(theme.accent))
                                        .child(
                                            Checkbox::new(("col-chk", ix))
                                                .checked(checked)
                                                .label(format!(
                                                    "{} ({})",
                                                    entry.column_label, entry.element
                                                ))
                                                .on_click(move |next_checked, window, cx| {
                                                    panel_toggle.update(cx, |p, cx| {
                                                        if *next_checked {
                                                            if !p.column_keys.contains(&elem_name) {
                                                                p.column_keys
                                                                    .push(elem_name.clone());
                                                            }
                                                        } else if p.column_keys.len() > 1 {
                                                            p.column_keys
                                                                .retain(|k| k != &elem_name);
                                                        }
                                                        let _ = p.services.cache.save_list_columns(
                                                            &p.client.profile().id,
                                                            &p.table,
                                                            &p.column_keys,
                                                        );
                                                        p.reload_data(false, window, cx);
                                                    });
                                                }),
                                        )
                                })),
                        ),
                )
        });
    }
}

impl Render for ListViewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let active_q = self.encoded_query.filter_only_string();

        let range_start = if self.records.is_empty() {
            0
        } else {
            self.offset + 1
        };
        let range_end = self.offset + self.records.len();
        let total_txt = self
            .total_count
            .map(|t| format!("of {t}"))
            .unwrap_or_default();

        v_flex()
            .size_full()
            .bg(theme.background)
            // Top Action & Quick Filter Header
            .child(
                v_flex()
                    .px_3p5()
                    .py_2p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar.opacity(0.45))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            // Left: Table Title + table identifier pill + Favorite star
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div().text_base().font_semibold().child(self.label.clone()),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(4.0))
                                            .bg(theme.secondary)
                                            .font_family(theme.mono_font_family.clone())
                                            .text_size(px(11.0))
                                            .text_color(theme.muted_foreground)
                                            .child(self.table.clone()),
                                    )
                                    .child(
                                        Button::new("fav-list-toggle")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::Star)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_favorite(cx);
                                            })),
                                    ),
                            )
                            // Right: Search + Columns + Query Toggle + Refresh + New Record
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(
                                        div().w(px(230.0)).child(
                                            Input::new(&self.quick_search_input)
                                                .small()
                                                .prefix(
                                                    Icon::new(IconName::Search)
                                                        .xsmall()
                                                        .text_color(theme.muted_foreground),
                                                )
                                                .cleanable(true),
                                        ),
                                    )
                                    .child(
                                        Button::new("toggle-encoded-query-bar")
                                            .secondary()
                                            .small()
                                            .icon(IconName::Funnel)
                                            .label("Query")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_raw_query_bar = !this.show_raw_query_bar;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("open-col-chooser")
                                            .secondary()
                                            .small()
                                            .icon(IconName::SlidersHorizontal)
                                            .label("Columns")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_column_chooser(window, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("refresh-list-btn")
                                            .ghost()
                                            .small()
                                            .icon(IconName::RotateCw)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.force_refresh(window, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("new-record-list-btn")
                                            .primary()
                                            .small()
                                            .icon(IconName::Plus)
                                            .label("New")
                                            .on_click(cx.listener(
                                                |this, evt: &ClickEvent, _, _| {
                                                    this.services.send_cmd(
                                                        WorkspaceCommand::OpenNewRecord {
                                                            table: this.table.clone(),
                                                            in_split: evt.modifiers().platform,
                                                        },
                                                    );
                                                },
                                            )),
                                    ),
                            ),
                    )
                    // Second bar: Preset Filter Pills + Interactive Filter Breadcrumb Chips
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .items_center()
                            .children(
                                [
                                    ("All", ""),
                                    ("Active", "active=true"),
                                    ("P1 / P2 High", "active=true^priority<=2"),
                                    ("New", "state=1"),
                                ]
                                .into_iter()
                                .enumerate()
                                .map(|(ix, (lbl, preset_q))| {
                                    let is_current = active_q == preset_q;
                                    let q_owned = preset_q.to_string();
                                    div()
                                        .id(("preset-pill", ix))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if is_current {
                                            theme.primary
                                        } else {
                                            theme.border
                                        })
                                        .bg(if is_current {
                                            theme.primary.opacity(0.16)
                                        } else {
                                            theme.secondary
                                        })
                                        .text_size(px(11.0))
                                        .font_medium()
                                        .text_color(if is_current {
                                            theme.primary
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.set_encoded_query_str(&q_owned, window, cx);
                                        }))
                                        .child(lbl)
                                }),
                            )
                            // Active clause chips with `x` removal
                            .children(self.encoded_query.clauses.iter().cloned().enumerate().map(
                                |(ix, clause)| {
                                    h_flex()
                                        .id(("filter-chip", ix))
                                        .px_2()
                                        .py_0p5()
                                        .gap_1()
                                        .items_center()
                                        .rounded(px(5.0))
                                        .bg(theme.accent)
                                        .border_1()
                                        .border_color(theme.primary.opacity(0.35))
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .font_family(theme.mono_font_family.clone())
                                                .child(format!(
                                                    "{} {} {}",
                                                    clause.field,
                                                    clause.operator.symbol_label(),
                                                    clause.value
                                                )),
                                        )
                                        .child(
                                            div()
                                                .id(("rm-chip", ix))
                                                .cursor_pointer()
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        if ix < this.encoded_query.clauses.len() {
                                                            this.encoded_query.clauses.remove(ix);
                                                            this.offset = 0;
                                                            let next_q = this
                                                                .encoded_query
                                                                .to_encoded_string();
                                                            this.raw_query_input.update(
                                                                cx,
                                                                |s, cx| {
                                                                    s.set_value(next_q, window, cx)
                                                                },
                                                            );
                                                            this.reload_data(false, window, cx);
                                                        }
                                                    },
                                                ))
                                                .child(
                                                    Icon::new(IconName::X)
                                                        .xsmall()
                                                        .text_color(theme.muted_foreground),
                                                ),
                                        )
                                },
                            )),
                    )
                    // Raw ServiceNow Encoded Query Bar (Power-User Mode)
                    .when(self.show_raw_query_bar, |el| {
                        el.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .pt_0p5()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child("sysparm_query="),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Input::new(&self.raw_query_input).small()),
                                )
                                .child(
                                    Button::new("run-raw-query-btn")
                                        .primary()
                                        .small()
                                        .label("Run Query")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let raw =
                                                this.raw_query_input.read(cx).value().to_string();
                                            this.set_encoded_query_str(&raw, window, cx);
                                        })),
                                ),
                        )
                    }),
            )
            // Main 120fps Virtualized DataTable
            .child(
                div().flex_1().overflow_hidden().child(
                    DataTable::new(&self.table_state)
                        .stripe(true)
                        .bordered(false)
                        .with_size(Size::Small),
                ),
            )
            // Bottom Telemetry & Pagination Status Bar
            .child(
                h_flex()
                    .px_3p5()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "Showing {range_start}–{range_end} {total_txt}"
                                    )),
                            )
                            .when_some(self.served_from_cache_ms, |el, ms| {
                                el.child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(rgb(0x34D399))
                                        .child(format!("⚡ SWR {:.2}ms", ms)),
                                )
                            })
                            .when_some(self.network_sync_ms, |el, ms| {
                                el.child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(format!("REST {ms}ms")),
                                )
                            })
                            .when(self.is_loading_network, |el| {
                                el.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.primary)
                                        .child("Syncing delta…"),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Button::new("list-prev-page")
                                    .secondary()
                                    .xsmall()
                                    .icon(IconName::ChevronLeft)
                                    .label("Prev")
                                    .disabled(self.offset == 0)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.offset = this.offset.saturating_sub(PAGE_LIMIT);
                                        this.reload_data(false, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("list-next-page")
                                    .secondary()
                                    .xsmall()
                                    .label("Next")
                                    .disabled(
                                        self.total_count
                                            .is_some_and(|tot| self.offset + PAGE_LIMIT >= tot)
                                            || self.records.len() < PAGE_LIMIT,
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.offset += PAGE_LIMIT;
                                        this.reload_data(false, window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

fn prettify_column_key(key: &str) -> String {
    key.split('_')
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, part)| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if i == 0 => {
                    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
                }
                _ => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
