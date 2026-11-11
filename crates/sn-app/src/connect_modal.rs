//! Multi-instance OAuth 2.0 PKCE connection manager & one-click Demo Sandbox onboarding.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sn_api::{CredentialStore, OAuthPkceFlow};
use sn_core::instance::DEFAULT_OAUTH_CALLBACK_PORT;
use sn_core::{InstanceId, InstanceProfile};
use tokio::sync::watch;

use crate::state::{AppServices, WorkspaceCommand};
use crate::theme::parse_hex_color;

const ACCENT_PRESETS: &[(&str, &str)] = &[
    ("Dev", "#34D399"),
    ("Test", "#38BDF8"),
    ("Stage", "#FBBF24"),
    ("Prod", "#F87171"),
    ("Indigo", "#818CF8"),
];

pub struct ConnectModalView {
    services: AppServices,
    instances: Vec<InstanceProfile>,
    active_instance_id: Option<InstanceId>,
    name_input: Entity<InputState>,
    url_input: Entity<InputState>,
    client_id_input: Entity<InputState>,
    client_secret_input: Entity<InputState>,
    port_input: Entity<InputState>,
    selected_accent: String,
    editing_id: Option<InstanceId>,
    pending_auth_url: Option<String>,
    cancel_tx: Option<watch::Sender<bool>>,
    status_error: Option<String>,
    is_busy: bool,
}

