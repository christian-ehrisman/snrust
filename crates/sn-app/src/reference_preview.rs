//! Clickable reference values: click opens the referenced record in a tab, ⌘-click opens it
//! in the split pane, and hovering shows a preview card of its key fields.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parking_lot::Mutex;
use sn_api::SnClient;
use sn_core::{SnRecord, TableSchema};

use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::sp;

const PREVIEW_OPEN_DELAY: Duration = Duration::from_millis(400);
/// How long a fetched preview is trusted before hovering fetches it again.
const PREVIEW_FRESH_FOR: Duration = Duration::from_secs(60);
const MAX_PREVIEW_ROWS: usize = 6;

#[derive(Clone)]
enum PreviewLoad {
    Loading,
    Loaded(Instant),
    Failed(String),
}

/// Tracks one fetch per referenced record so that re-rendering an open card, or hovering the
/// same value in many rows, does not repeat the request.
#[derive(Clone, Default)]
pub struct PreviewLoads(Arc<Mutex<HashMap<String, PreviewLoad>>>);

impl PreviewLoads {
    fn get(&self, key: &str) -> Option<PreviewLoad> {
        self.0.lock().get(key).cloned()
    }

    fn set(&self, key: &str, state: PreviewLoad) {
        self.0.lock().insert(key.to_string(), state);
    }
}

/// A reference value holds one record only when it is a single id. `glide_list` fields carry
/// several comma-separated ids and are not linkable as one record.
pub fn is_single_reference(raw: &str) -> bool {
    !raw.trim().is_empty() && !raw.contains(',')
}

/// Label and value pairs shown in the card: the target table's list columns when its schema
/// is known, otherwise whatever populated fields the record has.
pub fn preview_rows(record: &SnRecord, schema: Option<&TableSchema>) -> Vec<(String, String)> {
    let title = record.primary_identifier();
    let summary = record.summary_text();
    let keys: Vec<&String> = match schema {
        Some(schema) if !schema.default_list_columns.is_empty() => {
            schema.default_list_columns.iter().collect()
        }
        _ => record.fields.keys().collect(),
    };
    keys.into_iter()
        .filter(|key| !key.starts_with("sys_"))
        .filter_map(|key| {
            let value = record.get_display(key);
            if value.is_empty() || value == title || Some(value) == summary.as_deref() {
                return None;
            }
            let label = schema
                .and_then(|schema| schema.fields.get(key))
                .map(|field| field.column_label.clone())
                .unwrap_or_else(|| crate::list_view::prettify_column_key(key));
            Some((label, value.to_string()))
        })
        .take(MAX_PREVIEW_ROWS)
        .collect()
}

/// Builds the link. `emphasized` draws it in the link colour at rest, for places such as
/// form fields where plain text would not read as clickable.
#[allow(clippy::too_many_arguments)]
pub fn reference_link(
    id: impl Into<ElementId>,
    services: &AppServices,
    client: &SnClient,
    target_table: &str,
    sys_id: &str,
    display: &str,
    emphasized: bool,
    cx: &App,
) -> HoverCard {
    let theme = cx.theme();
    let primary = theme.primary;
    let rest_color = if emphasized {
        theme.primary
    } else {
        theme.foreground
    };

    let click_services = services.clone();
    let click_table = target_table.to_string();
    let click_id = sys_id.to_string();
    let selector = format!("ref-link:{target_table}:{sys_id}");
    let trigger = div()
        .id("ref-link")
        .debug_selector(move || selector)
        .max_w_full()
        .cursor_pointer()
        .text_xs()
        .text_color(rest_color)
        .truncate()
        .hover(move |style| style.text_color(primary).underline())
        .on_click(move |event: &ClickEvent, _, cx| {
            cx.stop_propagation();
            click_services.send_cmd(WorkspaceCommand::OpenRecord {
                table: click_table.clone(),
                sys_id: click_id.clone(),
                seed_record: None,
                in_split: event.modifiers().platform,
            });
        })
        .child(display.to_string());

    let services = services.clone();
    let client = client.clone();
    let table = target_table.to_string();
    let sys_id = sys_id.to_string();
    HoverCard::new(id)
        .open_delay(PREVIEW_OPEN_DELAY)
        .trigger_style(StyleRefinement::default().max_w_full().min_w_0())
        .trigger(trigger)
        .content(move |_, _, cx| render_preview(&services, &client, &table, &sys_id, cx))
}

