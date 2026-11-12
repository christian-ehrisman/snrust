//! 120fps GPU-virtualized Classic List View (`ListViewPanel`) backed by `gpui-component` `DataTable`,
//! 0ms Stale-While-Revalidate (`SnCache`) rendering, live `sn-realtime` delta updates, visual filter
//! chips, raw ServiceNow encoded query bar, and customizable dictionary columns.

use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState,
};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::{ListRecordsParams, SnClient};
use sn_core::{
    ClassicTarget, Condition, EncodedQuery, FieldType, FilterClause, FilterOperator, SavedFilter,
    SnRecord, SortSpec, TableSchema,
};
use sn_realtime::{RealtimeEvent, SubscriptionGuard, WatchTarget};

use crate::column_panel::{ColumnPanel, ColumnPanelEvent};
use crate::favorites;
use crate::filter_builder::{FilterBuilder, FilterBuilderEvent};
use crate::prefs::AppPrefs;
use crate::reference_preview::{is_single_reference, reference_link};
use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::{priority_badge_colors, sp, state_badge_colors};

/// Delegate feeding virtualized rows and semantic ServiceNow cells into `DataTable`.
pub struct SnListTableDelegate {
    services: AppServices,
    client: SnClient,
    table: String,
    columns: Vec<Column>,
    column_keys: Vec<String>,
    records: Vec<SnRecord>,
    schema: Option<TableSchema>,
    sort_tx: async_channel::Sender<(String, bool)>,
    view: String,
}

impl SnListTableDelegate {
    /// Columns are empty until [`Self::rebuild_columns`] runs.
    fn new(
        services: AppServices,
        client: SnClient,
        table: String,
        view: String,
        schema: Option<TableSchema>,
        records: Vec<SnRecord>,
        sort_tx: async_channel::Sender<(String, bool)>,
    ) -> Self {
        Self {
            services,
            client,
            table,
            columns: Vec::new(),
            column_keys: Vec::new(),
            records,
            schema,
            sort_tx,
            view,
        }
    }

    fn rebuild_columns(
        &mut self,
        keys: Vec<String>,
        active_sort: Option<&SortSpec>,
        ui_scale: f32,
    ) {
        self.column_keys = keys;
        self.columns = self
            .column_keys
            .iter()
            .map(|key| {
                // Dot-walked keys are labelled through the cached schema of each
                // referenced table; until that is cached the key itself is shown.
                let label = self
                    .schema
                    .as_ref()
                    .and_then(|schema| {
                        schema.dot_walk_label(key, |table| {
                            self.services
                                .cache
                                .get_table_schema(&self.client.profile().id, table)
                                .ok()
                                .flatten()
                                .map(|cached| cached.data)
                        })
                    })
                    .unwrap_or_else(|| prettify_column_key(key));

                let width = match key.as_str() {
                    "number" => 128.0,
                    "short_description" | "description" => 340.0,
                    "priority" | "state" | "incident_state" => 136.0,
                    "active" => 84.0,
                    "sys_updated_on" | "sys_created_on" => 162.0,
                    _ => 165.0,
                };

                let mut col = Column::new(key.clone(), label).sortable();
                col.width = px(width * ui_scale);
                col.min_width = px(68.0 * ui_scale);
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
        let display = self
            .records
            .get(row_ix)
            .and_then(|r| {
                self.column_keys
                    .get(col_ix)
                    .map(|key| r.get_display(key).to_string())
            })
            .unwrap_or_default();
        let raw = self
            .records
            .get(row_ix)
            .and_then(|r| {
                self.column_keys
                    .get(col_ix)
                    .map(|key| r.get_raw(key).to_string())
            })
            .unwrap_or_default();
        h_flex()
            .size_full()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_cell(row_ix, col_ix, cx)),
            )
            .child(
                Button::new(("copy-cell", row_ix * self.columns.len() + col_ix))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .tooltip("Copy value (Option-click for raw value)")
                    .on_click(move |event, _, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            if event.modifiers().alt {
                                raw.clone()
                            } else {
                                display.clone()
                            },
                        ));
                    }),
            )
    }
}