impl ConnectModalView {
    pub fn new(
        services: AppServices,
        active_instance_id: Option<InstanceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let instances = services.cache.list_instances().unwrap_or_default();
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Acme Production or Dev PDI"));
        let url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("dev12345 or https://acme.service-now.com")
        });
        let client_id_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("OAuth Application Registry Client ID")
        });
        let client_secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Optional (leave blank for Public PKCE client)")
        });
        let port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(DEFAULT_OAUTH_CALLBACK_PORT.to_string())
                .placeholder("8910")
        });

        Self {
            services,
            instances,
            active_instance_id,
            name_input,
            url_input,
            client_id_input,
            client_secret_input,
            port_input,
            selected_accent: "#38BDF8".to_string(),
            editing_id: None,
            pending_auth_url: None,
            cancel_tx: None,
            status_error: None,
            is_busy: false,
        }
    }

    fn current_redirect_uri(&self, cx: &App) -> String {
        let port = self
            .port_input
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT);
        format!("http://127.0.0.1:{port}/oauth/callback")
    }

    fn populate_form_from(
        &mut self,
        profile: &InstanceProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing_id = Some(profile.id.clone());
        self.selected_accent = profile.accent_hex.clone();
        self.name_input
            .update(cx, |s, cx| s.set_value(profile.name.clone(), window, cx));
        self.url_input.update(cx, |s, cx| {
            s.set_value(profile.base_url.clone(), window, cx)
        });
        self.client_id_input.update(cx, |s, cx| {
            s.set_value(profile.client_id.clone(), window, cx)
        });
        self.port_input.update(cx, |s, cx| {
            s.set_value(profile.callback_port.to_string(), window, cx)
        });
        self.client_secret_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.status_error = None;
        cx.notify();
    }

    fn reset_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.selected_accent = "#38BDF8".to_string();
        self.name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.url_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.client_id_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.client_secret_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.port_input.update(cx, |s, cx| {
            s.set_value(DEFAULT_OAUTH_CALLBACK_PORT.to_string(), window, cx)
        });
        self.status_error = None;
        cx.notify();
    }

    fn start_oauth_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy {
            return;
        }
        let name = self.name_input.read(cx).value().to_string();
        let raw_url = self.url_input.read(cx).value().to_string();
        let client_id = self.client_id_input.read(cx).value().trim().to_string();
        let client_secret_raw = self.client_secret_input.read(cx).value().trim().to_string();
        let callback_port = self
            .port_input
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT);

        if client_id.is_empty() {
            self.status_error = Some("OAuth Client ID is required.".to_string());
            cx.notify();
            return;
        }

        let mut profile =
            match InstanceProfile::new(name, &raw_url, &client_id, !client_secret_raw.is_empty()) {
                Ok(p) => p,
                Err(err) => {
                    self.status_error = Some(err.to_string());
                    cx.notify();
                    return;
                }
            };

        if let Some(existing_id) = &self.editing_id {
            profile.id = existing_id.clone();
        }
        profile.callback_port = callback_port;
        profile.accent_hex = self.selected_accent.clone();

        let client_secret_opt = (!client_secret_raw.is_empty()).then_some(client_secret_raw);
        if let Some(sec) = &client_secret_opt {
            let _ = self
                .services
                .cred_store
                .save_client_secret(&profile.id, sec);
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancel_tx = Some(cancel_tx);
        self.is_busy = true;
        self.status_error = None;
        cx.notify();

        let services = self.services.clone();
        let tokio_handle = services.tokio_handle.clone();
        let cred_store: Arc<dyn CredentialStore> = services.cred_store.clone();
        let (phase_tx, phase_rx) = async_channel::bounded::<Result<String, String>>(2);
        let (done_tx, done_rx) = async_channel::bounded::<Result<InstanceProfile, String>>(1);

        let prof_bg = profile.clone();
        tokio_handle.spawn(async move {
            match OAuthPkceFlow::start_pkce_listener(
                &prof_bg,
                client_secret_opt,
                cred_store,
                true,
                Some(cancel_rx),
            )
            .await
            {
                Ok(pending) => {
                    let _ = phase_tx.send(Ok(pending.authorize_url.clone())).await;
                    match pending.wait_for_callback(Duration::from_secs(240)).await {
                        Ok(_creds) => {
                            let mut connected = prof_bg;
                            connected.last_connected_at = Some(Utc::now());
                            let _ = done_tx.send(Ok(connected)).await;
                        }
                        Err(e) => {
                            let _ = done_tx.send(Err(e.to_string())).await;
                        }
                    }
                }
                Err(e) => {
                    let _ = phase_tx.send(Err(e.to_string())).await;
                }
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(phase) = phase_rx.recv().await {
                match phase {
                    Ok(auth_url) => {
                        let _ = this.update(cx, |view, cx| {
                            view.pending_auth_url = Some(auth_url);
                            cx.notify();
                        });
                    }
                    Err(err_msg) => {
                        let _ = this.update(cx, |view, cx| {
                            view.is_busy = false;
                            view.pending_auth_url = None;
                            view.cancel_tx = None;
                            view.status_error = Some(err_msg);
                            cx.notify();
                        });
                        return;
                    }
                }
            }

            if let Ok(outcome) = done_rx.recv().await {
                let _ = this.update_in(cx, |view, window, cx| {
                    view.is_busy = false;
                    view.pending_auth_url = None;
                    view.cancel_tx = None;

                    match outcome {
                        Ok(connected_profile) => {
                            let _ = view.services.cache.upsert_instance(&connected_profile);
                            let _ = view
                                .services
                                .cache
                                .set_active_instance_id(Some(&connected_profile.id));
                            view.services.evict_client(&connected_profile.id);
                            view.instances =
                                view.services.cache.list_instances().unwrap_or_default();
                            view.active_instance_id = Some(connected_profile.id.clone());
                            window.close_dialog(cx);
                            view.services.send_cmd(WorkspaceCommand::SwitchInstance {
                                profile: connected_profile,
                            });
                        }
                        Err(err_msg) => {
                            view.status_error = Some(err_msg);
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }

    fn cancel_oauth(&mut self, cx: &mut Context<Self>) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(true);
        }
        self.is_busy = false;
        self.pending_auth_url = None;
        cx.notify();
    }

    fn launch_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy {
            return;
        }
        self.is_busy = true;
        self.status_error = None;
        cx.notify();

        let services = self.services.clone();
        let (tx, rx) = async_channel::bounded(1);
        services.tokio_handle.spawn({
            let services = services.clone();
            async move {
                let _ = tx.send(services.launch_demo_sandbox().await).await;
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(res) = rx.recv().await {
                let _ = this.update_in(cx, |view, window, cx| {
                    view.is_busy = false;
                    match res {
                        Ok(profile) => {
                            view.instances =
                                view.services.cache.list_instances().unwrap_or_default();
                            view.active_instance_id = Some(profile.id.clone());
                            window.close_dialog(cx);
                            view.services
                                .send_cmd(WorkspaceCommand::SwitchInstance { profile });
                        }
                        Err(err) => {
                            view.status_error = Some(format!("Demo Sandbox failed: {err}"));
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }
}

impl Render for ConnectModalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let redirect_uri = self.current_redirect_uri(cx);
        let redirect_for_copy = redirect_uri.clone();
        let theme = cx.theme().clone();

        h_flex()
            .w_full()
            .min_h(px(460.0))
            .gap_5()
            .items_start()
            // Left column: Saved Instances + Instant Demo Sandbox
            .child(
                v_flex()
                    .w(px(270.0))
                    .flex_shrink_0()
                    .gap_3()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .bg(theme.sidebar)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .gap_2()
                            .p_2p5()
                            .rounded(theme.radius)
                            .bg(theme.accent)
                            .border_1()
                            .border_color(theme.primary.opacity(0.4))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(IconName::Zap).small().text_color(theme.primary))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_semibold()
                                            .text_color(theme.foreground)
                                            .child("Instant Live Demo Sandbox"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Explore 180+ incidents, SWR cache, auto-grid customization & live server updates locally."),
                            )
                            .child(
                                Button::new("launch-demo-btn")
                                    .primary()
                                    .small()
                                    .w_full()
                                    .label("Launch Demo Sandbox")
                                    .disabled(self.is_busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.launch_demo(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .pt_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(theme.muted_foreground)
                                    .child("SAVED INSTANCES"),
                            )
                            .child(
                                Button::new("new-instance-profile-btn")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .label("New")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_form(window, cx);
                                    })),
                            ),
                    )
                    .child(if self.instances.is_empty() {
                        div()
                            .py_6()
                            .text_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("No instances saved yet.")
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_1p5()
                            .children(self.instances.iter().cloned().enumerate().map(
                                |(ix, inst)| {
                                    let is_active =
                                        self.active_instance_id.as_ref() == Some(&inst.id);
                                    let dot_color =
                                        parse_hex_color(&inst.accent_hex).unwrap_or(theme.primary);
                                    let inst_switch = inst.clone();
                                    let inst_edit = inst.clone();
                                    let inst_del_id = inst.id.clone();

                                    v_flex()
                                        .id(("saved-inst", ix))
                                        .p_2()
                                        .gap_1()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(if is_active {
                                            theme.primary
                                        } else {
                                            theme.border
                                        })
                                        .bg(if is_active {
                                            theme.secondary
                                        } else {
                                            theme.background
                                        })
                                        .child(
                                            h_flex()
                                                .justify_between()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .items_center()
                                                        .overflow_hidden()
                                                        .child(
                                                            div()
                                                                .size(px(8.0))
                                                                .rounded_full()
                                                                .bg(dot_color),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .font_semibold()
                                                                .truncate()
                                                                .child(inst.name.clone()),
                                                        ),
                                                )
                                                .when(is_active, |el| {
                                                    el.child(
                                                        div()
                                                            .px_1p5()
                                                            .py_0p5()
                                                            .rounded(px(4.0))
                                                            .bg(theme.primary.opacity(0.18))
                                                            .text_size(px(10.0))
                                                            .text_color(theme.primary)
                                                            .child("Active"),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .text_color(theme.muted_foreground)
                                                .truncate()
                                                .child(inst.base_url.clone()),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .pt_1()
                                                .child(
                                                    Button::new(("switch-inst", ix))
                                                        .secondary()
                                                        .xsmall()
                                                        .label(if is_active {
                                                            "Reconnect"
                                                        } else {
                                                            "Switch"
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                let _ = this
                                                                    .services
                                                                    .cache
                                                                    .set_active_instance_id(Some(
                                                                        &inst_switch.id,
                                                                    ));
                                                                window.close_dialog(cx);
                                                                this.services.send_cmd(
                                                                    WorkspaceCommand::SwitchInstance {
                                                                        profile: inst_switch.clone(),
                                                                    },
                                                                );
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new(("edit-inst", ix))
                                                        .ghost()
                                                        .xsmall()
                                                        .label("Edit")
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.populate_form_from(
                                                                    &inst_edit, window, cx,
                                                                );
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new(("del-inst", ix))
                                                        .ghost()
                                                        .xsmall()
                                                        .icon(IconName::Trash)
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                let _ = this
                                                                    .services
                                                                    .cache
                                                                    .delete_instance(&inst_del_id);
                                                                let _ = this
                                                                    .services
                                                                    .cred_store
                                                                    .delete_credentials(&inst_del_id);
                                                                this.services
                                                                    .evict_client(&inst_del_id);
                                                                this.instances = this
                                                                    .services
                                                                    .cache
                                                                    .list_instances()
                                                                    .unwrap_or_default();
                                                                this.services.send_cmd(
                                                                    WorkspaceCommand::InstanceDeleted {
                                                                        instance_id: inst_del_id
                                                                            .clone(),
                                                                    },
                                                                );
                                                                cx.notify();
                                                            },
                                                        )),
                                                ),
                                        )
                                },
                            ))
                            .into_any_element()
                    }),
            )
            // Right column: OAuth 2.0 PKCE Configuration Form
            .child(
                v_flex()
                    .flex_1()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(if self.editing_id.is_some() {
                                        "Update ServiceNow OAuth Profile"
                                    } else {
                                        "Connect via ServiceNow OAuth 2.0 (Auth Code + PKCE)"
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Authenticates in your system browser (SSO/MFA supported) and stores tokens in macOS Keychain."),
                            ),
                    )
                    .child(field_block(
                        "Display Name",
                        Input::new(&self.name_input).small(),
                        &theme,
                    ))
                    .child(field_block(
                        "Instance Name or URL",
                        Input::new(&self.url_input).small(),
                        &theme,
                    ))
                    .child(
                        h_flex()
                            .gap_3()
                            .w_full()
                            .child(
                                div().flex_1().child(field_block(
                                    "OAuth Client ID",
                                    Input::new(&self.client_id_input).small(),
                                    &theme,
                                )),
                            )
                            .child(
                                div().w(px(110.0)).child(field_block(
                                    "Callback Port",
                                    Input::new(&self.port_input).small(),
                                    &theme,
                                )),
                            ),
                    )
                    .child(field_block(
                        "Client Secret (Optional for Public PKCE Endpoints)",
                        Input::new(&self.client_secret_input)
                            .mask_toggle()
                            .small(),
                        &theme,
                    ))
                    // Environment Accent Selector
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(theme.muted_foreground)
                                    .child("Environment Badge Color"),
                            )
                            .child(
                                h_flex().gap_2().children(
                                    ACCENT_PRESETS.iter().enumerate().map(|(ix, (lbl, hex))| {
                                        let is_sel = self.selected_accent == *hex;
                                        let swatch =
                                            parse_hex_color(hex).unwrap_or(theme.primary);
                                        let hex_owned = (*hex).to_string();
                                        h_flex()
                                            .id(("accent-opt", ix))
                                            .px_2()
                                            .py_1()
                                            .gap_1p5()
                                            .items_center()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .cursor_pointer()
                                            .border_color(if is_sel {
                                                swatch
                                            } else {
                                                theme.border
                                            })
                                            .bg(if is_sel {
                                                swatch.opacity(0.16)
                                            } else {
                                                theme.secondary
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.selected_accent = hex_owned.clone();
                                                cx.notify();
                                            }))
                                            .child(
                                                div().size(px(8.0)).rounded_full().bg(swatch),
                                            )
                                            .child(div().text_xs().child(*lbl))
                                    }),
                                ),
                            ),
                    )
                    // Redirect URL helper card
                    .child(
                        v_flex()
                            .gap_1p5()
                            .p_2p5()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_medium()
                                            .child("Redirect URL for ServiceNow Application Registry"),
                                    )
                                    .child(
                                        Button::new("copy-redirect-uri")
                                            .secondary()
                                            .xsmall()
                                            .icon(IconName::Copy)
                                            .label("Copy URL")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    redirect_for_copy.clone(),
                                                ));
                                                this.services.send_cmd(WorkspaceCommand::ToastInfo(
                                                    format!(
                                                        "Copied Redirect URL: {}",
                                                        redirect_for_copy
                                                    ),
                                                ));
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.primary)
                                    .child(redirect_uri),
                            ),
                    )
                    .when_some(self.status_error.clone(), |el, err| {
                        el.child(
                            div()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.danger.opacity(0.14))
                                .border_1()
                                .border_color(theme.danger.opacity(0.4))
                                .text_xs()
                                .text_color(theme.danger)
                                .child(err),
                        )
                    })
                    .when_some(self.pending_auth_url.clone(), |el, auth_url| {
                        el.child(
                            h_flex()
                                .p_2p5()
                                .justify_between()
                                .items_center()
                                .rounded(theme.radius)
                                .bg(theme.primary.opacity(0.12))
                                .border_1()
                                .border_color(theme.primary.opacity(0.4))
                                .child(
                                    div()
                                        .text_xs()
                                        .child("Waiting for ServiceNow OAuth sign-in in your browser…"),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .child(
                                            Button::new("reopen-browser-btn")
                                                .secondary()
                                                .xsmall()
                                                .icon(IconName::ExternalLink)
                                                .label("Open Browser")
                                                .on_click(move |_, _, _| {
                                                    let _ = open::that(&auth_url);
                                                }),
                                        )
                                        .child(
                                            Button::new("cancel-oauth-btn")
                                                .danger()
                                                .xsmall()
                                                .label("Cancel")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_oauth(cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .pt_1()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("oauth-connect-submit")
                                    .primary()
                                    .icon(IconName::KeyRound)
                                    .label(if self.is_busy {
                                        "Authenticating…"
                                    } else {
                                        "Sign in with OAuth 2.0 + PKCE"
                                    })
                                    .disabled(self.is_busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_oauth_connect(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

fn field_block(
    label: &'static str,
    control: impl IntoElement,
    theme: &gpui_kit::component::theme::Theme,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_xs()
                .font_medium()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(control)
}
