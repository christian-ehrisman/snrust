//! Condition builder for list filters.
//!
//! Edits an [`EncodedQuery`] as one row per condition, like the classic filter builder:
//! field, operator and value, joined by AND, OR, or a new set of conditions (`^NQ`). It
//! expands inline under the list toolbar, so the table moves down rather than being covered.
//! Conditions it cannot model are shown as editable text rows and passed through untouched.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{IndexPath, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::SnClient;
use sn_core::{
    Condition, EncodedQuery, FieldType, FilterClause, FilterOperator, Joiner, SortSpec, TableSchema,
};

use crate::state::AppServices;
use crate::theme::sp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterBuilderEvent {
    Run(EncodedQuery),
    /// Save the built filter under a name.
    Save(EncodedQuery),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PickItem {
    value: String,
    label: String,
}

impl SearchableListItem for PickItem {
    type Value = Self;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

type Picker = Entity<SelectState<SearchableVec<PickItem>>>;

enum ValueEditor {
    /// The operator takes no value (`is empty`, `is anything`).
    None,
    Text(Entity<InputState>),
    /// A fixed set: choice labels, or true/false.
    Pick(Picker),
    Reference {
        target_table: String,
    },
}

enum RowBody {
    Clause {
        field: String,
        operator: FilterOperator,
        value: String,
        display_hint: Option<String>,
        field_picker: Picker,
        operator_picker: Picker,
        editor: ValueEditor,
    },
    /// A condition the builder does not model, edited as encoded-query text.
    Raw(Entity<InputState>),
}

struct Row {
    id: u64,
    joiner: Joiner,
    body: RowBody,
    _subscriptions: Vec<Subscription>,
}

pub struct FilterBuilder {
    services: AppServices,
    client: SnClient,
    schema: TableSchema,
    rows: Vec<Row>,
    sorts: Vec<SortSpec>,
    sort_picker: Picker,
    next_row_id: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FilterBuilderEvent> for FilterBuilder {}

/// How the value of a condition is entered, given its field type and operator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorKind {
    None,
    Text,
    Choices,
    Boolean,
    Reference(String),
}

fn editor_kind(field_type: &FieldType, operator: FilterOperator) -> EditorKind {
    if !operator.takes_value() {
        return EditorKind::None;
    }
    let exact = matches!(operator, FilterOperator::Equals | FilterOperator::NotEquals);
    match field_type {
        FieldType::Choice | FieldType::SysClassName if exact => EditorKind::Choices,
        FieldType::Boolean if exact => EditorKind::Boolean,
        FieldType::Reference { target_table } if exact => {
            EditorKind::Reference(target_table.clone())
        }
        _ => EditorKind::Text,
    }
}

fn value_placeholder(field_type: &FieldType, operator: FilterOperator) -> &'static str {
    match (field_type, operator) {
        (_, FilterOperator::In | FilterOperator::NotIn) => "Comma-separated values",
        (_, FilterOperator::Between) => "from@to",
        (FieldType::Date | FieldType::DateTime, _) => {
            "YYYY-MM-DD, or javascript:gs.daysAgoStart(7)"
        }
        (_, FilterOperator::SameAs | FilterOperator::NotSameAs) => "Another field's name",
        _ => "Value",
    }
}

impl FilterBuilder {
    pub fn new(
        services: AppServices,
        client: SnClient,
        schema: TableSchema,
        query: &EncodedQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sort_picker = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        let sort_subscription = cx.subscribe_in(
            &sort_picker,
            window,
            |this: &mut Self, _, event: &SelectEvent<SearchableVec<PickItem>>, _, cx| {
                let SelectEvent::Confirm(choice) = event;
                let descending = this.sorts.first().is_some_and(|sort| sort.descending);
                match choice {
                    Some(item) => {
                        let sort = SortSpec {
                            field: item.value.clone(),
                            descending,
                        };
                        match this.sorts.first_mut() {
                            Some(first) => *first = sort,
                            None => this.sorts.push(sort),
                        }
                    }
                    None => this.sorts.clear(),
                }
                cx.notify();
            },
        );
        let mut builder = Self {
            services,
            client,
            schema,
            rows: Vec::new(),
            sorts: Vec::new(),
            sort_picker,
            next_row_id: 0,
            _subscriptions: vec![sort_subscription],
        };
        builder.set_query(query, window, cx);
        builder
    }

    /// Replaces the builder's contents, for when the list's filter changes elsewhere.
    pub fn set_query(&mut self, query: &EncodedQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.clear();
        for (joiner, condition) in query.flatten() {
            let row = self.build_row(joiner, condition, window, cx);
            self.rows.push(row);
        }
        self.sorts = query.sorts.clone();
        self.sync_sort_picker(window, cx);
        cx.notify();
    }

    fn field_type(&self, field: &str) -> FieldType {
        self.schema
            .fields
            .get(field)
            .map(|entry| entry.field_type.clone())
            .unwrap_or(FieldType::String)
    }

    fn field_label(&self, field: &str) -> String {
        let instance_id = self.client.profile().id.clone();
        self.schema
            .dot_walk_label(field, |table| {
                self.services
                    .cache
                    .get_table_schema(&instance_id, table)
                    .ok()
                    .flatten()
                    .map(|cached| cached.data)
            })
            .unwrap_or_else(|| field.to_string())
    }

    /// Every field of the table, plus `current` when it is a dot-walked path so that an
    /// existing condition on `caller_id.email` stays selected and editable.
    fn field_items(&self, current: &str) -> Vec<PickItem> {
        let mut items: Vec<PickItem> = self
            .schema
            .fields
            .values()
            .map(|field| PickItem {
                value: field.element.clone(),
                label: format!("{}  ·  {}", field.column_label, field.element),
            })
            .collect();
        items.sort_by_key(|item| item.label.to_lowercase());
        if !current.is_empty() && !self.schema.fields.contains_key(current) {
            items.insert(
                0,
                PickItem {
                    value: current.to_string(),
                    label: format!("{}  ·  {current}", self.field_label(current)),
                },
            );
        }
        items
    }

    fn sync_sort_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .sorts
            .first()
            .map(|sort| sort.field.clone())
            .unwrap_or_default();
        let items = self.field_items(&current);
        let selected = items.iter().find(|item| item.value == current).cloned();
        self.sort_picker.update(cx, |picker, cx| {
            picker.set_items(SearchableVec::new(items), window, cx);
            match &selected {
                Some(item) => picker.set_selected_value(item, window, cx),
                None => picker.set_selected_index(None, window, cx),
            }
        });
    }

    fn make_picker(
        items: Vec<PickItem>,
        selected: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Picker {
        let index = selected
            .and_then(|value| items.iter().position(|item| item.value == value))
            .map(|row| IndexPath::default().row(row));
        cx.new(|cx| SelectState::new(SearchableVec::new(items), index, window, cx).searchable(true))
    }

    fn build_editor(
        &self,
        row_id: u64,
        clause: &FilterClause,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ValueEditor {
        let (field, operator, value) = (
            clause.field.as_str(),
            clause.operator,
            clause.value.as_str(),
        );
        let field_type = self.field_type(field);
        let pick = |items: Vec<PickItem>,
                    subscriptions: &mut Vec<Subscription>,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            let picker = Self::make_picker(items, Some(value), window, cx);
            subscriptions.push(cx.subscribe_in(
                &picker,
                window,
                move |this: &mut Self, _, event: &SelectEvent<SearchableVec<PickItem>>, _, cx| {
                    let SelectEvent::Confirm(choice) = event;
                    this.set_value(
                        row_id,
                        choice
                            .as_ref()
                            .map(|item| item.value.clone())
                            .unwrap_or_default(),
                        choice.as_ref().map(|item| item.label.clone()),
                        cx,
                    );
                },
            ));
            ValueEditor::Pick(picker)
        };
        match editor_kind(&field_type, operator) {
            EditorKind::None => ValueEditor::None,
            EditorKind::Choices => {
                let mut items: Vec<PickItem> = self
                    .schema
                    .choices_for(field, None)
                    .into_iter()
                    .map(|choice| PickItem {
                        value: choice.value.clone(),
                        label: choice.label.clone(),
                    })
                    .collect();
                // A value outside the choice list (inactive choice, script) must stay visible.
                if !value.is_empty() && !items.iter().any(|item| item.value == value) {
                    items.insert(
                        0,
                        PickItem {
                            value: value.to_string(),
                            label: value.to_string(),
                        },
                    );
                }
                pick(items, subscriptions, window, cx)
            }
            EditorKind::Boolean => pick(
                vec![
                    PickItem {
                        value: "true".into(),
                        label: "true".into(),
                    },
                    PickItem {
                        value: "false".into(),
                        label: "false".into(),
                    },
                ],
                subscriptions,
                window,
                cx,
            ),
            EditorKind::Reference(target_table) => ValueEditor::Reference { target_table },
            EditorKind::Text => {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(value.to_string())
                        .placeholder(value_placeholder(&field_type, operator))
                });
                subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    move |this: &mut Self, input, event, _, cx| match event {
                        InputEvent::Change => {
                            let text = input.read(cx).value().to_string();
                            this.set_value(row_id, text, None, cx);
                        }
                        InputEvent::PressEnter { .. } => this.run(cx),
                        _ => {}
                    },
                ));
                ValueEditor::Text(input)
            }
        }
    }

    fn build_row(
        &mut self,
        joiner: Joiner,
        condition: Condition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Row {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let mut subscriptions = Vec::new();
        let body = match condition {
            Condition::Raw(raw) => {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(raw)
                        .placeholder("Encoded query condition")
                });
                subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    |this: &mut Self, _, event, _, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.run(cx);
                        }
                    },
                ));
                RowBody::Raw(input)
            }
            Condition::Clause(clause) => {
                let field_picker = Self::make_picker(
                    self.field_items(&clause.field),
                    Some(&clause.field),
                    window,
                    cx,
                );
                subscriptions.push(cx.subscribe_in(
                    &field_picker,
                    window,
                    move |this: &mut Self,
                          _,
                          event: &SelectEvent<SearchableVec<PickItem>>,
                          window,
                          cx| {
                        let SelectEvent::Confirm(Some(item)) = event else {
                            return;
                        };
                        this.set_field(id, item.value.clone(), window, cx);
                    },
                ));
                let operator_picker = self.build_operator_picker(
                    id,
                    &clause.field,
                    clause.operator,
                    &mut subscriptions,
                    window,
                    cx,
                );
                let editor = self.build_editor(id, &clause, &mut subscriptions, window, cx);
                RowBody::Clause {
                    field: clause.field,
                    operator: clause.operator,
                    value: clause.value,
                    display_hint: clause.display_hint,
                    field_picker,
                    operator_picker,
                    editor,
                }
            }
        };
        Row {
            id,
            joiner,
            body,
            _subscriptions: subscriptions,
        }
    }

    fn build_operator_picker(
        &self,
        row_id: u64,
        field: &str,
        operator: FilterOperator,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Picker {
        let mut operators = FilterOperator::for_field_type(&self.field_type(field)).to_vec();
        // An operator from an existing query stays selectable even if unusual for the type.
        if !operators.contains(&operator) {
            operators.insert(0, operator);
        }
        let items: Vec<PickItem> = operators
            .into_iter()
            .map(|operator| PickItem {
                value: operator.sn_token().to_string(),
                label: operator.label().to_string(),
            })
            .collect();
        let picker = Self::make_picker(items, Some(operator.sn_token()), window, cx);
        subscriptions.push(cx.subscribe_in(
            &picker,
            window,
            move |this: &mut Self, _, event: &SelectEvent<SearchableVec<PickItem>>, window, cx| {
                let SelectEvent::Confirm(Some(item)) = event else {
                    return;
                };
                if let Some(operator) = FilterOperator::all()
                    .iter()
                    .copied()
                    .find(|operator| operator.sn_token() == item.value)
                {
                    this.set_operator(row_id, operator, window, cx);
                }
            },
        ));
        picker
    }

    fn row_index(&self, row_id: u64) -> Option<usize> {
        self.rows.iter().position(|row| row.id == row_id)
    }

    /// Rebuilds a clause row in place after its field or operator changed, keeping its
    /// position, joiner and (where still meaningful) its value.
    fn replace_clause(
        &mut self,
        row_id: u64,
        clause: FilterClause,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.row_index(row_id) else {
            return;
        };
        let joiner = self.rows[ix].joiner;
        let mut row = self.build_row(joiner, Condition::Clause(clause), window, cx);
        // Subscriptions were registered under the new row's id; adopt it.
        std::mem::swap(&mut self.rows[ix], &mut row);
        cx.notify();
    }

    fn set_field(
        &mut self,
        row_id: u64,
        field: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.row_index(row_id) else {
            return;
        };
        let RowBody::Clause {
            field: current,
            operator,
            ..
        } = &self.rows[ix].body
        else {
            return;
        };
        if *current == field {
            return;
        }
        let allowed = FilterOperator::for_field_type(&self.field_type(&field));
        let operator = if allowed.contains(operator) {
            *operator
        } else {
            allowed[0]
        };
        // A value for the old field rarely means anything for the new one.
        self.replace_clause(row_id, FilterClause::new(field, operator, ""), window, cx);
    }

    fn set_operator(
        &mut self,
        row_id: u64,
        operator: FilterOperator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.row_index(row_id) else {
            return;
        };
        let RowBody::Clause {
            field,
            operator: current,
            value,
            display_hint,
            ..
        } = &self.rows[ix].body
        else {
            return;
        };
        if *current == operator {
            return;
        }
        let field_type = self.field_type(field);
        let same_editor = editor_kind(&field_type, *current) == editor_kind(&field_type, operator);
        let mut clause = FilterClause::new(
            field.clone(),
            operator,
            if same_editor {
                value.clone()
            } else {
                String::new()
            },
        );
        if same_editor {
            clause.display_hint = display_hint.clone();
        }
        self.replace_clause(row_id, clause, window, cx);
    }

    fn set_value(
        &mut self,
        row_id: u64,
        new_value: String,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.row_index(row_id)
            && let RowBody::Clause {
                value,
                display_hint,
                ..
            } = &mut self.rows[ix].body
        {
            *value = new_value;
            *display_hint = hint;
            cx.notify();
        }
    }

    /// Adds an empty condition after row `after` (or at the end).
    pub fn add_condition(
        &mut self,
        joiner: Joiner,
        after: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (field, operator) = match after.and_then(|id| self.row_index(id)) {
            // An OR alternative usually tests the same field again.
            Some(ix) if joiner == Joiner::Or => match &self.rows[ix].body {
                RowBody::Clause {
                    field, operator, ..
                } => (field.clone(), *operator),
                RowBody::Raw(_) => (String::new(), FilterOperator::Equals),
            },
            _ => (String::new(), FilterOperator::Equals),
        };
        let row = self.build_row(
            joiner,
            Condition::Clause(FilterClause::new(field, operator, "")),
            window,
            cx,
        );
        let at = after
            .and_then(|id| self.row_index(id))
            .map(|ix| {
                // Keep a condition's OR alternatives together: insert after the last of them.
                let mut end = ix + 1;
                if joiner != Joiner::Or {
                    while self
                        .rows
                        .get(end)
                        .is_some_and(|row| row.joiner == Joiner::Or)
                    {
                        end += 1;
                    }
                }
                end
            })
            .unwrap_or(self.rows.len());
        self.rows.insert(at, row);
        cx.notify();
    }

    fn remove_row(&mut self, row_id: u64, cx: &mut Context<Self>) {
        let Some(ix) = self.row_index(row_id) else {
            return;
        };
        let removed = self.rows.remove(ix);
        // The condition that takes its place inherits how the removed one was joined, so
        // deleting the first of a set does not merge two sets or orphan an OR.
        if removed.joiner != Joiner::Or
            && let Some(next) = self.rows.get_mut(ix)
            && next.joiner == Joiner::Or
        {
            next.joiner = removed.joiner;
        }
        cx.notify();
    }

    /// The query as currently built. Conditions without a field, and empty text rows, are
    /// left out.
    pub fn to_query(&self, cx: &App) -> EncodedQuery {
        let flat = self.rows.iter().filter_map(|row| {
            let condition = match &row.body {
                RowBody::Raw(input) => {
                    let raw = input.read(cx).value().trim().to_string();
                    (!raw.is_empty()).then_some(Condition::Raw(raw))?
                }
                RowBody::Clause {
                    field,
                    operator,
                    value,
                    display_hint,
                    ..
                } => {
                    if field.is_empty() {
                        return None;
                    }
                    let mut clause = FilterClause::new(field.clone(), *operator, value.clone());
                    clause.display_hint = display_hint.clone();
                    Condition::Clause(clause)
                }
            };
            Some((row.joiner, condition))
        });
        EncodedQuery::from_flat(flat, self.sorts.clone())
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let query = self.to_query(cx);
        cx.emit(FilterBuilderEvent::Run(query));
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.clear();
        self.sorts.clear();
        self.sync_sort_picker(window, cx);
        self.run(cx);
    }

    fn pick_reference(
        &mut self,
        row_id: u64,
        target_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let builder = cx.entity().downgrade();
        crate::form_view::open_reference_dialog(
            self.services.clone(),
            self.client.clone(),
            target_table,
            move |picked, _, cx| {
                let _ = builder.update(cx, |builder, cx| {
                    builder.set_value(row_id, picked.sys_id, Some(picked.display_value), cx);
                });
            },
            window,
            cx,
        );
    }

    #[cfg(test)]
    pub fn run_now(&mut self, cx: &mut Context<Self>) {
        self.run(cx);
    }

    #[cfg(test)]
    pub fn row_ids(&self) -> Vec<u64> {
        self.rows.iter().map(|row| row.id).collect()
    }

    #[cfg(test)]
    pub fn choose_field(
        &mut self,
        row_id: u64,
        field: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_field(row_id, field.to_string(), window, cx);
    }

    #[cfg(test)]
    pub fn choose_operator(
        &mut self,
        row_id: u64,
        operator: FilterOperator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_operator(row_id, operator, window, cx);
    }

    #[cfg(test)]
    pub fn choose_value(&mut self, row_id: u64, value: &str, cx: &mut Context<Self>) {
        self.set_value(row_id, value.to_string(), None, cx);
    }

    #[cfg(test)]
    pub fn delete_row(&mut self, row_id: u64, cx: &mut Context<Self>) {
        self.remove_row(row_id, cx);
    }

    #[cfg(test)]
    pub fn editor_kinds(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .map(|row| match &row.body {
                RowBody::Raw(_) => "raw",
                RowBody::Clause { editor, .. } => match editor {
                    ValueEditor::None => "none",
                    ValueEditor::Text(_) => "text",
                    ValueEditor::Pick(_) => "pick",
                    ValueEditor::Reference { .. } => "reference",
                },
            })
            .collect()
    }
}

