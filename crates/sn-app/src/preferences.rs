//! Preferences dialog (`⌘,`): appearance, list behaviour, accounts and local data.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_core::InstanceProfile;

use crate::prefs::{
    AppPrefs, Appearance, Density, LIST_PAGE_SIZES, MAX_UI_SCALE, MIN_UI_SCALE, UI_SCALE_STEP,
};
use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::{sp, spx};

pub struct PreferencesView {
    services: AppServices,
    active_profile: Option<InstanceProfile>,
}

impl PreferencesView {
    pub fn new(services: AppServices, active_profile: Option<InstanceProfile>) -> Self {
        Self {
            services,
            active_profile,
        }
    }
}

fn scale_key(scale: f32) -> SharedString {
    format!("{:.0}", scale * 100.0).into()
}

fn appearance_page() -> SettingPage {
    let scale_steps = ((MAX_UI_SCALE - MIN_UI_SCALE) / UI_SCALE_STEP).round() as usize;
    let scale_options = (0..=scale_steps)
        .map(|step| {
            let scale = MIN_UI_SCALE + step as f32 * UI_SCALE_STEP;
            (scale_key(scale), format!("{:.0}%", scale * 100.0).into())
        })
        .collect();

    SettingPage::new("Appearance")
        .icon(IconName::Palette)
        .resettable(true)
        .group(
            SettingGroup::new().title("Theme").item(
                SettingItem::new(
                    "Appearance",
                    SettingField::dropdown(
                        Appearance::ALL
                            .into_iter()
                            .map(|option| (option.key().into(), option.label().into()))
                            .collect(),
                        |cx| AppPrefs::get(cx).appearance.key().into(),
                        |value: SharedString, cx| {
                            AppPrefs::update(cx, |prefs| {
                                prefs.appearance = Appearance::from_key(&value);
                            });
                        },
                    )
                    .default_value(Appearance::default().key()),
                )
                .description("Match system follows the macOS light or dark setting."),
            ),
        )
        .group(
            SettingGroup::new()
                .title("Size")
                .item(
                    SettingItem::new(
                        "UI scale",
                        SettingField::dropdown(
                            scale_options,
                            |cx| scale_key(AppPrefs::get(cx).ui_scale),
                            |value: SharedString, cx| {
                                if let Ok(percent) = value.parse::<f32>() {
                                    AppPrefs::update(cx, |prefs| prefs.ui_scale = percent / 100.0);
                                }
                            },
                        )
                        .default_value(scale_key(1.0)),
                    )
                    .description("Scales text and controls. Also ⌘+ and ⌘−; ⌘0 resets."),
                )
                .item(
                    SettingItem::new(
                        "Density",
                        SettingField::dropdown(
                            Density::ALL
                                .into_iter()
                                .map(|option| (option.key().into(), option.label().into()))
                                .collect(),
                            |cx| AppPrefs::get(cx).density.key().into(),
                            |value: SharedString, cx| {
                                AppPrefs::update(cx, |prefs| {
                                    prefs.density = Density::from_key(&value);
                                });
                            },
                        )
                        .default_value(Density::default().key()),
                    )
                    .description("Compact fits more list rows and form fields on screen."),
                ),
        )
}

fn lists_page() -> SettingPage {
    SettingPage::new("Lists")
        .icon(IconName::Table)
        .resettable(true)
        .group(
            SettingGroup::new().title("Paging").item(
                SettingItem::new(
                    "Rows per page",
                    SettingField::dropdown(
                        LIST_PAGE_SIZES
                            .into_iter()
                            .map(|size| (size.to_string().into(), size.to_string().into()))
                            .collect(),
                        |cx| AppPrefs::get(cx).list_page_size.to_string().into(),
                        |value: SharedString, cx| {
                            if let Ok(size) = value.parse::<usize>() {
                                AppPrefs::update(cx, |prefs| prefs.list_page_size = size);
                            }
                        },
                    )
                    .default_value(AppPrefs::default().list_page_size.to_string()),
                )
                .description("Open lists reload when this changes."),
            ),
        )
        .group(
            SettingGroup::new().title("Diagnostics").item(
                SettingItem::new(
                    "Show timing readouts",
                    SettingField::switch(
                        |cx| AppPrefs::get(cx).show_perf_telemetry,
                        |value, cx| {
                            AppPrefs::update(cx, |prefs| prefs.show_perf_telemetry = value);
                        },
                    )
                    .default_value(AppPrefs::default().show_perf_telemetry),
                )
                .description("Cache and network timings in the list footer and form header."),
            ),
        )
}

fn accounts_page(services: AppServices, active: Option<InstanceProfile>) -> SettingPage {
    let summary = active
        .as_ref()
        .map(|profile| format!("Signed in to {}", profile.display_label()))
        .unwrap_or_else(|| "Not connected".to_string());
    SettingPage::new("Accounts").icon(IconName::User).group(
        SettingGroup::new()
            .title("ServiceNow connections")
            .item(SettingItem::render(move |_, _, cx| {
                let services = services.clone();
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .child(div().text_sm().child(summary.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Add instances, add users, or sign in again. \
                                         Also on the instance pill in the title bar.",
                                    ),
                            ),
                    )
                    .child(
                        Button::new("prefs-manage-connections")
                            .secondary()
                            .small()
                            .label("Manage connections…")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                services.send_cmd(WorkspaceCommand::ShowInstanceModal);
                            }),
                    )
            })),
    )
}

fn data_page(services: AppServices, active: Option<InstanceProfile>) -> SettingPage {
    SettingPage::new("Data").icon(IconName::Database).group(
        SettingGroup::new()
            .title("Local cache")
            .item(SettingItem::render(move |_, _, cx| {
                let stats = services
                    .cache
                    .cache_stats(active.as_ref().map(|profile| &profile.id))
                    .unwrap_or_default();
                let megabytes = stats.db_bytes as f64 / (1024.0 * 1024.0);
                let services = services.clone();
                let instance_id = active.as_ref().map(|profile| profile.id.clone());
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .child(div().text_sm().child(format!(
                                "{} records · {} table schemas · {megabytes:.1} MB on disk",
                                stats.record_count, stats.schema_count
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Clearing removes cached records and schemas for the \
                                         current account. Custom layouts and columns are kept.",
                                    ),
                            ),
                    )
                    .when_some(instance_id, |row, instance_id| {
                        row.child(
                            Button::new("prefs-clear-cache")
                                .danger()
                                .small()
                                .label("Clear cache")
                                .on_click(move |_, _, cx| {
                                    match services.cache.clear_instance_cache(&instance_id) {
                                        Ok(()) => {
                                            services.send_cmd(WorkspaceCommand::RefreshSidebar);
                                            services.send_cmd(WorkspaceCommand::ToastInfo(
                                                "Cleared cached records and schemas.".into(),
                                            ));
                                        }
                                        Err(err) => {
                                            services.send_cmd(WorkspaceCommand::ToastError(
                                                format!("Cannot clear cache: {err}"),
                                            ))
                                        }
                                    }
                                    cx.refresh_windows();
                                }),
                        )
                    })
            })),
    )
}

impl Render for PreferencesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().h(sp(460.0)).child(
            Settings::new("snrust-preferences")
                .sidebar_width(spx(180.0, cx))
                .pages([
                    appearance_page(),
                    lists_page(),
                    accounts_page(self.services.clone(), self.active_profile.clone()),
                    data_page(self.services.clone(), self.active_profile.clone()),
                ]),
        )
    }
}
