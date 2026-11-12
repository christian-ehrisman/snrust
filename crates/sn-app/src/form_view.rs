//! Live Classic Form View (`FormViewPanel`) & Rearrangeable 2-Column Auto-Grid.
//!
//! ### Core Capabilities
//! - **0ms SWR Row Hydration**: Renders immediately from cached row fields while upgrading full
//!   record fields, `TableSchema`, and `sys_journal_field` Activity Stream in the background.
//! - **Live Real-Time Field Updates**: Applies incoming AMB WebSocket / REST delta changes live to
//!   clean fields with a crisp accent glow pulse, while protecting dirty local edits and surfacing
//!   inline `Keep Mine | Use Server` conflict badges.
//! - **Dictionary-Driven Typed Controls**: Searchable `Select` choice dropdowns, async cached +
//!   network Reference lookup modal with jump-to-record (`↗`), Boolean `Switch`, multi-line
//!   `Textarea`, scalar `Input`, and dual `work_notes` / `comments` Activity Stream composer.
//! - **Rearrangeable 2-Column Auto-Grid**: Native GPUI drag-and-drop card reordering (`on_drag` /
//!   `on_drop`), `1 col` / `2 col` span toggles, field visibility & "+ Add Field" drawer, custom
//!   section dividers, and automatic per-`(instance, table)` SQLite persistence.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{
    Disableable as _, Icon, IndexPath, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::SnClient;
use sn_core::{
    ClassicTarget, ColSpan, DictionaryEntry, FieldConflict, FieldType, FieldValue, FormGridLayout,
    FormSessionState, GridVisualRow, JournalEntry, ReferenceCandidate, SnRecord, TableSchema,
};
use sn_realtime::{RealtimeEvent, SubscriptionGuard, WatchTarget};

use crate::favorites;
use crate::prefs::{AppPrefs, Density};
use crate::reference_preview::{is_single_reference, reference_link};
use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::{priority_badge_colors, sp, spx, state_badge_colors};

const LIVE_PULSE_DURATION: Duration = Duration::from_millis(2800);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnChoiceItem {
    value: String,
    label: String,
}

impl SearchableListItem for SnChoiceItem {
    type Value = Self;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

/// Payload carried during native GPUI drag-and-drop reordering of Auto-Grid field cards.
#[derive(Clone)]
struct DraggedGridField {
    form_entity_id: EntityId,
    element: String,
    label: String,
}

struct DraggedFieldGhost {
    label: String,
}

impl Render for DraggedFieldGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.primary)
            .child(
                Icon::new(IconName::GripVertical)
                    .xsmall()
                    .text_color(theme.primary),
            )
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(self.label.clone()),
            )
    }
}

type ReferencePickHandler = Box<dyn Fn(ReferenceCandidate, &mut Window, &mut App) + 'static>;

/// Async cached + network Reference search picker modal (`sys_user`, `sys_user_group`, `cmdb_ci`, etc.).
struct ReferencePickerModal {
    services: AppServices,
    client: SnClient,
    target_table: String,
    search_input: Entity<InputState>,
    candidates: Vec<ReferenceCandidate>,
    is_searching: bool,
    on_pick: ReferencePickHandler,
    _sub: Subscription,
}

impl ReferencePickerModal {
    fn new(
        services: AppServices,
        client: SnClient,
        target_table: String,
        on_pick: impl Fn(ReferenceCandidate, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(format!("Search {target_table} records…"))
        });

        let initial_cached = services
            .cache
            .search_reference_candidates(&client.profile().id, &target_table, "", 25)
            .unwrap_or_default();

        let sub = cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, _, evt, window, cx| {
                if matches!(evt, InputEvent::Change) {
                    this.run_lookup(window, cx);
                }
            },
        );

        let mut modal = Self {
            services,
            client,
            target_table,
            search_input,
            candidates: initial_cached,
            is_searching: false,
            on_pick: Box::new(on_pick),
            _sub: sub,
        };
        modal.run_lookup(window, cx);
        modal
    }

    fn run_lookup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        let inst_id = self.client.profile().id.clone();

        // 1. Instant local FTS5 cache lookup (<0.5ms)!
        if let Ok(local) = self.services.cache.search_reference_candidates(
            &inst_id,
            &self.target_table,
            &query,
            25,
        ) && !local.is_empty()
        {
            self.candidates = local;
        }

        self.is_searching = true;
        cx.notify();

        // 2. Network search to supplement/refresh
        let services = self.services.clone();
        let client = self.client.clone();
        let target = self.target_table.clone();
        let (tx, rx) = async_channel::bounded(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let (disp_field, sec_field) = match target.as_str() {
                "sys_user" => ("name", Some("email")),
                "sys_user_group" => ("name", Some("description")),
                "cmdb_ci" => ("name", Some("sys_class_name")),
                _ => ("name", None),
            };
            let res = client
                .search_reference(&target, disp_field, &query, sec_field, 25)
                .await;
            if let Ok(ref list) = res {
                let _ = services
                    .cache
                    .save_reference_candidates_async(inst_id, list.clone())
                    .await;
            }
            let _ = tx.send(res).await;
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(remote_list)) = rx.recv().await {
                let _ = this.update(cx, |m, cx| {
                    m.is_searching = false;
                    if !remote_list.is_empty() {
                        m.candidates = remote_list;
                    }
                    cx.notify();
                });
            } else {
                let _ = this.update(cx, |m, cx| {
                    m.is_searching = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for ReferencePickerModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        v_flex()
            .gap_2p5()
            .child(
                Input::new(&self.search_input)
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .cleanable(true),
            )
            .when(self.is_searching, |el| {
                el.child(
                    div()
                        .text_size(sp(11.0))
                        .text_color(theme.primary)
                        .child("Searching ServiceNow…"),
                )
            })
            .child(
                v_flex()
                    .id("ref-candidates-scroll")
                    .max_h(sp(320.0))
                    .overflow_y_scroll()
                    .gap_1()
                    .children(
                        self.candidates
                            .iter()
                            .cloned()
                            .enumerate()
                            .map(|(ix, cand)| {
                                let cand_pick = cand.clone();
                                h_flex()
                                    .id(("ref-cand", ix))
                                    .px_3()
                                    .py_2()
                                    .justify_between()
                                    .items_center()
                                    .rounded(theme.radius)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.accent))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        (this.on_pick)(cand_pick.clone(), window, cx);
                                        window.close_dialog(cx);
                                    }))
                                    .child(
                                        v_flex()
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_medium()
                                                    .truncate()
                                                    .child(cand.display_value),
                                            )
                                            .when_some(cand.secondary, |el, sec| {
                                                el.child(
                                                    div()
                                                        .text_size(sp(11.0))
                                                        .text_color(theme.muted_foreground)
                                                        .truncate()
                                                        .child(sec),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .font_family(theme.mono_font_family.clone())
                                            .text_size(sp(10.0))
                                            .text_color(theme.muted_foreground)
                                            .child(cand.sys_id.chars().take(8).collect::<String>()),
                                    )
                            }),
                    ),
            )
    }
}

/// Opens the searchable reference picker for `target_table`. Shared by form reference
/// fields and the list filter builder.
pub(crate) fn open_reference_dialog(
    services: AppServices,
    client: SnClient,
    target_table: String,
    on_pick: impl Fn(ReferenceCandidate, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let modal = cx.new(|cx| {
        ReferencePickerModal::new(services, client, target_table.clone(), on_pick, window, cx)
    });
    let first_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            modal
                .read(cx)
                .search_input
                .focus_handle(cx)
                .focus(window, cx);
        }
        dialog
            .w(spx(540.0, cx))
            .title(format!("Select Reference — {target_table}"))
            .close_button(true)
            .overlay_closable(true)
            .child(modal.clone())
    });
}