impl Render for FilterBuilder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sort = self.sorts.first().cloned();

        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let row_id = row.id;
                let joiner_label = match (ix, row.joiner) {
                    (0, _) => "",
                    (_, Joiner::And) => "AND",
                    (_, Joiner::Or) => "OR",
                    (_, Joiner::NewQuery) => "",
                };
                let body = match &row.body {
                    RowBody::Raw(input) => h_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().flex_1().child(Input::new(input).small()))
                        .into_any_element(),
                    RowBody::Clause {
                        value,
                        display_hint,
                        field_picker,
                        operator_picker,
                        editor,
                        ..
                    } => h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .items_center()
                        .child(
                            div().w(sp(230.0)).flex_shrink_0().child(
                                Select::new(field_picker)
                                    .small()
                                    .placeholder("Choose field")
                                    .w_full(),
                            ),
                        )
                        .child(
                            div()
                                .w(sp(160.0))
                                .flex_shrink_0()
                                .child(Select::new(operator_picker).small().w_full()),
                        )
                        .child(div().flex_1().min_w_0().map(|cell| {
                            match editor {
                                ValueEditor::None => cell,
                                ValueEditor::Text(input) => cell.child(Input::new(input).small()),
                                ValueEditor::Pick(picker) => cell.child(
                                    Select::new(picker)
                                        .small()
                                        .placeholder("Choose value")
                                        .w_full(),
                                ),
                                ValueEditor::Reference { target_table } => {
                                    let target = target_table.clone();
                                    let shown = display_hint
                                        .clone()
                                        .filter(|hint| !hint.is_empty())
                                        .unwrap_or_else(|| value.clone());
                                    cell.child(
                                        Button::new(("filter-ref", row_id as usize))
                                            .secondary()
                                            .small()
                                            .w_full()
                                            .icon(IconName::Search)
                                            .label(if shown.is_empty() {
                                                format!("Select {target}…")
                                            } else {
                                                shown
                                            })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.pick_reference(
                                                    row_id,
                                                    target.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                }
                            }
                        }))
                        .into_any_element(),
                };
                v_flex()
                    .when(ix > 0 && row.joiner == Joiner::NewQuery, |wrap| {
                        wrap.child(
                            h_flex()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .child(div().h(px(1.0)).flex_1().bg(theme.border))
                                .child(
                                    div()
                                        .text_size(sp(10.5))
                                        .font_semibold()
                                        .text_color(theme.muted_foreground)
                                        .child("OR ALL OF THESE CONDITIONS"),
                                )
                                .child(div().h(px(1.0)).flex_1().bg(theme.border)),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                div()
                                    .w(sp(34.0))
                                    .flex_shrink_0()
                                    .text_size(sp(10.5))
                                    .font_semibold()
                                    .text_color(if row.joiner == Joiner::Or {
                                        theme.primary
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(joiner_label),
                            )
                            .child(body)
                            .child(
                                Button::new(("filter-and", row_id as usize))
                                    .ghost()
                                    .xsmall()
                                    .label("AND")
                                    .tooltip("Add a condition that must also match")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.add_condition(Joiner::And, Some(row_id), window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("filter-or", row_id as usize))
                                    .ghost()
                                    .xsmall()
                                    .label("OR")
                                    .tooltip("Add an alternative to this condition")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.add_condition(Joiner::Or, Some(row_id), window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("filter-remove", row_id as usize))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::X)
                                    .tooltip("Remove condition")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_row(row_id, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        let has_rows = !rows.is_empty();

        v_flex()
            .id("filter-builder")
            .debug_selector(|| "filter-builder".into())
            .p_2p5()
            .gap_1p5()
            .rounded(theme.radius)
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "enter" if event.keystroke.modifiers.platform => this.run(cx),
                    "escape" => cx.emit(FilterBuilderEvent::Close),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .when(!has_rows, |builder| {
                builder.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("No conditions: every record is shown. Add one to filter."),
                )
            })
            .children(rows)
            .child(
                h_flex()
                    .pt_1()
                    .gap_1p5()
                    .flex_wrap()
                    .items_center()
                    .child(
                        Button::new("filter-add-condition")
                            .secondary()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("Condition")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_condition(Joiner::And, None, window, cx);
                            })),
                    )
                    .when(has_rows, |bar| {
                        bar.child(
                            Button::new("filter-add-set")
                                .secondary()
                                .xsmall()
                                .icon(IconName::Plus)
                                .label("OR conditions")
                                .tooltip(
                                    "Start a second set of conditions; records matching either \
                                     set are shown",
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_condition(Joiner::NewQuery, None, window, cx);
                                })),
                        )
                    })
                    .child(
                        div()
                            .pl_2()
                            .text_size(sp(11.0))
                            .text_color(theme.muted_foreground)
                            .child("Sort by"),
                    )
                    .child(
                        div().w(sp(200.0)).child(
                            Select::new(&self.sort_picker)
                                .small()
                                .placeholder("Default order")
                                .cleanable(true)
                                .w_full(),
                        ),
                    )
                    .when_some(sort, |bar, sort| {
                        bar.child(
                            Button::new("filter-sort-direction")
                                .ghost()
                                .xsmall()
                                .label(if sort.descending {
                                    "Z → A"
                                } else {
                                    "A → Z"
                                })
                                .tooltip("Switch between ascending and descending")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(first) = this.sorts.first_mut() {
                                        first.descending = !first.descending;
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("filter-clear")
                            .ghost()
                            .xsmall()
                            .label("Clear all")
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    )
                    .child(
                        Button::new("filter-save")
                            .secondary()
                            .xsmall()
                            .label("Save…")
                            .tooltip("Save this filter as a pill above the list")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let query = this.to_query(cx);
                                cx.emit(FilterBuilderEvent::Save(query));
                            })),
                    )
                    .child(
                        Button::new("filter-run")
                            .primary()
                            .xsmall()
                            .label("Run  ⌘↵")
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    ),
            )
    }
}