fn render_preview<V: 'static>(
    services: &AppServices,
    client: &SnClient,
    table: &str,
    sys_id: &str,
    cx: &mut Context<V>,
) -> AnyElement {
    let instance_id = client.profile().id.clone();
    let key = format!("{}:{table}:{sys_id}", instance_id.as_str());
    let cached = services
        .cache
        .get_record(&instance_id, table, sys_id)
        .ok()
        .flatten()
        .map(|item| item.data);
    let schema = services
        .cache
        .get_table_schema(&instance_id, table)
        .ok()
        .flatten()
        .map(|item| item.data);

    let load = services.preview_loads.get(&key);
    let needs_fetch = match &load {
        None => true,
        Some(PreviewLoad::Loaded(at)) => at.elapsed() > PREVIEW_FRESH_FOR,
        Some(PreviewLoad::Loading | PreviewLoad::Failed(_)) => false,
    };
    if needs_fetch {
        services.preview_loads.set(&key, PreviewLoad::Loading);
        let (done_tx, done_rx) = async_channel::bounded::<()>(1);
        let bg_services = services.clone();
        let bg_client = client.clone();
        let bg_table = table.to_string();
        let bg_id = sys_id.to_string();
        let bg_key = key.clone();
        services.tokio_handle.spawn(async move {
            // The schema only supplies labels and column choice; the card works without it.
            let (record, _) = tokio::join!(
                bg_client.get_record(&bg_table, &bg_id),
                bg_services.ensure_table_schema(&bg_client, &bg_table, false)
            );
            let state = match record {
                Ok(record) => {
                    let _ = bg_services
                        .cache
                        .upsert_record(&bg_client.profile().id, &record);
                    PreviewLoad::Loaded(Instant::now())
                }
                Err(err) => PreviewLoad::Failed(err.to_string()),
            };
            bg_services.preview_loads.set(&bg_key, state);
            let _ = done_tx.send(()).await;
        });
        cx.spawn(async move |card, cx| {
            let _ = done_rx.recv().await;
            let _ = card.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    let theme = cx.theme().clone();
    let table_label = schema
        .as_ref()
        .map(|schema| schema.label.clone())
        .unwrap_or_else(|| table.to_string());
    let status = match (&cached, &load) {
        (_, Some(PreviewLoad::Failed(err))) => Some(format!("Preview unavailable: {err}")),
        (None, _) => Some("Loading…".to_string()),
        _ => None,
    };

    v_flex()
        .w(sp(300.0))
        .gap_1p5()
        .child(
            div()
                .text_size(sp(10.5))
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(table_label.to_uppercase()),
        )
        .when_some(cached.as_ref(), |card, record| {
            card.child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(record.primary_identifier()),
            )
            .when_some(record.summary_text(), |card, summary| {
                card.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(summary),
                )
            })
            .children(
                preview_rows(record, schema.as_ref())
                    .into_iter()
                    .map(|(label, value)| {
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(
                                div()
                                    .w(sp(96.0))
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(label),
                            )
                            .child(div().flex_1().min_w_0().text_xs().child(value))
                    }),
            )
        })
        .when_some(status, |card, status| {
            card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
        })
        .child(
            div()
                .pt_1()
                .border_t_1()
                .border_color(theme.border)
                .text_size(sp(10.5))
                .text_color(theme.muted_foreground)
                .child("Click to open · ⌘-click to open beside"),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: with test support enabled the GPUI glob re-exports its own `test`
    // attribute, which would shadow the standard one.
    use super::{is_single_reference, preview_rows};
    use indexmap::IndexMap;
    use sn_core::{FieldValue, SnRecord};

    fn user() -> SnRecord {
        let mut fields = IndexMap::new();
        for (key, value) in [
            ("sys_id", "usr_1"),
            ("name", "Beth Anglin"),
            ("email", "beth@example.com"),
            ("department", "Service Desk"),
            ("title", ""),
            ("sys_updated_on", "2026-11-11 08:00:00"),
        ] {
            fields.insert(key.to_string(), FieldValue::scalar(value));
        }
        SnRecord::from_field_map("sys_user", fields, false)
    }

    #[test]
    fn preview_omits_title_summary_empty_and_system_fields() {
        // "Beth Anglin" is the title and the email is the summary line, so neither repeats.
        assert_eq!(
            preview_rows(&user(), None),
            vec![("Department".to_string(), "Service Desk".to_string())]
        );
    }

    #[test]
    fn only_single_ids_are_linkable() {
        assert!(is_single_reference("46d44a23a9fe19810012d100cca80666"));
        assert!(is_single_reference("usr_marcus"));
        assert!(!is_single_reference(""));
        assert!(!is_single_reference("a1,b2"));
    }
}
