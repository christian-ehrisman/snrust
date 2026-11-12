//! Searchable field chooser used by form customization ("Add Field").

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::column_panel::field_row_text;
use crate::theme::sp;

pub struct FieldOption {
    pub name: String,
    pub label: String,
    pub source: String,
    pub selected: bool,
}

/// Returns whether the field was added.
type OnChoose = Box<dyn Fn(&str, &mut Window, &mut App) -> bool>;

pub struct FieldPicker {
    search: Entity<InputState>,
    entries: Vec<FieldOption>,
    on_choose: OnChoose,
    _subscription: Subscription,
}

impl FieldPicker {
    pub fn new(
        mut entries: Vec<FieldOption>,
        on_choose: impl Fn(&str, &mut Window, &mut App) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        entries.sort_by_key(|entry| entry.label.to_lowercase());
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search field label, name, or parent table…")
        });
        let subscription = cx.subscribe(&search, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            search,
            entries,
            on_choose: Box::new(on_choose),
            _subscription: subscription,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.search.focus_handle(cx).focus(window, cx);
    }
}

impl Render for FieldPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let matches: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.split_whitespace().all(|word| {
                    format!("{} {} {}", entry.label, entry.name, entry.source)
                        .to_lowercase()
                        .contains(word)
                })
            })
            .collect();
        let count = matches.len();
        let rows: Vec<_> = matches
            .into_iter()
            .map(|(ix, entry)| {
                let added = entry.selected;
                h_flex()
                    .id(("choose-field", ix))
                    .h(sp(24.0))
                    .px_1p5()
                    .gap_2()
                    .flex_shrink_0()
                    .items_center()
                    .rounded(px(4.0))
                    .when(!added, |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(theme.accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = this.entries[ix].name.clone();
                                if (this.on_choose)(&name, window, cx) {
                                    this.entries[ix].selected = true;
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        Icon::new(if added {
                            IconName::Check
                        } else {
                            IconName::Plus
                        })
                        .xsmall()
                        .text_color(if added {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        }),
                    )
                    .child(field_row_text(
                        &entry.label,
                        &format!("{} · {}", entry.name, entry.source),
                        240.0,
                        cx,
                    ))
            })
            .collect();
        v_flex()
            .gap_2()
            .child(
                Input::new(&self.search)
                    .small()
                    .prefix(Icon::new(IconName::Search).xsmall())
                    .cleanable(true),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{count} fields · click to add")),
            )
            .child(
                v_flex()
                    .id("field-search-results")
                    .max_h(sp(380.0))
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}