impl SnListTableDelegate {
    fn render_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> AnyElement {
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
            let view = self.view.clone();
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
                    services.send_cmd(WorkspaceCommand::OpenTarget {
                        target: sn_core::ClassicTarget::Form {
                            table: tbl.clone(),
                            sys_id: sid.clone(),
                            query: String::new(),
                            view: view.clone(),
                        },
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
                        .text_size(sp(11.0))
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
                        .text_size(sp(11.0))
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
            && is_single_reference(&raw)
            && !disp.is_empty()
        {
            return h_flex()
                .h_full()
                .min_w_0()
                .items_center()
                .child(reference_link(
                    ("ref-link-cell", row_ix * self.columns.len() + col_ix),
                    &self.services,
                    &self.client,
                    &target_table,
                    &raw,
                    &disp,
                    false,
                    cx,
                ))
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

/// A breadcrumb chip for one filter row (its `^OR` alternatives are shown together).
struct FilterChip {
    group_ix: usize,
    row_ix: usize,
    text: String,
}

pub struct ListViewPanel {
    services: AppServices,
    client: SnClient,
    table: String,
    label: String,
    schema: Option<TableSchema>,
    column_keys: Vec<String>,
    view: String,
    request_generation: u64,

    encoded_query: EncodedQuery,
    offset: usize,
    /// Page size the current rows were fetched with; follows the preference.
    page_limit: usize,
    total_count: Option<usize>,
    records: Vec<SnRecord>,

    is_loading_network: bool,
    served_from_cache_ms: Option<f64>,
    network_sync_ms: Option<u64>,
    show_raw_query_bar: bool,
    filter_builder: Option<Entity<FilterBuilder>>,
    _filter_builder_sub: Option<Subscription>,
    /// One breadcrumb chip per filter condition, rebuilt when the filter or schema changes.
    chips: Vec<FilterChip>,
    /// Saved filters for this table, shown as pills.
    saved_filters: Vec<SavedFilter>,
    is_favorited: bool,
    column_panel: Option<Entity<ColumnPanel>>,
    _column_panel_sub: Option<Subscription>,

    quick_search_input: Entity<InputState>,
    raw_query_input: Entity<InputState>,
    table_state: Entity<TableState<SnListTableDelegate>>,
    _realtime_guard: Option<SubscriptionGuard>,
    _subs: Vec<Subscription>,
}

impl ListViewPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        services: AppServices,
        client: SnClient,
        table: String,
        label: Option<String>,
        initial_query: String,
        view: String,
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
            .get_list_columns(&inst_id, &sn_core::classic::view_cache_key(&table, &view))
            .ok()
            .flatten();

        let server_cols: Option<Vec<String>> = match services.cache.get_account_metadata(
            &inst_id,
            &format!("list:{}", sn_core::classic::view_cache_key(&table, &view)),
        ) {
            Ok(cols) => cols,
            Err(err) => {
                tracing::warn!(%err, "Cannot read synced list columns");
                None
            }
        };
        let column_keys = custom_cols
            .or(server_cols)
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
        let page_limit = AppPrefs::get(cx).list_page_size;
        let ui_scale = AppPrefs::get(cx).ui_scale;
        let cached_page = services
            .cache
            .get_list_page(&inst_id, &table, &canonical_q, 0, page_limit)
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
        let mut delegate = SnListTableDelegate::new(
            services.clone(),
            client.clone(),
            table.clone(),
            view.clone(),
            cached_schema.clone(),
            initial_records.clone(),
            sort_tx,
        );
        delegate.rebuild_columns(column_keys.clone(), parsed_query.sorts.first(), ui_scale);

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
                        this.services.send_cmd(WorkspaceCommand::OpenTarget {
                            target: sn_core::ClassicTarget::Form {
                                table: this.table.clone(),
                                sys_id: rec.sys_id.clone(),
                                query: String::new(),
                                view: this.view.clone(),
                            },
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
                        this.sync_column_panel(cx);
                        let _ = this.services.cache.save_list_columns(
                            &this.client.profile().id,
                            &sn_core::classic::view_cache_key(&this.table, &this.view),
                            &cols,
                        );
                        cx.notify();
                    }
                }
                _ => {}
            },
        ));

        subs.push(AppPrefs::observe_in(window, cx, |this, window, cx| {
            if this.page_limit != AppPrefs::get(cx).list_page_size {
                // Stay on the page that contains the first visible row.
                let limit = AppPrefs::get(cx).list_page_size;
                this.offset = this.offset / limit * limit;
                this.reload_data(false, window, cx);
            } else {
                // Column widths follow the UI scale.
                this.sync_delegate_rows(cx);
                cx.notify();
            }
        }));

        cx.spawn_in(window, async move |this, cx| {
            while let Ok((field, desc)) = sort_rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    panel.encoded_query.sorts = vec![SortSpec {
                        field,
                        descending: desc,
                    }];
                    panel.after_query_changed(true, window, cx);
                });
            }
        })
        .detach();

        let is_favorited = favorites::find(
            &services,
            &inst_id,
            &ClassicTarget::List {
                table: table.clone(),
                query: canonical_q.clone(),
                view: sn_core::classic::normalize_view(&view),
            },
        )
        .is_some();

        let mut panel = Self {
            services,
            client,
            table,
            label: resolved_label,
            schema: cached_schema,
            column_keys,
            view,
            request_generation: 0,
            encoded_query: parsed_query,
            offset: 0,
            page_limit,
            total_count: initial_total,
            records: initial_records,
            is_loading_network: false,
            served_from_cache_ms: cache_ms,
            network_sync_ms: None,
            show_raw_query_bar: false,
            filter_builder: None,
            _filter_builder_sub: None,
            chips: Vec::new(),
            saved_filters: Vec::new(),
            is_favorited,
            column_panel: None,
            _column_panel_sub: None,
            quick_search_input,
            raw_query_input,
            table_state,
            _realtime_guard: None,
            _subs: subs,
        };

        panel.saved_filters = panel.local_saved_filters();
        panel.rebuild_chips();
        panel.reload_data(false, window, cx);
        panel
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }

    #[cfg(test)]
    pub fn is_favorited(&self) -> bool {
        self.is_favorited
    }

    #[cfg(test)]
    pub fn records(&self) -> &[SnRecord] {
        &self.records
    }

    #[cfg(test)]
    pub fn column_keys(&self) -> &[String] {
        &self.column_keys
    }

    #[cfg(test)]
    pub fn instance_id(&self) -> sn_core::InstanceId {
        self.client.profile().id.clone()
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
        let ui_scale = AppPrefs::get(cx).ui_scale;
        self.table_state.update(cx, |ts, cx| {
            let d = ts.delegate_mut();
            d.view = self.view.clone();
            d.schema = schema;
            d.records = rows;
            d.rebuild_columns(cols, sort.as_ref(), ui_scale);
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

        // Replace the previous keyword rather than stacking another one.
        self.encoded_query.remove_clauses(|clause| {
            clause.field == search_field && clause.operator == FilterOperator::Contains
        });
        if !term.is_empty() {
            // Applied to every `^NQ` branch, so it narrows the whole result.
            self.encoded_query.and_clause(FilterClause::new(
                search_field,
                FilterOperator::Contains,
                term,
            ));
        }
        self.after_query_changed(true, window, cx);
    }

    fn set_encoded_query_str(&mut self, q_str: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.set_query(EncodedQuery::parse(q_str), window, cx);
    }

    pub fn set_query(&mut self, query: EncodedQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.encoded_query = query;
        self.after_query_changed(true, window, cx);
    }

    /// Brings everything derived from the filter back in step and reloads from the first
    /// page. `sync_builder` is false when the change came from the builder itself, whose
    /// in-progress rows must not be rebuilt under the user.
    fn after_query_changed(
        &mut self,
        sync_builder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.offset = 0;
        let canonical = self.encoded_query.to_encoded_string();
        self.raw_query_input
            .update(cx, |s, cx| s.set_value(canonical, window, cx));
        if sync_builder && let Some(builder) = self.filter_builder.clone() {
            let query = self.encoded_query.clone();
            builder.update(cx, |builder, cx| builder.set_query(&query, window, cx));
        }
        self.rebuild_chips();
        self.reload_data(false, window, cx);
    }

    /// Human-readable text for one condition: field label, operator, and the display value
    /// of choices and references where it is known.
    fn describe_condition(&self, condition: &Condition) -> String {
        let clause = match condition {
            Condition::Raw(raw) => return raw.clone(),
            Condition::Clause(clause) => clause,
        };
        let instance_id = self.client.profile().id.clone();
        let schema = self.schema.as_ref();
        let label = schema
            .and_then(|schema| {
                schema.dot_walk_label(&clause.field, |table| {
                    self.services
                        .cache
                        .get_table_schema(&instance_id, table)
                        .ok()
                        .flatten()
                        .map(|cached| cached.data)
                })
            })
            .unwrap_or_else(|| clause.field.clone());
        if !clause.operator.takes_value() {
            return format!("{label} {}", clause.operator.symbol_label());
        }
        let field_type = schema
            .and_then(|schema| schema.fields.get(&clause.field))
            .map(|field| field.field_type.clone());
        let value = clause
            .display_hint
            .clone()
            .filter(|hint| !hint.is_empty())
            .or_else(|| match field_type {
                Some(FieldType::Choice) => schema?
                    .choices_for(&clause.field, None)
                    .into_iter()
                    .find(|choice| choice.value == clause.value)
                    .map(|choice| choice.label.clone()),
                Some(FieldType::Reference { target_table }) => self
                    .services
                    .cache
                    .get_record(&instance_id, &target_table, &clause.value)
                    .ok()
                    .flatten()
                    .map(|cached| cached.data.primary_identifier()),
                _ => None,
            })
            .unwrap_or_else(|| clause.value.clone());
        format!("{label} {} {value}", clause.operator.symbol_label())
    }

    fn rebuild_chips(&mut self) {
        let mut chips = Vec::new();
        for (group_ix, group) in self.encoded_query.groups.iter().enumerate() {
            for (row_ix, row) in group.rows.iter().enumerate() {
                let text = row
                    .alternatives
                    .iter()
                    .map(|condition| self.describe_condition(condition))
                    .collect::<Vec<_>>()
                    .join("  or  ");
                chips.push(FilterChip {
                    group_ix,
                    row_ix,
                    text,
                });
            }
        }
        self.chips = chips;
    }

    fn remove_chip(
        &mut self,
        group_ix: usize,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.encoded_query.groups.get_mut(group_ix)
            && row_ix < group.rows.len()
        {
            group.rows.remove(row_ix);
            self.encoded_query.normalize();
            self.after_query_changed(true, window, cx);
        }
    }

    /// Classic breadcrumb behaviour: keep the conditions up to and including `row_ix`.
    fn truncate_filter_after(
        &mut self,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let [group] = self.encoded_query.groups.as_mut_slice() {
            group.rows.truncate(row_ix + 1);
            self.after_query_changed(true, window, cx);
        }
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.encoded_query.groups.clear();
        self.after_query_changed(true, window, cx);
    }

    /// Shows or hides the inline condition builder.
    pub fn toggle_filter_builder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_builder.take().is_some() {
            self._filter_builder_sub = None;
            cx.notify();
            return;
        }
        let Some(schema) = self.schema.clone() else {
            self.services.send_cmd(WorkspaceCommand::ToastInfo(
                "The filter builder opens once the table's fields have loaded.".into(),
            ));
            return;
        };
        let (services, client, query) = (
            self.services.clone(),
            self.client.clone(),
            self.encoded_query.clone(),
        );
        let builder = cx.new(|cx| FilterBuilder::new(services, client, schema, &query, window, cx));
        self._filter_builder_sub = Some(cx.subscribe_in(
            &builder,
            window,
            |this: &mut Self, _, event: &FilterBuilderEvent, window, cx| match event.clone() {
                FilterBuilderEvent::Run(query) => {
                    this.encoded_query = query;
                    this.after_query_changed(false, window, cx);
                }
                FilterBuilderEvent::Save(query) => this.prompt_save_filter(query, window, cx),
                FilterBuilderEvent::Close => {
                    this.filter_builder = None;
                    this._filter_builder_sub = None;
                    cx.notify();
                }
            },
        ));
        self.filter_builder = Some(builder);
        cx.notify();
    }

    // ---- Saved filters (pills) ----

    fn local_filters_key(&self) -> String {
        format!("filters.local:{}", self.table)
    }

    /// Filters kept on this Mac because ServiceNow would not store them.
    fn local_saved_filters(&self) -> Vec<SavedFilter> {
        self.services
            .cache
            .get_account_metadata(&self.client.profile().id, &self.local_filters_key())
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn store_local_saved_filters(&self, filters: &[SavedFilter]) {
        if let Err(err) = self.services.cache.save_account_metadata(
            &self.client.profile().id,
            &self.local_filters_key(),
            &filters.to_vec(),
        ) {
            self.services.send_cmd(WorkspaceCommand::ToastError(format!(
                "Cannot save filter: {err}"
            )));
        }
    }

    /// Reloads the pills: ServiceNow's saved filters for this table and its parents, then
    /// any kept locally. A failed fetch leaves the current pills in place.
    fn load_saved_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = &self.schema else {
            return;
        };
        let chain = schema.hierarchy.clone();
        let client = self.client.clone();
        let (tx, rx) = async_channel::bounded(1);
        self.services.tokio_handle.spawn(async move {
            let _ = tx.send(client.fetch_saved_filters(&chain).await).await;
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = this.update(cx, |panel, cx| {
                    match result {
                        Ok(mut filters) => {
                            filters.extend(panel.local_saved_filters());
                            panel.saved_filters = filters;
                        }
                        Err(err) => {
                            tracing::warn!(%err, table = %panel.table, "Cannot load saved filters")
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Runs a saved-filter write against ServiceNow, then reloads the pills. `on_refused`
    /// applies the same change to the local store when ServiceNow rejects the write.
    fn write_saved_filter<F>(
        &mut self,
        write: F,
        on_refused: impl FnOnce(&mut Self) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: std::future::Future<Output = sn_core::SnResult<()>> + Send + 'static,
    {
        let (tx, rx) = async_channel::bounded(1);
        self.services.tokio_handle.spawn(async move {
            let _ = tx.send(write.await).await;
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    if let Err(err) = result {
                        on_refused(panel);
                        panel.services.send_cmd(WorkspaceCommand::ToastInfo(format!(
                            "ServiceNow did not accept the change, so it is kept on this Mac \
                             only. ({err})"
                        )));
                    }
                    panel.load_saved_filters(window, cx);
                });
            }
        })
        .detach();
    }

    fn prompt_save_filter(
        &mut self,
        query: EncodedQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if query.is_empty() {
            self.services.send_cmd(WorkspaceCommand::ToastInfo(
                "Add at least one condition before saving a filter.".into(),
            ));
            return;
        }
        let list = cx.entity().downgrade();
        let filter = query.to_encoded_string();
        crate::workspace::prompt_for_name(
            "Save filter",
            "",
            "Save",
            move |title, window, cx| {
                let _ = list.update(cx, |list, cx| {
                    list.save_filter(title, filter.clone(), window, cx);
                });
            },
            window,
            cx,
        );
    }

    pub fn save_filter(
        &mut self,
        title: String,
        filter: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let table = self.table.clone();
        let local = SavedFilter {
            id: format!("local:{}", uuid::Uuid::new_v4()),
            title: title.clone(),
            table: table.clone(),
            filter: filter.clone(),
            editable: true,
        };
        self.write_saved_filter(
            async move {
                client
                    .create_saved_filter(&title, &table, &filter)
                    .await
                    .map(|_| ())
            },
            move |panel| {
                let mut filters = panel.local_saved_filters();
                filters.push(local);
                panel.store_local_saved_filters(&filters);
            },
            window,
            cx,
        );
    }

    /// Renames a saved filter and/or replaces its query.
    pub fn update_filter(
        &mut self,
        id: String,
        title: Option<String>,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let apply_locally = {
            let (id, title, filter) = (id.clone(), title.clone(), filter.clone());
            move |panel: &mut Self| {
                let mut filters = panel.local_saved_filters();
                if let Some(saved) = filters.iter_mut().find(|saved| saved.id == id) {
                    if let Some(title) = title {
                        saved.title = title;
                    }
                    if let Some(filter) = filter {
                        saved.filter = filter;
                    }
                }
                panel.store_local_saved_filters(&filters);
            }
        };
        if id.starts_with("local:") {
            apply_locally(self);
            self.load_saved_filters(window, cx);
            return;
        }
        let client = self.client.clone();
        self.write_saved_filter(
            async move {
                client
                    .update_saved_filter(&id, title.as_deref(), filter.as_deref())
                    .await
            },
            // A refused edit of a server filter has no local copy to change.
            |_| {},
            window,
            cx,
        );
    }

    pub fn delete_filter(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if id.starts_with("local:") {
            let mut filters = self.local_saved_filters();
            filters.retain(|saved| saved.id != id);
            self.store_local_saved_filters(&filters);
            self.load_saved_filters(window, cx);
            return;
        }
        let client = self.client.clone();
        self.write_saved_filter(
            async move { client.delete_saved_filter(&id).await },
            |_| {},
            window,
            cx,
        );
    }

    fn render_filter_pills(&self, active_q: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pill = |id: ElementId, current: bool| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_full()
                .cursor_pointer()
                .border_1()
                .border_color(if current { theme.primary } else { theme.border })
                .bg(if current {
                    theme.primary.opacity(0.16)
                } else {
                    theme.secondary
                })
                .text_size(sp(11.0))
                .font_medium()
                .text_color(if current {
                    theme.primary
                } else {
                    theme.muted_foreground
                })
        };
        let list = cx.entity().downgrade();
        let current_filter = self.encoded_query.to_encoded_string();
        let already_saved = self
            .saved_filters
            .iter()
            .any(|saved| EncodedQuery::parse(&saved.filter).filter_only_string() == active_q);

        h_flex()
            .gap_1p5()
            .flex_wrap()
            .items_center()
            .child(
                pill("pill-all".into(), active_q.is_empty())
                    .debug_selector(|| "pill-all".into())
                    .on_click(cx.listener(|this, _, window, cx| this.clear_filter(window, cx)))
                    .child("All"),
            )
            .children(self.saved_filters.iter().enumerate().map(|(ix, saved)| {
                let is_current = !active_q.is_empty()
                    && EncodedQuery::parse(&saved.filter).filter_only_string() == active_q;
                let apply = saved.filter.clone();
                let selector = format!("pill:{}", saved.title);
                let (menu_list, menu_saved, menu_current) =
                    (list.clone(), saved.clone(), current_filter.clone());
                pill(("saved-pill", ix).into(), is_current)
                    .debug_selector(move || selector)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_encoded_query_str(&apply, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(saved.title.clone())
                            // Global filters cannot be edited from here.
                            .when(!saved.editable, |pill| {
                                pill.child(Icon::new(IconName::Lock).xsmall())
                            }),
                    )
                    .context_menu(move |menu, _, _| {
                        let (list, saved) = (menu_list.clone(), menu_saved.clone());
                        if !saved.editable {
                            return menu.item(
                                PopupMenuItem::new("Save a copy I can edit").on_click(
                                    move |_, window, cx| {
                                        let _ = list.update(cx, |list, cx| {
                                            list.save_filter(
                                                saved.title.clone(),
                                                saved.filter.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            );
                        }
                        let rename = {
                            let (list, saved) = (list.clone(), saved.clone());
                            PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                                let (list, id) = (list.clone(), saved.id.clone());
                                crate::workspace::prompt_for_name(
                                    "Rename filter",
                                    &saved.title,
                                    "Rename",
                                    move |title, window, cx| {
                                        let _ = list.update(cx, |list, cx| {
                                            list.update_filter(
                                                id.clone(),
                                                Some(title),
                                                None,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                    window,
                                    cx,
                                );
                            })
                        };
                        let update = {
                            let (list, id, current) =
                                (list.clone(), saved.id.clone(), menu_current.clone());
                            PopupMenuItem::new("Update to the current filter").on_click(
                                move |_, window, cx| {
                                    let _ = list.update(cx, |list, cx| {
                                        list.update_filter(
                                            id.clone(),
                                            None,
                                            Some(current.clone()),
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            )
                        };
                        let duplicate = {
                            let (list, saved) = (list.clone(), saved.clone());
                            PopupMenuItem::new("Duplicate").on_click(move |_, window, cx| {
                                let _ = list.update(cx, |list, cx| {
                                    list.save_filter(
                                        format!("{} copy", saved.title),
                                        saved.filter.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            })
                        };
                        let delete = {
                            let (list, id) = (list.clone(), saved.id.clone());
                            PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                                let _ = list.update(cx, |list, cx| {
                                    list.delete_filter(id.clone(), window, cx);
                                });
                            })
                        };
                        menu.item(rename)
                            .item(update)
                            .item(duplicate)
                            .separator()
                            .item(delete)
                    })
            }))
            .child(
                Button::new("save-filter-pill")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip(if active_q.is_empty() {
                        "Filter the list first, then save the filter here"
                    } else if already_saved {
                        "This filter is already saved"
                    } else {
                        "Save the current filter"
                    })
                    .disabled(active_q.is_empty() || already_saved)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.prompt_save_filter(this.encoded_query.clone(), window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("toggle-encoded-query-bar")
                    .ghost()
                    .xsmall()
                    .label("Edit as text")
                    .tooltip("Show the encoded query (sysparm_query) for direct editing")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_raw_query_bar = !this.show_raw_query_bar;
                        cx.notify();
                    })),
            )
    }

    fn render_filter_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Truncating at a separator is only well defined without `^NQ` branches.
        let single_group = self.encoded_query.groups.len() == 1;
        h_flex().gap_1().flex_wrap().items_center().children(
            self.chips.iter().enumerate().flat_map(|(ix, chip)| {
                let (group_ix, row_ix) = (chip.group_ix, chip.row_ix);
                let separator = (ix > 0).then(|| {
                    if row_ix == 0 {
                        div()
                            .px_1()
                            .text_size(sp(10.5))
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child("OR")
                            .into_any_element()
                    } else {
                        let selector = format!("crumb-sep:{row_ix}");
                        div()
                            .id(("crumb-sep", ix))
                            .debug_selector(move || selector)
                            .px_1()
                            .rounded(px(3.0))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .when(single_group, |separator| {
                                separator
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.secondary_hover))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.truncate_filter_after(row_ix - 1, window, cx);
                                    }))
                            })
                            .child("›")
                            .into_any_element()
                    }
                });
                let chip_selector = format!("chip:{ix}");
                let remove_selector = format!("chip-remove:{ix}");
                let chip_el = h_flex()
                    .id(("filter-chip", ix))
                    .debug_selector(move || chip_selector)
                    .px_2()
                    .py_0p5()
                    .gap_1()
                    .items_center()
                    .rounded(px(5.0))
                    .bg(theme.accent)
                    .border_1()
                    .border_color(theme.primary.opacity(0.35))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.filter_builder.is_none() {
                            this.toggle_filter_builder(window, cx);
                        }
                    }))
                    .child(div().text_size(sp(11.0)).child(chip.text.clone()))
                    .child(
                        div()
                            .id(("rm-chip", ix))
                            .debug_selector(move || remove_selector)
                            .cursor_pointer()
                            .rounded(px(3.0))
                            .hover(|style| style.bg(theme.secondary_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.remove_chip(group_ix, row_ix, window, cx);
                            }))
                            .child(
                                Icon::new(IconName::X)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            ),
                    )
                    .into_any_element();
                separator.into_iter().chain(std::iter::once(chip_el))
            }),
        )
    }

    #[cfg(test)]
    pub fn saved_filters(&self) -> &[SavedFilter] {
        &self.saved_filters
    }

    #[cfg(test)]
    pub fn chip_texts(&self) -> Vec<String> {
        self.chips.iter().map(|chip| chip.text.clone()).collect()
    }

    #[cfg(test)]
    pub fn total_count(&self) -> Option<usize> {
        self.total_count
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.is_loading_network
    }

    #[cfg(test)]
    pub fn query_string(&self) -> String {
        self.encoded_query.to_encoded_string()
    }

    #[cfg(test)]
    pub fn filter_builder(&self) -> Option<Entity<FilterBuilder>> {
        self.filter_builder.clone()
    }

    fn reload_data(&mut self, force_schema: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.request_generation += 1;
        let generation = self.request_generation;
        self.page_limit = AppPrefs::get(cx).list_page_size;
        let page_limit = self.page_limit;
        self.refresh_favorite_state(cx);
        let inst_id = self.client.profile().id.clone();
        let q_str = self.encoded_query.to_encoded_string();

        // 1. Check SWR cache for instant paint on filter/page switches!
        let swr_t0 = Instant::now();
        if let Ok(Some(cached)) = self.services.cache.get_list_page(
            &inst_id,
            &self.table,
            &q_str,
            self.offset,
            page_limit,
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
        let view = self.view.clone();
        let offset = self.offset;
        let current_custom_cols = services
            .cache
            .get_list_columns(&inst_id, &sn_core::classic::view_cache_key(&table, &view))
            .ok()
            .flatten();
        let fallback_cols = self.column_keys.clone();

        let (tx, rx) =
            async_channel::bounded::<Result<(TableSchema, sn_core::ListPage, u64), String>>(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let net_t0 = Instant::now();
            let mut schema = match services
                .ensure_table_schema(&client, &table, force_schema)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            let server_key = format!("list:{}", sn_core::classic::view_cache_key(&table, &view));
            match services
                .cache
                .get_account_metadata::<Vec<String>>(&client.profile().id, &server_key)
            {
                Ok(Some(columns)) => schema.default_list_columns = columns,
                Err(err) => tracing::warn!(%err, "Cannot read synced list columns"),
                _ => {}
            }
            match client.fetch_list_columns(&schema.hierarchy, &view).await {
                Ok(columns) => {
                    schema.default_list_columns = columns.unwrap_or_else(|| {
                        TableSchema::derive_default_list_columns(&schema.fields)
                    });
                    if let Err(err) = services.cache.save_account_metadata(
                        &client.profile().id,
                        &server_key,
                        &schema.default_list_columns,
                    ) {
                        services.send_cmd(WorkspaceCommand::ToastError(format!(
                            "Cannot cache list columns: {err}"
                        )));
                    }
                }
                Err(err) => services.send_cmd(WorkspaceCommand::ToastError(format!(
                    "Cannot sync ServiceNow list columns: {err}; using cached/default columns."
                ))),
            }
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
                limit: page_limit,
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
                let _ = this.update_in(cx, |panel, window, cx| {
                    if panel.request_generation != generation {
                        return;
                    }
                    panel.is_loading_network = false;
                    match res {
                        Ok((schema, page, net_ms)) => {
                            if panel
                                .services
                                .cache
                                .get_list_columns(
                                    &panel.client.profile().id,
                                    &sn_core::classic::view_cache_key(&panel.table, &panel.view),
                                )
                                .ok()
                                .flatten()
                                .is_none()
                                && !schema.default_list_columns.is_empty()
                            {
                                panel.column_keys = schema.default_list_columns.clone();
                            }
                            panel.label = schema.label.clone();
                            let first_schema = panel.schema.is_none();
                            panel.schema = Some(schema);
                            if first_schema {
                                panel.load_saved_filters(window, cx);
                            }
                            panel.total_count = page.total_count;
                            panel.records = page.records;
                            panel.network_sync_ms = Some(net_ms);
                            panel.sync_delegate_rows(cx);
                            panel.sync_column_panel(cx);
                            // Labels and display values may only now be resolvable.
                            panel.rebuild_chips();

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

    /// This list as a favorite: the table, the exact filter and sort, and the view.
    fn favorite_target(&self) -> ClassicTarget {
        ClassicTarget::List {
            table: self.table.clone(),
            query: self.encoded_query.to_encoded_string(),
            view: sn_core::classic::normalize_view(&self.view),
        }
    }

    /// Re-reads whether this list is a favorite, after favorites or the filter changed.
    pub fn refresh_favorite_state(&mut self, cx: &mut Context<Self>) {
        let favorited = favorites::find(
            &self.services,
            &self.client.profile().id,
            &self.favorite_target(),
        )
        .is_some();
        if favorited != self.is_favorited {
            self.is_favorited = favorited;
            cx.notify();
        }
    }

    /// Adds or removes this list from the user's ServiceNow favorites. The star updates when
    /// the favorites tree reloads.
    pub fn toggle_favorite(&mut self, _cx: &mut Context<Self>) {
        let target = self.favorite_target();
        match favorites::find(&self.services, &self.client.profile().id, &target) {
            Some(existing) => favorites::remove(&self.services, &self.client, existing),
            None => {
                let title = if self.encoded_query.filter_only_string().is_empty() {
                    self.label.clone()
                } else {
                    format!("{} · Filtered", self.label)
                };
                favorites::add(&self.services, &self.client, title, target);
            }
        }
    }

    /// Shows or hides the docked column chooser.
    pub fn toggle_column_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.column_panel.take().is_some() {
            self._column_panel_sub = None;
            cx.notify();
            return;
        }
        let Some(schema) = self.schema.clone() else {
            self.services.send_cmd(WorkspaceCommand::ToastInfo(
                "Columns can be chosen once the table's fields have loaded.".into(),
            ));
            return;
        };
        let services = self.services.clone();
        let client = self.client.clone();
        let shown = self.column_keys.clone();
        let panel = cx.new(|cx| ColumnPanel::new(services, client, schema, shown, window, cx));
        self._column_panel_sub = Some(cx.subscribe_in(
            &panel,
            window,
            |this: &mut Self, _, event: &ColumnPanelEvent, window, cx| {
                this.apply_column_event(event.clone(), window, cx);
            },
        ));
        panel.update(cx, |panel, cx| panel.focus(window, cx));
        self.column_panel = Some(panel);
        cx.notify();
    }

    fn apply_column_event(
        &mut self,
        event: ColumnPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cache_key = sn_core::classic::view_cache_key(&self.table, &self.view);
        let mut columns = self.column_keys.clone();
        let needs_fetch = match event {
            ColumnPanelEvent::Close => {
                self.column_panel = None;
                self._column_panel_sub = None;
                cx.notify();
                return;
            }
            ColumnPanelEvent::Reset => {
                if let Err(err) = self
                    .services
                    .cache
                    .delete_list_columns(&self.client.profile().id, &cache_key)
                {
                    self.services
                        .send_cmd(WorkspaceCommand::ToastError(err.to_string()));
                    return;
                }
                // The server layout arrives with the reload, which also updates the panel.
                self.reload_data(true, window, cx);
                return;
            }
            ColumnPanelEvent::SetShown { key, shown: true } => {
                if columns.contains(&key) {
                    return;
                }
                columns.push(key);
                true
            }
            ColumnPanelEvent::SetShown { key, shown: false } => {
                if columns.len() == 1 {
                    self.services.send_cmd(WorkspaceCommand::ToastInfo(
                        "Keep at least one column.".into(),
                    ));
                    return;
                }
                columns.retain(|column| *column != key);
                false
            }
            ColumnPanelEvent::Move { key, before } => {
                let Some(from) = columns.iter().position(|column| *column == key) else {
                    return;
                };
                let moved = columns.remove(from);
                let to = before
                    .and_then(|before| columns.iter().position(|column| *column == before))
                    .unwrap_or(columns.len());
                columns.insert(to, moved);
                false
            }
        };
        if columns == self.column_keys {
            return;
        }
        if let Err(err) =
            self.services
                .cache
                .save_list_columns(&self.client.profile().id, &cache_key, &columns)
        {
            self.services
                .send_cmd(WorkspaceCommand::ToastError(err.to_string()));
            return;
        }
        self.column_keys = columns;
        self.sync_column_panel(cx);
        if needs_fetch {
            // A newly shown field has no values in the loaded rows yet.
            self.reload_data(false, window, cx);
        } else {
            self.sync_delegate_rows(cx);
            cx.notify();
        }
    }

    fn sync_column_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.column_panel.clone() {
            let shown = self.column_keys.clone();
            let schema = self.schema.clone();
            panel.update(cx, |panel, cx| {
                if let Some(schema) = schema {
                    panel.set_schema(schema, cx);
                }
                panel.set_shown(shown, cx);
            });
        }
    }

    #[cfg(test)]
    pub fn column_panel(&self) -> Option<Entity<ColumnPanel>> {
        self.column_panel.clone()
    }
}

impl Render for ListViewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let active_q = self.encoded_query.filter_only_string();
        let prefs = AppPrefs::get(cx).clone();
        let page_limit = self.page_limit;

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
                                            .text_size(sp(11.0))
                                            .text_color(theme.muted_foreground)
                                            .child(self.table.clone()),
                                    )
                                    .child(
                                        Button::new("fav-list-toggle")
                                            .ghost()
                                            .xsmall()
                                            .icon(Icon::new(IconName::Star).text_color(
                                                if self.is_favorited {
                                                    rgb(0xFBBF24).into()
                                                } else {
                                                    theme.muted_foreground
                                                },
                                            ))
                                            .tooltip(if self.is_favorited {
                                                "Remove from favorites"
                                            } else {
                                                "Add this list and filter to favorites"
                                            })
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
                                        div().w(sp(230.0)).child(
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
                                        Button::new("toggle-filter-builder")
                                            .map(|button| {
                                                if self.filter_builder.is_some() {
                                                    button.primary()
                                                } else {
                                                    button.secondary()
                                                }
                                            })
                                            .small()
                                            .icon(IconName::Funnel)
                                            .label("Filter")
                                            .tooltip(
                                                "Build the filter: fields, operators, AND / OR",
                                            )
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.toggle_filter_builder(window, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("open-col-chooser")
                                            .map(|button| {
                                                if self.column_panel.is_some() {
                                                    button.primary()
                                                } else {
                                                    button.secondary()
                                                }
                                            })
                                            .small()
                                            .icon(IconName::SlidersHorizontal)
                                            .label("Columns")
                                            .tooltip("Choose and reorder columns")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.toggle_column_panel(window, cx);
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
                    // Saved-filter pills: "All", ServiceNow's saved filters, then "+" to save.
                    .child(self.render_filter_pills(&active_q, cx))
                    // Breadcrumb chips for the active filter.
                    .when(!self.chips.is_empty(), |el| {
                        el.child(self.render_filter_chips(cx))
                    })
                    .when_some(self.filter_builder.clone(), |el, builder| el.child(builder))
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
            // The column chooser docks beside the table, which narrows rather than being
            // covered, so headers stay visible while columns are toggled.
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("list-table-area")
                            .debug_selector(|| "list-table-area".into())
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(
                                DataTable::new(&self.table_state)
                                    .stripe(true)
                                    .bordered(false)
                                    .with_size(prefs.table_size()),
                            ),
                    )
                    .when_some(self.column_panel.clone(), |row, panel| {
                        row.child(
                            div()
                                .id("column-panel-dock")
                                .debug_selector(|| "column-panel-dock".into())
                                .w(sp(300.0))
                                .flex_shrink_0()
                                .h_full()
                                .child(panel),
                        )
                    }),
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
                            .when_some(
                                self.served_from_cache_ms
                                    .filter(|_| prefs.show_perf_telemetry),
                                |el, ms| {
                                    el.child(
                                        div()
                                            .text_size(sp(11.0))
                                            .font_family(theme.mono_font_family.clone())
                                            .text_color(rgb(0x34D399))
                                            .child(format!("⚡ SWR {:.2}ms", ms)),
                                    )
                                },
                            )
                            .when_some(
                                self.network_sync_ms.filter(|_| prefs.show_perf_telemetry),
                                |el, ms| {
                                    el.child(
                                        div()
                                            .text_size(sp(11.0))
                                            .font_family(theme.mono_font_family.clone())
                                            .text_color(theme.muted_foreground)
                                            .child(format!("REST {ms}ms")),
                                    )
                                },
                            )
                            .when(self.is_loading_network, |el| {
                                el.child(
                                    div()
                                        .text_size(sp(11.0))
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
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.offset = this.offset.saturating_sub(page_limit);
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
                                            .is_some_and(|tot| self.offset + page_limit >= tot)
                                            || self.records.len() < page_limit,
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.offset += page_limit;
                                        this.reload_data(false, window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

pub(crate) fn prettify_column_key(key: &str) -> String {
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