enum FieldWidgetControl {
    SingleLine(Entity<InputState>),
    MultiLine(Entity<TextareaState>),
    Choice {
        state: Entity<SelectState<SearchableVec<SnChoiceItem>>>,
    },
    Boolean,
    Reference {
        target_table: String,
    },
}

pub struct FormViewPanel {
    services: AppServices,
    client: SnClient,
    schema: Option<TableSchema>,
    layout: FormGridLayout,
    session: FormSessionState,
    view: String,

    controls: HashMap<String, FieldWidgetControl>,
    work_notes_input: Entity<TextareaState>,
    comments_input: Entity<TextareaState>,
    active_journal_tab: &'static str, // "work_notes" or "comments"
    new_section_input: Entity<InputState>,

    customize_grid_mode: bool,
    show_system_metadata: bool,
    is_hydrating: bool,
    is_saving: bool,
    is_favorited: bool,
    swr_hydrated_ms: Option<f64>,
    live_pulse_until: HashMap<String, Instant>,

    _realtime_guard: Option<SubscriptionGuard>,
    _control_subs: Vec<Subscription>,
}

impl FormViewPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn open_existing(
        services: AppServices,
        client: SnClient,
        table: String,
        sys_id: String,
        seed_record: Option<SnRecord>,
        view: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t0 = Instant::now();
        let inst_id = client.profile().id.clone();

        // 1. Instant synchronous cache hydration (`SnCache` hot `moka` LRU -> SQLite)
        let cached_rec = services
            .cache
            .get_record(&inst_id, &table, &sys_id)
            .ok()
            .flatten()
            .map(|c| c.data);

        let initial_record = match (cached_rec, seed_record) {
            (Some(mut c), Some(s)) => {
                c.merge_from(&s);
                c
            }
            (Some(c), None) => c,
            (None, Some(s)) => s,
            (None, None) => {
                let mut stub = SnRecord::new_draft(table.clone());
                stub.sys_id = sys_id.clone();
                stub.is_partial = true;
                stub
            }
        };

        let cached_schema = services
            .cache
            .get_table_schema(&inst_id, &table)
            .ok()
            .flatten()
            .map(|c| c.data);

        let cached_journals = services
            .cache
            .get_journals(&inst_id, &table, &sys_id)
            .unwrap_or_default();

        let swr_ms =
            (!initial_record.fields.is_empty()).then(|| t0.elapsed().as_secs_f64() * 1000.0);

        let mut session = FormSessionState::from_existing_record(inst_id.clone(), initial_record);
        let _ = session.merge_journals(cached_journals);

        let layout =
            Self::resolve_layout(&services, &inst_id, &table, &view, cached_schema.as_ref());
        let is_favorited = favorites::find(
            &services,
            &inst_id,
            &ClassicTarget::Form {
                table: table.clone(),
                sys_id: sys_id.clone(),
                query: String::new(),
                view: sn_core::classic::normalize_view(&view),
            },
        )
        .is_some();

        let mut panel = Self::build_common(
            services,
            client,
            cached_schema,
            layout,
            session,
            view,
            swr_ms,
            is_favorited,
            window,
            cx,
        );
        panel.hydrate_from_network(false, window, cx);
        panel
    }

    pub fn open_new(
        services: AppServices,
        client: SnClient,
        table: String,
        view: String,
        initial_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inst_id = client.profile().id.clone();
        let cached_schema = services
            .cache
            .get_table_schema(&inst_id, &table)
            .ok()
            .flatten()
            .map(|c| c.data);
        let mut session = FormSessionState::new_record(inst_id.clone(), table.clone());
        let defaults = sn_core::EncodedQuery::parse(&initial_query);
        // Only plain `field=value` conditions can be applied as defaults without running
        // ServiceNow script; anything else is reported once and left for the user.
        let mut skipped = defaults.groups.len() > 1;
        for row in defaults
            .groups
            .first()
            .map(|g| g.rows.as_slice())
            .unwrap_or(&[])
        {
            match row.as_single_clause() {
                Some(clause)
                    if clause.operator == sn_core::FilterOperator::Equals
                        && sn_core::classic::valid_table_name(&clause.field)
                        && !clause.field.starts_with("sys_")
                        && !clause.value.starts_with("javascript:") =>
                {
                    session.edit_field(clause.field.clone(), FieldValue::scalar(&clause.value));
                }
                _ => skipped = true,
            }
        }
        if skipped {
            services.send_cmd(WorkspaceCommand::ToastInfo(
                "Some module defaults need ServiceNow scripting and were not applied; enter \
                 those values manually."
                    .into(),
            ));
        }
        let layout =
            Self::resolve_layout(&services, &inst_id, &table, &view, cached_schema.as_ref());

        let mut panel = Self::build_common(
            services,
            client,
            cached_schema,
            layout,
            session,
            view,
            None,
            false,
            window,
            cx,
        );
        panel.hydrate_schema_only(window, cx);
        panel
    }

    #[allow(clippy::too_many_arguments)]
    fn build_common(
        services: AppServices,
        client: SnClient,
        schema: Option<TableSchema>,
        layout: FormGridLayout,
        session: FormSessionState,
        view: String,
        swr_hydrated_ms: Option<f64>,
        is_favorited: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let work_notes_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Add internal work notes (included on Save / ⌘S)…")
        });
        let comments_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Add customer-visible comments (included on Save / ⌘S)…")
        });
        let new_section_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("New section header title…"));

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &work_notes_input,
            window,
            |this: &mut Self, state, evt, _, cx| {
                if matches!(evt, InputEvent::Change) {
                    let txt = state.read(cx).value().to_string();
                    this.session.edit_journal("work_notes", txt);
                    cx.notify();
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &comments_input,
            window,
            |this: &mut Self, state, evt, _, cx| {
                if matches!(evt, InputEvent::Change) {
                    let txt = state.read(cx).value().to_string();
                    this.session.edit_journal("comments", txt);
                    cx.notify();
                }
            },
        ));

        let mut this = Self {
            services,
            client,
            schema,
            layout,
            session,
            view,
            controls: HashMap::new(),
            work_notes_input,
            comments_input,
            active_journal_tab: "work_notes",
            new_section_input,
            customize_grid_mode: false,
            show_system_metadata: false,
            is_hydrating: false,
            is_saving: false,
            is_favorited,
            swr_hydrated_ms,
            live_pulse_until: HashMap::new(),
            _realtime_guard: None,
            _control_subs: subs,
        };

        this.rebuild_field_controls(window, cx);
        this
    }

    fn resolve_layout(
        services: &AppServices,
        inst_id: &sn_core::InstanceId,
        table: &str,
        view: &str,
        schema: Option<&TableSchema>,
    ) -> FormGridLayout {
        if let Ok(Some(mut saved)) = services
            .cache
            .get_form_layout(inst_id, &sn_core::classic::view_cache_key(table, view))
        {
            if let Some(s) = schema {
                saved.reconcile_with_schema(s);
            }
            return saved;
        }
        if let Some(s) = schema {
            match services
                .cache
                .get_account_metadata::<Vec<sn_core::ClassicFormView>>(
                    inst_id,
                    &format!("forms:{table}"),
                ) {
                Ok(Some(views)) => {
                    if let Some(server_view) = views
                        .iter()
                        .find(|candidate| candidate.name == view)
                        .or_else(|| views.iter().find(|candidate| candidate.name.is_empty()))
                    {
                        return server_view.layout(s);
                    }
                }
                Err(err) => tracing::warn!(%err, "Cannot read cached form views"),
                _ => {}
            }
            return FormGridLayout::generate_smart_default(s);
        }
        // Minimal bootstrap layout before schema arrives over the network
        let fallback_schema = TableSchema::assemble(
            table,
            table,
            vec![table.to_string()],
            &HashMap::new(),
            &HashMap::new(),
            None,
        );
        FormGridLayout::generate_smart_default(&fallback_schema)
    }

    pub fn table_name(&self) -> &str {
        &self.session.table
    }

    pub fn tab_title(&self) -> String {
        if self.session.is_new_record() {
            format!("New {}", self.session.table)
        } else {
            self.session.baseline.primary_identifier()
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.session.is_dirty()
    }

    #[cfg(test)]
    pub fn raw_value(&self, field: &str) -> String {
        self.session.effective_raw(field).to_string()
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.is_hydrating
    }

    pub fn toggle_customize_grid(&mut self, cx: &mut Context<Self>) {
        self.customize_grid_mode = !self.customize_grid_mode;
        cx.notify();
    }

    pub fn force_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session.is_new_record() {
            self.hydrate_from_network(true, window, cx);
        } else {
            self.hydrate_schema_only(window, cx);
        }
    }

    fn rebuild_field_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Collect all field names referenced in layout or baseline record
        let mut all_fields: Vec<String> = self
            .layout
            .items
            .iter()
            .filter_map(|i| i.field_name().map(str::to_string))
            .collect();
        for f in &self.layout.hidden_fields {
            if !all_fields.contains(f) {
                all_fields.push(f.clone());
            }
        }
        if let Some(schema) = &self.schema {
            for k in schema.fields.keys() {
                if !all_fields.contains(k) {
                    all_fields.push(k.clone());
                }
            }
        } else {
            for k in self.session.baseline.fields.keys() {
                if !all_fields.contains(k) {
                    all_fields.push(k.clone());
                }
            }
        }

        for field_name in all_fields {
            if self.controls.contains_key(&field_name) {
                continue;
            }
            let entry = self.schema.as_ref().and_then(|s| s.fields.get(&field_name));
            let ftype = entry
                .map(|e| e.field_type.clone())
                .unwrap_or(FieldType::String);

            if matches!(ftype, FieldType::JournalInput | FieldType::JournalList) {
                continue;
            }

            let cur_raw = self.session.effective_raw(&field_name).to_string();
            let cur_disp = self.session.effective_display(&field_name).to_string();

            let control = match ftype {
                FieldType::Choice => {
                    let items: Vec<SnChoiceItem> = self
                        .schema
                        .as_ref()
                        .map(|s| {
                            s.choices_for(&field_name, None)
                                .into_iter()
                                .map(|c| SnChoiceItem {
                                    value: c.value.clone(),
                                    label: c.label.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let selected_ix = items
                        .iter()
                        .position(|i| i.value == cur_raw)
                        .map(|row| IndexPath::default().row(row));

                    let select_state = cx.new(|cx| {
                        SelectState::new(SearchableVec::new(items), selected_ix, window, cx)
                            .searchable(true)
                    });

                    let fname_sub = field_name.clone();
                    self._control_subs.push(cx.subscribe_in(
                        &select_state,
                        window,
                        move |this: &mut Self,
                              _,
                              evt: &SelectEvent<SearchableVec<SnChoiceItem>>,
                              _,
                              cx| {
                            let SelectEvent::Confirm(opt) = evt;
                            let fv = match opt {
                                Some(item) => {
                                    FieldValue::new(item.value.clone(), item.label.clone())
                                }
                                None => FieldValue::default(),
                            };
                            this.session.edit_field(fname_sub.clone(), fv);
                            cx.notify();
                        },
                    ));

                    FieldWidgetControl::Choice {
                        state: select_state,
                    }
                }
                FieldType::Boolean => FieldWidgetControl::Boolean,
                FieldType::Reference { target_table } => {
                    FieldWidgetControl::Reference { target_table }
                }
                FieldType::MultiLineText | FieldType::Html => {
                    let ta = cx.new(|cx| TextareaState::new(window, cx).default_value(cur_disp));
                    let fname_sub = field_name.clone();
                    self._control_subs.push(cx.subscribe_in(
                        &ta,
                        window,
                        move |this: &mut Self, state, evt, _, cx| {
                            if matches!(evt, InputEvent::Change) {
                                let v = state.read(cx).value().to_string();
                                this.session
                                    .edit_field(fname_sub.clone(), FieldValue::scalar(v));
                                cx.notify();
                            }
                        },
                    ));
                    FieldWidgetControl::MultiLine(ta)
                }
                _ => {
                    let inp = cx.new(|cx| InputState::new(window, cx).default_value(cur_disp));
                    let fname_sub = field_name.clone();
                    self._control_subs.push(cx.subscribe_in(
                        &inp,
                        window,
                        move |this: &mut Self, state, evt, _, cx| {
                            if matches!(evt, InputEvent::Change) {
                                let v = state.read(cx).value().to_string();
                                this.session
                                    .edit_field(fname_sub.clone(), FieldValue::scalar(v));
                                cx.notify();
                            }
                        },
                    ));
                    FieldWidgetControl::SingleLine(inp)
                }
            };

            self.controls.insert(field_name, control);
        }
    }

    /// Synchronizes clean GPUI widget states (`set_value` / `set_selected_value`) from `self.session`
    /// without firing false user-change events.
    fn sync_clean_controls_from_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (field, ctrl) in &self.controls {
            if self.session.is_field_dirty(field) {
                continue;
            }
            let raw = self.session.effective_raw(field).to_string();
            let disp = self.session.effective_display(field).to_string();

            match ctrl {
                FieldWidgetControl::SingleLine(inp) => {
                    if inp.read(cx).value().as_ref() != disp {
                        inp.update(cx, |s, cx| s.set_value(disp, window, cx));
                    }
                }
                FieldWidgetControl::MultiLine(ta) => {
                    if ta.read(cx).value().as_ref() != disp {
                        ta.update(cx, |s, cx| s.set_value(disp, window, cx));
                    }
                }
                FieldWidgetControl::Choice { state } => {
                    let target_item = SnChoiceItem {
                        value: raw,
                        label: disp,
                    };
                    state.update(cx, |s, cx| {
                        s.set_selected_value(&target_item, window, cx);
                    });
                }
                FieldWidgetControl::Boolean | FieldWidgetControl::Reference { .. } => {}
            }
        }
    }

    fn hydrate_schema_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.is_hydrating = true;
        cx.notify();

        let services = self.services.clone();
        let client = self.client.clone();
        let table = self.session.table.clone();
        let (tx, rx) = async_channel::bounded(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let res = services.ensure_table_schema(&client, &table, false).await;
            if let Ok(schema) = &res {
                services.sync_form_views(&client, schema).await;
            }
            let _ = tx.send(res).await;
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    panel.is_hydrating = false;
                    match result {
                        Ok(schema) => panel.apply_loaded_schema(schema, window, cx),
                        Err(err) => panel
                            .services
                            .send_cmd(WorkspaceCommand::ToastError(format!(
                                "Cannot load form schema: {err}"
                            ))),
                    }
                });
            } else {
                let _ = this.update(cx, |panel, cx| {
                    panel.is_hydrating = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn hydrate_from_network(
        &mut self,
        force_schema: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sys_id) = self.session.sys_id.clone() else {
            return;
        };
        self.is_hydrating = true;
        cx.notify();

        let services = self.services.clone();
        let client = self.client.clone();
        let table = self.session.table.clone();
        let (tx, rx) =
            async_channel::bounded::<Result<(TableSchema, SnRecord, Vec<JournalEntry>), String>>(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let (schema_res, rec_res, jrn_res) = tokio::join!(
                services.ensure_table_schema(&client, &table, force_schema),
                client.get_record(&table, &sys_id),
                client.get_journal_entries(&sys_id, None, 50)
            );

            match (schema_res, rec_res) {
                (Ok(schema), Ok(record)) => {
                    services.sync_form_views(&client, &schema).await;
                    let inst_id = client.profile().id.clone();
                    let journals = jrn_res.unwrap_or_default();
                    let _ = services.cache.upsert_record(&inst_id, &record);
                    let _ = services
                        .cache
                        .save_journals_async(
                            inst_id,
                            table.clone(),
                            sys_id.clone(),
                            journals.clone(),
                        )
                        .await;
                    let _ = tx.send(Ok((schema, record, journals))).await;
                }
                (Err(e), _) | (_, Err(e)) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(outcome) = rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    panel.is_hydrating = false;
                    match outcome {
                        Ok((schema, full_record, journals)) => {
                            panel.session.hydrate_initial_full_record(full_record);
                            let _ = panel.session.merge_journals(journals);
                            panel.apply_loaded_schema(schema, window, cx);
                            panel.start_realtime_watch();
                        }
                        Err(err) => {
                            panel
                                .services
                                .send_cmd(WorkspaceCommand::ToastError(format!(
                                    "Failed loading record: {err}"
                                )));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn apply_loaded_schema(
        &mut self,
        schema: TableSchema,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Rebuild controls when schema arrives so Choice & Reference fields upgrade from bootstrap!
        self.controls.clear();
        self.layout = Self::resolve_layout(
            &self.services,
            &self.client.profile().id,
            &schema.table,
            &self.view,
            Some(&schema),
        );
        self.schema = Some(schema);
        self.rebuild_field_controls(window, cx);
        self.sync_clean_controls_from_session(window, cx);
    }

    fn start_realtime_watch(&mut self) {
        let Some(sys_id) = self.session.sys_id.clone() else {
            return;
        };
        self._realtime_guard = Some(self.services.realtime.watch(
            self.client.clone(),
            WatchTarget::Record {
                table: self.session.table.clone(),
                sys_id,
                last_mod_count: self.session.baseline.sys_mod_count,
                last_updated_on: self.session.baseline.sys_updated_on.clone(),
                last_journal_ts: self.session.latest_journal_timestamp().map(str::to_string),
            },
        ));
    }

    pub fn on_realtime_event(
        &mut self,
        event: &RealtimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let my_inst = &self.client.profile().id;
        let Some(my_sys_id) = self.session.sys_id.clone() else {
            return;
        };

        match event {
            RealtimeEvent::RecordUpdated {
                instance_id,
                table,
                sys_id,
                record,
                ..
            } if instance_id == my_inst && table == &self.session.table && *sys_id == my_sys_id => {
                let outcome = self.session.apply_server_record(record);
                if !outcome.updated_clean_fields.is_empty() {
                    let until = Instant::now() + LIVE_PULSE_DURATION;
                    for pulse in &outcome.updated_clean_fields {
                        self.live_pulse_until.insert(pulse.field.clone(), until);
                    }
                    self.sync_clean_controls_from_session(window, cx);

                    // Schedule pulse cleanup after LIVE_PULSE_DURATION so border returns smoothly to normal
                    cx.spawn(async move |this, cx| {
                        tokio::time::sleep(LIVE_PULSE_DURATION + Duration::from_millis(80)).await;
                        let _ = this.update(cx, |panel, cx| {
                            let now = Instant::now();
                            panel.live_pulse_until.retain(|_, exp| *exp > now);
                            cx.notify();
                        });
                    })
                    .detach();
                }
                cx.notify();
            }
            RealtimeEvent::JournalsAdded {
                instance_id,
                table,
                sys_id,
                entries,
            } if instance_id == my_inst
                && table == &self.session.table
                && *sys_id == my_sys_id
                && self.session.merge_journals(entries.clone()) > 0 =>
            {
                cx.notify();
            }
            _ => {}
        }
    }

    pub fn save(&mut self, close_after: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        let payload = self.session.build_save_payload();
        if payload.is_empty() && !self.session.is_new_record() {
            self.services
                .send_cmd(WorkspaceCommand::ToastInfo("No unsaved changes".into()));
            if close_after {
                self.services.send_cmd(WorkspaceCommand::RecordSaved {
                    instance_id: self.client.profile().id.clone(),
                    record: self.session.baseline.clone(),
                    close_after: true,
                });
            }
            return;
        }

        // Validate mandatory dictionary fields before submitting
        if let Some(schema) = &self.schema {
            for item in &self.layout.items {
                if let Some(fname) = item.field_name()
                    && let Some(entry) = schema.fields.get(fname)
                    && entry.mandatory
                    && !entry.read_only
                    && self.session.effective_raw(fname).trim().is_empty()
                {
                    self.services.send_cmd(WorkspaceCommand::ToastError(format!(
                        "Mandatory field '{}' is required before saving.",
                        entry.column_label
                    )));
                    return;
                }
            }
        }

        self.is_saving = true;
        cx.notify();

        let services = self.services.clone();
        let client = self.client.clone();
        let table = self.session.table.clone();
        let existing_id = self.session.sys_id.clone();
        let had_journal = !self.session.journal_drafts.is_empty();
        let (tx, rx) = async_channel::bounded::<Result<(SnRecord, Vec<JournalEntry>), String>>(1);

        let handle = services.tokio_handle.clone();
        handle.spawn(async move {
            let save_res = match &existing_id {
                Some(sid) => client.patch_record(&table, sid, &payload).await,
                None => client.create_record(&table, &payload).await,
            };

            match save_res {
                Ok(saved_rec) => {
                    let inst_id = client.profile().id.clone();
                    let _ = services.cache.upsert_record(&inst_id, &saved_rec);
                    let journals = if had_journal && !saved_rec.sys_id.is_empty() {
                        let j = client
                            .get_journal_entries(&saved_rec.sys_id, None, 50)
                            .await
                            .unwrap_or_default();
                        let _ = services
                            .cache
                            .save_journals_async(
                                inst_id,
                                table.clone(),
                                saved_rec.sys_id.clone(),
                                j.clone(),
                            )
                            .await;
                        j
                    } else {
                        Vec::new()
                    };
                    let _ = tx.send(Ok((saved_rec, journals))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                }
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(res) = rx.recv().await {
                let _ = this.update_in(cx, |panel, window, cx| {
                    panel.is_saving = false;
                    match res {
                        Ok((saved_rec, journals)) => {
                            let was_new = panel.session.is_new_record();
                            panel.session.commit_saved_record(saved_rec.clone());
                            if !journals.is_empty() {
                                let _ = panel.session.merge_journals(journals);
                            }

                            panel
                                .work_notes_input
                                .update(cx, |s, cx| s.set_value("", window, cx));
                            panel
                                .comments_input
                                .update(cx, |s, cx| s.set_value("", window, cx));
                            panel.sync_clean_controls_from_session(window, cx);

                            // Advance realtime watermark so our own write isn't echoed as a remote update!
                            if let Some(guard) = &panel._realtime_guard {
                                panel.services.realtime.advance_record_watermark(
                                    &panel.client.profile().id,
                                    guard.id(),
                                    saved_rec.sys_mod_count,
                                    saved_rec.sys_updated_on.clone(),
                                    panel.session.latest_journal_timestamp().map(str::to_string),
                                );
                            } else if was_new {
                                panel.start_realtime_watch();
                            }

                            panel
                                .services
                                .send_cmd(WorkspaceCommand::ToastSuccess(format!(
                                    "Saved {}",
                                    saved_rec.primary_identifier()
                                )));
                            panel.services.send_cmd(WorkspaceCommand::RecordSaved {
                                instance_id: panel.client.profile().id.clone(),
                                record: saved_rec,
                                close_after,
                            });
                        }
                        Err(err) => {
                            panel
                                .services
                                .send_cmd(WorkspaceCommand::ToastError(format!(
                                    "Save failed: {err}"
                                )));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn persist_layout(&self) {
        if let Err(err) = self.services.cache.save_form_layout_for_view(
            &self.client.profile().id,
            &self.layout,
            &self.view,
        ) {
            self.services.send_cmd(WorkspaceCommand::ToastError(format!(
                "Cannot save form layout: {err}"
            )));
        }
    }

    fn reset_layout_to_default(&mut self, cx: &mut Context<Self>) {
        let inst_id = &self.client.profile().id;
        if let Err(err) = self.services.cache.delete_form_layout(
            inst_id,
            &sn_core::classic::view_cache_key(&self.session.table, &self.view),
        ) {
            self.services
                .send_cmd(WorkspaceCommand::ToastError(err.to_string()));
            return;
        }
        self.layout = Self::resolve_layout(
            &self.services,
            inst_id,
            &self.session.table,
            &self.view,
            self.schema.as_ref(),
        );
        cx.notify();
    }

    fn open_view_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let views = match self
            .services
            .cache
            .get_account_metadata::<Vec<sn_core::ClassicFormView>>(
                &self.client.profile().id,
                &format!("forms:{}", self.session.table),
            ) {
            Ok(views) => views.unwrap_or_default(),
            Err(err) => {
                self.services
                    .send_cmd(WorkspaceCommand::ToastError(err.to_string()));
                return;
            }
        };
        if views.is_empty() {
            self.services.send_cmd(WorkspaceCommand::ToastInfo(
                "No ServiceNow form views available. Using the automatic grid.".into(),
            ));
            return;
        }
        let items: Vec<_> = views
            .into_iter()
            .map(|view| SnChoiceItem {
                value: view.name,
                label: view.label,
            })
            .collect();
        let selected = items
            .iter()
            .position(|item| item.value == self.view)
            .map(|row| IndexPath::default().row(row));
        let state = cx.new(|cx| {
            SelectState::new(SearchableVec::new(items), selected, window, cx).searchable(true)
        });
        self._control_subs.push(cx.subscribe_in(
            &state,
            window,
            |this, _, event: &SelectEvent<SearchableVec<SnChoiceItem>>, window, cx| {
                let SelectEvent::Confirm(Some(item)) = event else {
                    return;
                };
                this.view = item.value.clone();
                this.layout = Self::resolve_layout(
                    &this.services,
                    &this.client.profile().id,
                    &this.session.table,
                    &this.view,
                    this.schema.as_ref(),
                );
                this.rebuild_field_controls(window, cx);
                this.refresh_favorite_state(cx);
                window.close_dialog(cx);
                cx.notify();
            },
        ));
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("ServiceNow form view")
                .w(spx(420.0, cx))
                .child(Select::new(&state).placeholder("Choose a view").w_full())
        });
    }

    fn open_reference_picker(
        &mut self,
        field_name: String,
        target_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form_weak = cx.entity().downgrade();
        open_reference_dialog(
            self.services.clone(),
            self.client.clone(),
            target_table,
            move |picked, _, cx| {
                let _ = form_weak.update(cx, |form, cx| {
                    form.session.edit_field(
                        field_name.clone(),
                        FieldValue::new(picked.sys_id, picked.display_value),
                    );
                    cx.notify();
                });
            },
            window,
            cx,
        );
    }

    fn open_add_hidden_field_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form_entity = cx.entity().clone();
        let entries = self
            .layout
            .hidden_fields
            .iter()
            .map(|name| {
                let field = self
                    .schema
                    .as_ref()
                    .and_then(|schema| schema.fields.get(name));
                crate::field_picker::FieldOption {
                    name: name.clone(),
                    label: field
                        .map(|f| f.column_label.clone())
                        .unwrap_or_else(|| name.clone()),
                    source: field
                        .map(|f| f.defined_on_table.clone())
                        .unwrap_or_default(),
                    selected: false,
                }
            })
            .collect();
        let picker = cx.new(|cx| {
            crate::field_picker::FieldPicker::new(
                entries,
                move |name, window, cx| {
                    form_entity.update(cx, |form, cx| {
                        let span = form
                            .schema
                            .as_ref()
                            .and_then(|schema| schema.fields.get(name))
                            .map(|field| {
                                if field.field_type.prefers_full_width(name, field.max_length) {
                                    ColSpan::Two
                                } else {
                                    ColSpan::One
                                }
                            })
                            .unwrap_or(ColSpan::One);
                        form.layout.show_field(name, span);
                        form.rebuild_field_controls(window, cx);
                        form.persist_layout();
                        cx.notify();
                    });
                    true
                },
                window,
                cx,
            )
        });
        let first_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_focus.replace(false) {
                picker.update(cx, |picker, cx| picker.focus(window, cx));
            }
            dialog
                .w(spx(480.0, cx))
                .title("Add Hidden Field to Form Grid")
                .close_button(true)
                .overlay_closable(true)
                .child(picker.clone())
        });
    }

    fn favorite_target(&self) -> Option<ClassicTarget> {
        Some(ClassicTarget::Form {
            table: self.session.table.clone(),
            sys_id: self.session.sys_id.clone()?,
            query: String::new(),
            view: sn_core::classic::normalize_view(&self.view),
        })
    }

    /// Re-reads whether this record is a favorite, after favorites or the view changed.
    pub fn refresh_favorite_state(&mut self, cx: &mut Context<Self>) {
        let favorited = self.favorite_target().is_some_and(|target| {
            favorites::find(&self.services, &self.client.profile().id, &target).is_some()
        });
        if favorited != self.is_favorited {
            self.is_favorited = favorited;
            cx.notify();
        }
    }

    /// Adds or removes this record from the user's ServiceNow favorites. The star updates
    /// when the favorites tree reloads.
    pub fn toggle_favorite(&mut self, _cx: &mut Context<Self>) {
        let Some(target) = self.favorite_target() else {
            return;
        };
        match favorites::find(&self.services, &self.client.profile().id, &target) {
            Some(existing) => favorites::remove(&self.services, &self.client, existing),
            None => {
                let number = self.session.baseline.primary_identifier();
                let title = match self.session.baseline.summary_text() {
                    Some(summary) => {
                        let summary: String = summary.chars().take(60).collect();
                        format!("{number} · {summary}")
                    }
                    None => number,
                };
                favorites::add(&self.services, &self.client, title, target);
            }
        }
    }

    fn render_sticky_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let primary_id = self.tab_title();
        let summary = self
            .session
            .effective_display("short_description")
            .to_string();
        let dirty_count = self.session.dirty_field_count();
        let conflict_count = self.session.conflicts.len();
        let is_demo = self.services.is_demo_instance(&self.client.profile().id);

        let state_disp = self.session.effective_display("state").to_string();
        let prio_disp = self.session.effective_display("priority").to_string();

        v_flex()
            .px_5()
            .py_3()
            .gap_2()
            .bg(theme.sidebar.opacity(0.6))
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    // Left: Record Identifier + Status Badges + SWR Speed
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .overflow_hidden()
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(5.0))
                                    .bg(theme.primary.opacity(0.16))
                                    .font_family(theme.mono_font_family.clone())
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(theme.primary)
                                    .child(primary_id),
                            )
                            .when(!state_disp.is_empty(), |el| {
                                let (bg, bdr, fg) = state_badge_colors(&state_disp, cx);
                                el.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(bg)
                                        .border_1()
                                        .border_color(bdr)
                                        .text_size(sp(11.0))
                                        .font_medium()
                                        .text_color(fg)
                                        .child(state_disp),
                                )
                            })
                            .when(!prio_disp.is_empty(), |el| {
                                let (bg, bdr, fg) = priority_badge_colors(&prio_disp, cx);
                                el.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(bg)
                                        .border_1()
                                        .border_color(bdr)
                                        .text_size(sp(11.0))
                                        .font_medium()
                                        .text_color(fg)
                                        .child(prio_disp),
                                )
                            })
                            .when(dirty_count > 0, |el| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(hsla(38. / 360., 0.90, 0.55, 0.16))
                                        .text_size(sp(11.0))
                                        .font_medium()
                                        .text_color(rgb(0xFBBF24))
                                        .child(format!("● {dirty_count} unsaved")),
                                )
                            })
                            .when(conflict_count > 0, |el| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(theme.danger.opacity(0.18))
                                        .text_size(sp(11.0))
                                        .font_semibold()
                                        .text_color(theme.danger)
                                        .child(format!("⚠ {conflict_count} live conflict")),
                                )
                            })
                            .when_some(
                                self.swr_hydrated_ms
                                    .filter(|_| AppPrefs::get(cx).show_perf_telemetry),
                                |el, ms| {
                                el.child(
                                    div()
                                        .text_size(sp(10.5))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(rgb(0x34D399))
                                        .child(format!("⚡ {:.2}ms", ms)),
                                )
                            }),
                    )
                    // Right: Demo Push, Customize Grid, Revert, Save (Cmd+S), Update
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(Button::new("form-view").secondary().small()
                                .label(format!("View: {}", if self.view.is_empty() { "Default" } else { &self.view }))
                                .on_click(cx.listener(|this, _, window, cx| this.open_view_picker(window, cx))))
                            .when(is_demo && !self.session.is_new_record(), |el| {
                                el.child(
                                    Button::new("demo-live-push-btn")
                                        .secondary()
                                        .small()
                                        .icon(IconName::Zap)
                                        .label("Simulate Live Push")
                                        .on_click(cx.listener(|this, _, _, _| {
                                            if let Some(sid) = &this.session.sys_id {
                                                this.services
                                                    .trigger_demo_live_push(&this.session.table, sid);
                                            }
                                        })),
                                )
                            })
                            .when(!self.session.is_new_record(), |el| {
                                el.child(
                                    Button::new("fav-rec-btn")
                                        .ghost()
                                        .small()
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
                                            "Add this record to favorites"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.toggle_favorite(cx);
                                        })),
                                )
                                .child(
                                    Button::new("copy-sys-id-btn")
                                        .ghost()
                                        .small()
                                        .icon(IconName::Copy)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(sid) = &this.session.sys_id {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    sid.clone(),
                                                ));
                                                this.services.send_cmd(WorkspaceCommand::ToastInfo(
                                                    format!("Copied sys_id: {sid}"),
                                                ));
                                            }
                                        })),
                                )
                                .child(
                                    Button::new("open-in-browser-btn")
                                        .ghost()
                                        .small()
                                        .icon(IconName::ExternalLink)
                                        .on_click(cx.listener(|this, _, _, _| {
                                            if let Some(sid) = &this.session.sys_id {
                                                let url = this
                                                    .client
                                                    .profile()
                                                    .browser_record_url(&this.session.table, sid);
                                                let _ = open::that(url);
                                            }
                                        })),
                                )
                            })
                            .child(
                                Button::new("customize-grid-btn")
                                    .with_variant(if self.customize_grid_mode {
                                        gpui_kit::component::button::ButtonVariant::Info
                                    } else {
                                        gpui_kit::component::button::ButtonVariant::Secondary
                                    })
                                    .small()
                                    .icon(IconName::LayoutGrid)
                                    .label(if self.customize_grid_mode {
                                        "Done Grid"
                                    } else {
                                        "Customize Grid"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_customize_grid(cx);
                                    })),
                            )
                            .when(dirty_count > 0, |el| {
                                el.child(
                                    Button::new("discard-drafts-btn")
                                        .ghost()
                                        .small()
                                        .label("Discard")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.session.revert_all();
                                            this.work_notes_input
                                                .update(cx, |s, cx| s.set_value("", window, cx));
                                            this.comments_input
                                                .update(cx, |s, cx| s.set_value("", window, cx));
                                            this.sync_clean_controls_from_session(window, cx);
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("save-form-btn")
                                    .primary()
                                    .small()
                                    .icon(IconName::Check)
                                    .label(if self.is_saving {
                                        "Saving…"
                                    } else if self.session.is_new_record() {
                                        "Create (⌘S)"
                                    } else {
                                        "Save (⌘S)"
                                    })
                                    .disabled(self.is_saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save(false, window, cx);
                                    })),
                            )
                            .when(!self.session.is_new_record(), |el| {
                                el.child(
                                    Button::new("update-close-form-btn")
                                        .secondary()
                                        .small()
                                        .label("Update")
                                        .disabled(self.is_saving)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save(true, window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .when(!summary.is_empty(), |el| {
                el.child(
                    div()
                        .text_sm()
                        .font_medium()
                        .text_color(theme.foreground)
                        .truncate()
                        .child(summary),
                )
            })
            // Grid Customization Banner when Customize Mode is toggled on!
            .when(self.customize_grid_mode, |el| {
                el.child(
                    h_flex()
                        .p_2p5()
                        .mt_1()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .rounded(theme.radius)
                        .bg(theme.primary.opacity(0.12))
                        .border_1()
                        .border_color(theme.primary.opacity(0.4))
                        .child(
                            div()
                                .text_xs()
                                .child("Drag cards by ⋮⋮ to rearrange or search for fields. Customizations are saved per account, table and view."),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    div()
                                        .w(sp(170.0))
                                        .child(Input::new(&self.new_section_input).xsmall()),
                                )
                                .child(
                                    Button::new("add-divider-btn")
                                        .secondary()
                                        .xsmall()
                                        .icon(IconName::Plus)
                                        .label("Section")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let raw = this
                                                .new_section_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_string();
                                            let title = if raw.is_empty() {
                                                "Details".to_string()
                                            } else {
                                                raw
                                            };
                                            this.layout.add_section_divider(title, Some(0));
                                            this.new_section_input
                                                .update(cx, |s, cx| s.set_value("", window, cx));
                                            this.persist_layout();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("open-hidden-drawer-btn")
                                        .secondary()
                                        .xsmall()
                                        .icon(IconName::Search)
                                        .label(format!(
                                            "Add Field ({})",
                                            self.layout.hidden_fields.len()
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_add_hidden_field_dialog(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("reset-auto-grid-btn")
                                        .ghost()
                                        .xsmall()
                                        .label("Reset to SN View")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.reset_layout_to_default(cx);
                                        })),
                                ),
                        ),
                )
            })
    }

    fn render_field_card(
        &self,
        item_index: usize,
        element_name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let entry: Option<&DictionaryEntry> = self
            .schema
            .as_ref()
            .and_then(|s| s.fields.get(element_name));

        let label = entry
            .map(|e| e.column_label.clone())
            .unwrap_or_else(|| element_name.replace('_', " "));
        let mandatory = entry.is_some_and(|e| e.mandatory);
        let read_only = entry.is_some_and(|e| e.read_only) || element_name == "number";
        let is_dirty = self.session.is_field_dirty(element_name);
        let conflict: Option<FieldConflict> = self.session.conflicts.get(element_name).cloned();
        let is_pulsing = self
            .live_pulse_until
            .get(element_name)
            .is_some_and(|until| *until > Instant::now());
        let span = self.layout.field_span(element_name).unwrap_or(ColSpan::One);

        let border_col: Hsla = if conflict.is_some() {
            theme.danger
        } else if is_pulsing {
            rgb(0x34D399).into()
        } else if is_dirty {
            Hsla::from(rgb(0xFBBF24)).opacity(0.65)
        } else if self.customize_grid_mode {
            theme.primary.opacity(0.45)
        } else {
            theme.border
        };

        let card_bg = if is_pulsing {
            hsla(152. / 360., 0.72, 0.45, 0.10)
        } else {
            theme.sidebar.opacity(0.35)
        };

        let elem_owned = element_name.to_string();
        let elem_drop = element_name.to_string();
        let my_entity_id = cx.entity_id();

        let control_el = self.render_field_widget(element_name, read_only, cx);
        let copy_field = element_name.to_string();
        let compact = AppPrefs::get(cx).density == Density::Compact;

        v_flex()
            .id(("form-card", item_index))
            .w_full()
            .map(|card| {
                if compact {
                    card.px_2().py_1().gap_0p5()
                } else {
                    card.p_2p5().gap_1p5()
                }
            })
            .rounded(theme.radius)
            .bg(card_bg)
            .border_1()
            .border_color(border_col)
            // Accept native GPUI drag-over & drop in Customize Grid mode (or anytime!)
            .drag_over::<DraggedGridField>(move |style, dragged, _, cx| {
                if dragged.form_entity_id == my_entity_id {
                    style
                        .border_color(cx.theme().primary)
                        .bg(cx.theme().primary.opacity(0.12))
                } else {
                    style
                }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedGridField, _, cx| {
                if dragged.form_entity_id == my_entity_id && dragged.element != elem_drop {
                    this.layout
                        .move_field_relative(&dragged.element, &elem_drop, false);
                    this.persist_layout();
                    cx.notify();
                }
            }))
            // Field Header Row (Label + Mandatory `*` + Live Pulse / Dirty dot + Customize Controls)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Button::new(("copy-field", item_index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .tooltip("Copy value (Option-click for raw value)")
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, _, cx| {
                                            let value = if event.modifiers().alt {
                                                this.session.effective_raw(&copy_field)
                                            } else {
                                                this.session.effective_display(&copy_field)
                                            };
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                value.to_string(),
                                            ));
                                        },
                                    )),
                            )
                            .when(self.customize_grid_mode, |el| {
                                let drag_payload = DraggedGridField {
                                    form_entity_id: my_entity_id,
                                    element: elem_owned.clone(),
                                    label: label.clone(),
                                };
                                el.child(
                                    div()
                                        .id(("drag-grip", item_index))
                                        .cursor_grab()
                                        .on_drag(drag_payload, |drag, _, _, cx| {
                                            cx.new(|_| DraggedFieldGhost {
                                                label: drag.label.clone(),
                                            })
                                        })
                                        .child(
                                            Icon::new(IconName::GripVertical)
                                                .xsmall()
                                                .text_color(theme.primary),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(theme.muted_foreground)
                                    .child(label),
                            )
                            .when(mandatory, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .font_bold()
                                        .text_color(theme.danger)
                                        .child("*"),
                                )
                            })
                            .when(is_dirty, |el| {
                                el.child(
                                    div()
                                        .text_size(sp(10.0))
                                        .text_color(rgb(0xFBBF24))
                                        .child("edited"),
                                )
                            })
                            .when(is_pulsing, |el| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .rounded_full()
                                        .bg(hsla(152. / 360., 0.72, 0.48, 0.22))
                                        .text_size(sp(10.0))
                                        .font_semibold()
                                        .text_color(rgb(0x34D399))
                                        .child("● Live update"),
                                )
                            }),
                    )
                    .when(self.customize_grid_mode, |el| {
                        let f_up = elem_owned.clone();
                        let f_dn = elem_owned.clone();
                        let f_sp = elem_owned.clone();
                        let f_hd = elem_owned.clone();
                        el.child(
                            h_flex()
                                .gap_0p5()
                                .items_center()
                                .child(
                                    Button::new(("nudge-up", item_index))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::ArrowUp)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.layout.nudge_field(&f_up, -1);
                                            this.persist_layout();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(("nudge-dn", item_index))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::ArrowDown)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.layout.nudge_field(&f_dn, 1);
                                            this.persist_layout();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(("toggle-span", item_index))
                                        .secondary()
                                        .xsmall()
                                        .label(match span {
                                            ColSpan::One => "1 col",
                                            ColSpan::Two => "2 col",
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.layout.toggle_span(&f_sp);
                                            this.persist_layout();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(("hide-fld", item_index))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::EyeOff)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.layout.hide_field(&f_hd);
                                            this.persist_layout();
                                            cx.notify();
                                        })),
                                ),
                        )
                    }),
            )
            // Control Body
            .child(control_el)
            // Dirty-Field Conflict Resolution Banner
            .when_some(conflict, |el, conf| {
                let f_keep = conf.field.clone();
                let f_accept = conf.field.clone();
                let who = conf
                    .updated_by
                    .clone()
                    .unwrap_or_else(|| "remote user".to_string());
                el.child(
                    h_flex()
                        .mt_1()
                        .p_2()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        .rounded(px(5.0))
                        .bg(theme.danger.opacity(0.14))
                        .border_1()
                        .border_color(theme.danger.opacity(0.4))
                        .child(
                            div()
                                .text_size(sp(11.0))
                                .text_color(theme.foreground)
                                .child(format!(
                                    "Server changed to \"{}\" ({who})",
                                    conf.server_latest.effective_display()
                                )),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new(("conf-keep", item_index))
                                        .secondary()
                                        .xsmall()
                                        .label("Keep Mine")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.session.resolve_conflict_keep_local(&f_keep);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(("conf-srv", item_index))
                                        .primary()
                                        .xsmall()
                                        .label("Use Server")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.session.resolve_conflict_accept_server(&f_accept);
                                            this.sync_clean_controls_from_session(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_field_widget(
        &self,
        field_name: &str,
        read_only: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(ctrl) = self.controls.get(field_name) else {
            return div()
                .text_xs()
                .child(self.session.effective_display(field_name).to_string())
                .into_any_element();
        };

        match ctrl {
            FieldWidgetControl::SingleLine(inp) => Input::new(inp)
                .small()
                .readonly(read_only)
                .into_any_element(),
            FieldWidgetControl::MultiLine(ta) => Textarea::new(ta)
                .h(sp(88.0))
                .readonly(read_only)
                .into_any_element(),
            FieldWidgetControl::Choice { state } => Select::new(state)
                .small()
                .disabled(read_only)
                .into_any_element(),
            FieldWidgetControl::Boolean => {
                let raw = self.session.effective_raw(field_name);
                let checked = raw == "true" || raw == "1";
                let fname = field_name.to_string();
                h_flex()
                    .py_1()
                    .items_center()
                    .child(
                        Switch::new(SharedString::from(format!("sw-{field_name}")))
                            .small()
                            .checked(checked)
                            .disabled(read_only)
                            .label(if checked { "True" } else { "False" })
                            .on_change(cx.listener(move |this, next: &bool, _, cx| {
                                let s = if *next { "true" } else { "false" };
                                this.session
                                    .edit_field(fname.clone(), FieldValue::new(s, s));
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            FieldWidgetControl::Reference { target_table } => {
                let raw_id = self.session.effective_raw(field_name).to_string();
                let disp = self.session.effective_display(field_name).to_string();
                let fname_pick = field_name.to_string();
                let fname_clr = field_name.to_string();
                let target_pick = target_table.clone();
                let linkable = is_single_reference(&raw_id) && !disp.is_empty();

                // The value links to its record; the rest of the box opens the picker.
                let value_el = if linkable {
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .child(reference_link(
                            SharedString::from(format!("ref-link-{field_name}")),
                            &self.services,
                            &self.client,
                            target_table,
                            &raw_id,
                            &disp,
                            true,
                            cx,
                        ))
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .truncate()
                        .text_color(if disp.is_empty() {
                            theme.muted_foreground
                        } else {
                            theme.foreground
                        })
                        .child(if disp.is_empty() {
                            format!("Select {target_table}…")
                        } else {
                            disp
                        })
                        .into_any_element()
                };

                let picker_selector = format!("ref-pick:{field_name}");
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("ref-trig-{field_name}")))
                            .debug_selector(move || picker_selector)
                            .flex_1()
                            .min_w_0()
                            .h(sp(28.0))
                            .px_2p5()
                            .gap_2()
                            .justify_between()
                            .items_center()
                            .rounded(theme.radius)
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.input)
                            .when(!read_only, |el| {
                                el.cursor_pointer()
                                    .hover(|s| s.border_color(theme.primary))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_reference_picker(
                                            fname_pick.clone(),
                                            target_pick.clone(),
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .child(value_el)
                            .when(!read_only, |el| {
                                el.child(
                                    Icon::new(IconName::Search)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                )
                            }),
                    )
                    .when(!raw_id.is_empty() && !read_only, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("ref-clr-{field_name}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::X)
                                .tooltip("Clear")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session
                                        .edit_field(fname_clr.clone(), FieldValue::default());
                                    cx.notify();
                                })),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    fn render_activity_stream(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_work_notes = self.active_journal_tab == "work_notes";

        v_flex()
            .mt_2()
            .p_4()
            .gap_3()
            .rounded(theme.radius_lg)
            .bg(theme.sidebar.opacity(0.4))
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child("Activity & Journal Stream"),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(theme.secondary)
                                    .text_size(sp(10.5))
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} entries", self.session.journals.len())),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("jrn-tab-wn")
                                    .with_variant(if is_work_notes {
                                        gpui_kit::component::button::ButtonVariant::Warning
                                    } else {
                                        gpui_kit::component::button::ButtonVariant::Ghost
                                    })
                                    .xsmall()
                                    .label("Work Notes (Internal)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.active_journal_tab = "work_notes";
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("jrn-tab-cm")
                                    .with_variant(if !is_work_notes {
                                        gpui_kit::component::button::ButtonVariant::Info
                                    } else {
                                        gpui_kit::component::button::ButtonVariant::Ghost
                                    })
                                    .xsmall()
                                    .label("Additional Comments")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.active_journal_tab = "comments";
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            // Composer box
            .child(if is_work_notes {
                div()
                    .p_1()
                    .rounded(theme.radius)
                    .border_l_2()
                    .border_color(rgb(0xFBBF24))
                    .child(Textarea::new(&self.work_notes_input).h(sp(74.0)))
            } else {
                div()
                    .p_1()
                    .rounded(theme.radius)
                    .border_l_2()
                    .border_color(rgb(0x38BDF8))
                    .child(Textarea::new(&self.comments_input).h(sp(74.0)))
            })
            // Chronological Live Timeline
            .children(self.session.journals.iter().map(|j| {
                let accent: Hsla = if j.is_work_note() {
                    rgb(0xFBBF24).into()
                } else {
                    rgb(0x38BDF8).into()
                };
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded(theme.radius)
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .border_l_2()
                    .border_color(accent)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_semibold()
                                            .child(j.sys_created_by.clone()),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .rounded(px(3.0))
                                            .bg(accent.opacity(0.16))
                                            .text_size(sp(10.0))
                                            .text_color(accent)
                                            .child(if j.is_work_note() {
                                                "Work note"
                                            } else {
                                                "Comment"
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(sp(11.0))
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
                                    .child(j.sys_created_on.clone()),
                            ),
                    )
                    .child(div().text_xs().child(j.value.clone()))
            }))
    }
}

impl Render for FormViewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let visual_rows = self.layout.compute_visual_rows();
        let has_journals =
            !self.layout.journal_fields.is_empty() || !self.session.journals.is_empty();
        let compact = AppPrefs::get(cx).density == Density::Compact;

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(self.render_sticky_header(cx))
            .child(
                v_flex()
                    .id("form-main-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .map(|grid| {
                        if compact {
                            grid.gap_1p5()
                        } else {
                            grid.gap_3()
                        }
                    })
                    // Packed 2-Column Rearrangeable Auto-Grid
                    .children(visual_rows.into_iter().map(|row| {
                        match row {
                            GridVisualRow::Divider {
                                item_index,
                                id,
                                title,
                                collapsed,
                            } => {
                                let div_toggle = id.clone();
                                let div_rm = id;
                                h_flex()
                                    .id(("sec-div", item_index))
                                    .pt_2()
                                    .pb_1()
                                    .justify_between()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .child(
                                        h_flex()
                                            .id(("sec-div-tog", item_index))
                                            .gap_1p5()
                                            .items_center()
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.layout.toggle_section_collapsed(&div_toggle);
                                                this.persist_layout();
                                                cx.notify();
                                            }))
                                            .child(
                                                Icon::new(if collapsed {
                                                    IconName::ChevronRight
                                                } else {
                                                    IconName::ChevronDown
                                                })
                                                .xsmall()
                                                .text_color(theme.muted_foreground),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_semibold()
                                                    .text_color(theme.foreground)
                                                    .child(title),
                                            ),
                                    )
                                    .when(self.customize_grid_mode, |el| {
                                        el.child(
                                            Button::new(("rm-sec-div", item_index))
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::Trash)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.layout.remove_section_divider(&div_rm);
                                                    this.persist_layout();
                                                    cx.notify();
                                                })),
                                        )
                                    })
                                    .into_any_element()
                            }
                            GridVisualRow::FullWidthField {
                                item_index,
                                element,
                            } => self
                                .render_field_card(item_index, &element, cx)
                                .into_any_element(),
                            GridVisualRow::TwoColumnPair { left, right } => {
                                let left_el = self.render_field_card(left.0, &left.1, cx);
                                let right_el = right
                                    .map(|(rix, rname)| self.render_field_card(rix, &rname, cx))
                                    .unwrap_or_else(|| div().w_full().into_any_element());
                                h_flex()
                                    .w_full()
                                    .map(|row| if compact { row.gap_1p5() } else { row.gap_3() })
                                    .items_start()
                                    .child(div().flex_1().min_w_0().child(left_el))
                                    .child(div().flex_1().min_w_0().child(right_el))
                                    .into_any_element()
                            }
                        }
                    }))
                    // Activity Stream & Journal Composer
                    .when(has_journals && !self.session.is_new_record(), |el| {
                        el.child(self.render_activity_stream(cx))
                    })
                    // Collapsible System Audit Metadata Footer
                    .when(!self.session.is_new_record(), |el| {
                        let sys_id = self.session.sys_id.clone().unwrap_or_default();
                        let updated_on =
                            self.session.effective_display("sys_updated_on").to_string();
                        let updated_by =
                            self.session.effective_display("sys_updated_by").to_string();
                        let created_on =
                            self.session.effective_display("sys_created_on").to_string();
                        let mod_count = self.session.baseline.sys_mod_count;

                        el.child(
                            v_flex()
                                .mt_2()
                                .p_2p5()
                                .gap_2()
                                .rounded(theme.radius)
                                .bg(theme.sidebar.opacity(0.25))
                                .border_1()
                                .border_color(theme.border.opacity(0.6))
                                .child(
                                    h_flex()
                                        .id("toggle-sys-meta")
                                        .justify_between()
                                        .items_center()
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_system_metadata = !this.show_system_metadata;
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .text_size(sp(11.0))
                                                .font_medium()
                                                .text_color(theme.muted_foreground)
                                                .child("System Audit Metadata"),
                                        )
                                        .child(
                                            Icon::new(if self.show_system_metadata {
                                                IconName::ChevronUp
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .xsmall()
                                            .text_color(theme.muted_foreground),
                                        ),
                                )
                                .when(self.show_system_metadata, |meta| {
                                    meta.child(
                                        h_flex()
                                            .flex_wrap()
                                            .gap_4()
                                            .text_size(sp(11.0))
                                            .font_family(theme.mono_font_family.clone())
                                            .text_color(theme.muted_foreground)
                                            .children(
                                                [
                                                    ("sys_id", sys_id),
                                                    ("updates", mod_count.to_string()),
                                                    ("updated", updated_on),
                                                    ("updated by", updated_by),
                                                    ("created", created_on),
                                                    (
                                                        "created by",
                                                        self.session
                                                            .effective_display("sys_created_by")
                                                            .to_string(),
                                                    ),
                                                ]
                                                .into_iter()
                                                .enumerate()
                                                .map(|(ix, (name, value))| {
                                                    Button::new(("copy-audit", ix))
                                                        .ghost()
                                                        .xsmall()
                                                        .label(format!("{name}: {value}"))
                                                        .icon(IconName::Copy)
                                                        .on_click(move |_, _, cx| {
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(
                                                                    value.clone(),
                                                                ),
                                                            )
                                                        })
                                                }),
                                            ),
                                    )
                                }),
                        )
                    }),
            )
    }
}
