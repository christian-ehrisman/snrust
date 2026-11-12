//! Navigator sidebar: favorites and applications as collapsible trees, the table catalog,
//! and recent records.
//!
//! A child module of `workspace` so it can render straight from `WorkspaceShell`'s state.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::theme::{ActiveTheme as _, Theme};
use gpui_kit::component::{Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use sn_api::SnClient;
use sn_cache::FavoriteItem;
use sn_core::NavNode;

use super::WorkspaceShell;
use crate::favorites::{self, FavoriteRef};
use crate::state::AppServices;
use crate::theme::{sp, spx};

/// Which tree nodes are open, remembered per account.
///
/// Applications start collapsed and favorites groups start open, so each is stored as the
/// set of exceptions to its default. Keys are ServiceNow `sys_id`s, which survive renames.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavExpansion {
    #[serde(default)]
    pub expanded: HashSet<String>,
    #[serde(default)]
    pub collapsed_favorites: HashSet<String>,
}

pub const NAV_EXPANSION_KEY: &str = "navigation.expansion";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tree {
    Applications,
    Favorites,
}

/// A tree node flattened for rendering, with its depth and its parent group.
struct FlatNode {
    node: NavNode,
    depth: usize,
    parent: Option<String>,
    open: bool,
}

fn flatten(
    nodes: &[NavNode],
    tree: Tree,
    expansion: &NavExpansion,
    force_open: bool,
    depth: usize,
    parent: Option<&str>,
    out: &mut Vec<FlatNode>,
) {
    for node in nodes {
        let open = node.is_group()
            && (force_open
                || match tree {
                    Tree::Applications => expansion.expanded.contains(&node.id),
                    Tree::Favorites => !expansion.collapsed_favorites.contains(&node.id),
                });
        out.push(FlatNode {
            node: NavNode {
                children: Vec::new(),
                ..node.clone()
            },
            depth,
            parent: parent.map(str::to_string),
            open,
        });
        if open {
            flatten(
                &node.children,
                tree,
                expansion,
                force_open,
                depth + 1,
                Some(&node.id),
                out,
            );
        }
    }
}

#[derive(Clone)]
struct DraggedFavorite {
    id: String,
    title: String,
    is_group: bool,
}

struct DraggedFavoriteGhost {
    title: String,
}

impl Render for DraggedFavoriteGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.primary)
            .text_xs()
            .child(self.title.clone())
    }
}

type SubmitName = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Small modal asking for one line of text (rename, new group).
struct NamePrompt {
    input: Entity<InputState>,
    confirm_label: SharedString,
    on_submit: SubmitName,
}

impl NamePrompt {
    /// Runs the callback and reports whether the name was accepted. A blank name is not,
    /// which keeps the dialog open.
    fn try_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return false;
        }
        (self.on_submit)(value, window, cx);
        true
    }
}

impl Render for NamePrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().gap_3().child(Input::new(&self.input)).child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("name-prompt-cancel")
                        .ghost()
                        .small()
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("name-prompt-confirm")
                        .primary()
                        .small()
                        .label(self.confirm_label.clone())
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.try_submit(window, cx) {
                                window.close_dialog(cx);
                            }
                        })),
                ),
        )
    }
}

pub fn prompt_for_name(
    heading: impl Into<SharedString>,
    initial: &str,
    confirm_label: impl Into<SharedString>,
    on_submit: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let heading: SharedString = heading.into();
    let confirm_label: SharedString = confirm_label.into();
    let initial = initial.to_string();
    let prompt = cx.new(|cx| NamePrompt {
        input: cx.new(|cx| InputState::new(window, cx).default_value(initial)),
        confirm_label,
        on_submit: Rc::new(on_submit),
    });
    let first_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            prompt.read(cx).input.focus_handle(cx).focus(window, cx);
        }
        dialog
            .w(spx(380.0, cx))
            .title(heading.clone())
            .close_button(true)
            .overlay_closable(true)
            // ↵ arrives as the dialog's own confirm action, which closes it only when the
            // name was accepted.
            .on_ok({
                let prompt = prompt.clone();
                move |_, window, cx| prompt.update(cx, |prompt, cx| prompt.try_submit(window, cx))
            })
            .child(prompt.clone())
    });
}

fn section_header(label: &'static str, theme: &Theme) -> Div {
    h_flex()
        .h(sp(22.0))
        .px_2()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .text_size(sp(10.0))
        .font_semibold()
        .text_color(theme.muted_foreground)
        .child(label)
}

/// One navigator row: fixed height, indented by depth.
fn tree_row(id: impl Into<ElementId>, depth: usize, theme: &Theme) -> Stateful<Div> {
    let hover = theme.accent;
    h_flex()
        .id(id)
        .h(sp(24.0))
        .w_full()
        .flex_shrink_0()
        .pl(sp(6.0 + depth as f32 * 12.0))
        .pr_1p5()
        .gap_1p5()
        .items_center()
        .rounded(theme.radius)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
}

fn chevron(open: bool, theme: &Theme) -> Icon {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .xsmall()
    .text_color(theme.muted_foreground)
}

impl WorkspaceShell {
    fn toggle_nav_node(&mut self, tree: Tree, id: &str, cx: &mut Context<Self>) {
        let set = match tree {
            Tree::Applications => &mut self.nav_expansion.expanded,
            Tree::Favorites => &mut self.nav_expansion.collapsed_favorites,
        };
        if !set.remove(id) {
            set.insert(id.to_string());
        }
        if let Some(profile) = &self.active_profile
            && let Err(err) = self.services.cache.save_account_metadata(
                &profile.id,
                NAV_EXPANSION_KEY,
                &self.nav_expansion,
            )
        {
            tracing::warn!(%err, "Cannot remember navigator expansion");
        }
        cx.notify();
    }

    fn render_application_row(
        &self,
        ix: usize,
        flat: &FlatNode,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let node = &flat.node;
        let selector = format!("app:{}", node.id);
        let row = tree_row(("app-node", ix), flat.depth, theme).debug_selector(move || selector);
        match node.target().cloned() {
            None => {
                let id = node.id.clone();
                row.on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_nav_node(Tree::Applications, &id, cx);
                }))
                .child(chevron(flat.open, theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .when(flat.depth == 0, |title| title.font_medium())
                        .when(flat.depth > 0, |title| {
                            title.text_color(theme.muted_foreground)
                        })
                        .truncate()
                        .child(node.title.clone()),
                )
                .into_any_element()
            }
            Some(target) => row
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.open_target(target.clone(), event.modifiers().platform, window, cx);
                }))
                // Aligns module titles with the titles of sibling sections.
                .child(div().flex_shrink_0().w(sp(12.0)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .truncate()
                        .child(node.title.clone()),
                )
                .into_any_element(),
        }
    }

    fn render_favorite_row(
        &self,
        ix: usize,
        flat: &FlatNode,
        groups: &[(String, String)],
        client: &SnClient,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let node = &flat.node;
        let is_group = node.is_group();
        let services = self.services.clone();
        let selector = format!("fav:{}", node.id);

        let drop_services = services.clone();
        let drop_client = client.clone();
        let drop_id = node.id.clone();
        let drop_parent = flat.parent.clone();
        let payload = DraggedFavorite {
            id: node.id.clone(),
            title: node.title.clone(),
            is_group,
        };
        let row = tree_row(("fav-node", ix), flat.depth, theme)
            .debug_selector(move || selector)
            .on_drag(payload, |drag, _, _, cx| {
                cx.new(|_| DraggedFavoriteGhost {
                    title: drag.title.clone(),
                })
            })
            .drag_over::<DraggedFavorite>(|style, _, _, cx| {
                style.border_t_2().border_color(cx.theme().primary)
            })
            .on_drop(move |dragged: &DraggedFavorite, _, _| {
                if dragged.id == drop_id {
                    return;
                }
                // Dropping a favorite on a group files it there; anything else lands just
                // before the row it was dropped on. Groups stay at the top level.
                let (group, before) = if is_group && !dragged.is_group {
                    (Some(drop_id.clone()), None)
                } else if dragged.is_group && drop_parent.is_some() {
                    return;
                } else {
                    (drop_parent.clone(), Some(drop_id.clone()))
                };
                favorites::place(
                    &drop_services,
                    &drop_client,
                    dragged.id.clone(),
                    group,
                    before,
                );
            });

        let row = match node.target().cloned() {
            None => {
                let id = node.id.clone();
                row.on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_nav_node(Tree::Favorites, &id, cx);
                }))
                .child(chevron(flat.open, theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .font_medium()
                        .truncate()
                        .child(node.title.clone()),
                )
            }
            Some(target) => row
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.open_target(target.clone(), event.modifiers().platform, window, cx);
                }))
                .child(Icon::new(IconName::Star).xsmall().text_color(rgb(0xFBBF24)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .truncate()
                        .child(node.title.clone()),
                ),
        };

        let menu_client = client.clone();
        let menu_id = node.id.clone();
        let menu_title = node.title.clone();
        let menu_parent = flat.parent.clone();
        let menu_groups = groups.to_vec();
        row.context_menu(move |menu, window, cx| {
            let rename = {
                let (services, client, id, title) = (
                    services.clone(),
                    menu_client.clone(),
                    menu_id.clone(),
                    menu_title.clone(),
                );
                PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                    let (services, client, id) = (services.clone(), client.clone(), id.clone());
                    prompt_for_name(
                        if is_group {
                            "Rename group"
                        } else {
                            "Rename favorite"
                        },
                        &title,
                        "Rename",
                        move |name, _, _| {
                            favorites::rename(&services, &client, id.clone(), name, is_group);
                        },
                        window,
                        cx,
                    );
                })
            };
            if is_group {
                let (services, client, id) =
                    (services.clone(), menu_client.clone(), menu_id.clone());
                return menu.item(rename).separator().item(
                    PopupMenuItem::new("Remove group (keeps its favorites)").on_click(
                        move |_, _, _| favorites::delete_group(&services, &client, id.clone()),
                    ),
                );
            }

            let move_services = services.clone();
            let move_client = menu_client.clone();
            let move_id = menu_id.clone();
            let move_parent = menu_parent.clone();
            let move_groups = menu_groups.clone();
            let remove = {
                let (services, client, id) =
                    (services.clone(), menu_client.clone(), menu_id.clone());
                PopupMenuItem::new("Remove from favorites").on_click(move |_, _, _| {
                    favorites::remove(&services, &client, FavoriteRef::Synced(id.clone()));
                })
            };
            menu.item(rename)
                .submenu("Move to", window, cx, move |menu, _, _| {
                    let mut menu = menu;
                    if move_parent.is_some() {
                        let (services, client, id) =
                            (move_services.clone(), move_client.clone(), move_id.clone());
                        menu =
                            menu.item(PopupMenuItem::new("Top level").on_click(move |_, _, _| {
                                favorites::place(&services, &client, id.clone(), None, None);
                            }));
                    }
                    for (group_id, group_title) in &move_groups {
                        if move_parent.as_deref() == Some(group_id.as_str()) {
                            continue;
                        }
                        let (services, client, id, group_id) = (
                            move_services.clone(),
                            move_client.clone(),
                            move_id.clone(),
                            group_id.clone(),
                        );
                        menu = menu.item(PopupMenuItem::new(group_title.clone()).on_click(
                            move |_, _, _| {
                                favorites::place(
                                    &services,
                                    &client,
                                    id.clone(),
                                    Some(group_id.clone()),
                                    None,
                                );
                            },
                        ));
                    }
                    let (services, client, id) =
                        (move_services.clone(), move_client.clone(), move_id.clone());
                    menu.separator()
                        .item(
                            PopupMenuItem::new("New group…").on_click(move |_, window, cx| {
                                let (services, client, id) =
                                    (services.clone(), client.clone(), id.clone());
                                prompt_for_name(
                                    "New favorites group",
                                    "",
                                    "Create",
                                    move |name, _, _| {
                                        favorites::create_group(
                                            &services,
                                            &client,
                                            name,
                                            Some(id.clone()),
                                        );
                                    },
                                    window,
                                    cx,
                                );
                            }),
                        )
                })
                .separator()
                .item(remove)
        })
        .into_any_element()
    }

    fn render_local_favorite_row(
        &self,
        ix: usize,
        favorite: &FavoriteItem,
        client: &SnClient,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = favorites::local_target(favorite);
        let services: AppServices = self.services.clone();
        let menu_client = client.clone();
        let id = favorite.id.clone();
        tree_row(("local-fav", ix), 0, theme)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.open_target(target.clone(), event.modifiers().platform, window, cx);
            }))
            .child(
                Icon::new(IconName::Star)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .truncate()
                    .child(favorite.title.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(sp(10.0))
                    .text_color(theme.muted_foreground)
                    .child("this Mac"),
            )
            .context_menu(move |menu, _, _| {
                let (services, client, id) = (services.clone(), menu_client.clone(), id.clone());
                menu.item(
                    PopupMenuItem::new("Remove from favorites").on_click(move |_, _, _| {
                        favorites::remove(&services, &client, FavoriteRef::Local(id.clone()));
                    }),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let filter = self
            .sidebar_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let filtering = !filter.is_empty();
        let narrowed = |nodes: &[NavNode]| -> Vec<NavNode> {
            nodes
                .iter()
                .filter_map(|node| node.filtered(&filter))
                .collect()
        };

        let mut favorite_rows = Vec::new();
        flatten(
            &narrowed(&self.navigation.favorites),
            Tree::Favorites,
            &self.nav_expansion,
            filtering,
            0,
            None,
            &mut favorite_rows,
        );
        let mut application_rows = Vec::new();
        flatten(
            &narrowed(&self.navigation.applications),
            Tree::Applications,
            &self.nav_expansion,
            filtering,
            0,
            None,
            &mut application_rows,
        );
        let groups: Vec<(String, String)> = self
            .navigation
            .favorite_groups()
            .map(|group| (group.id.clone(), group.title.clone()))
            .collect();
        let local_favorites: Vec<&FavoriteItem> = self
            .favorites
            .iter()
            .filter(|favorite| favorite.title.to_lowercase().contains(&filter))
            .collect();
        let client = self.active_client.clone();

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_1p5()
                    .pt_1p5()
                    .justify_between()
                    .items_center()
                    .child(
                        Button::new("sidebar-mode")
                            .ghost()
                            .xsmall()
                            .label(if self.show_table_catalog {
                                "Show applications"
                            } else {
                                "Browse tables"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_table_catalog = !this.show_table_catalog;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("sync-navigation")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Sync applications and favorites from ServiceNow")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.sync_navigation(window, cx);
                            })),
                    ),
            )
            .child(
                div().px_2().py_1p5().child(
                    Input::new(&self.sidebar_filter_input)
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
                v_flex()
                    .id("sidebar-scroll-body")
                    .flex_1()
                    .min_h_0()
                    .px_1p5()
                    .pb_2()
                    .gap_2()
                    .overflow_y_scroll()
                    // Favorites: the ServiceNow tree, then anything saved only locally.
                    .when_some(
                        client.clone().filter(|_| {
                            !favorite_rows.is_empty() || !local_favorites.is_empty()
                        }),
                        |body, client| {
                            let new_group_client = client.clone();
                            let upload_client = client.clone();
                            body.child(
                                v_flex()
                                    .child(section_header("FAVORITES", &theme).child(
                                        Button::new("new-favorites-group")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::FolderPlus)
                                            .tooltip("New favorites group")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                let services = this.services.clone();
                                                let client = new_group_client.clone();
                                                prompt_for_name(
                                                    "New favorites group",
                                                    "",
                                                    "Create",
                                                    move |name, _, _| {
                                                        favorites::create_group(
                                                            &services, &client, name, None,
                                                        );
                                                    },
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    ))
                                    .children(favorite_rows.iter().enumerate().map(
                                        |(ix, flat)| {
                                            self.render_favorite_row(
                                                ix, flat, &groups, &client, &theme, cx,
                                            )
                                        },
                                    ))
                                    .children(local_favorites.iter().enumerate().map(
                                        |(ix, favorite)| {
                                            self.render_local_favorite_row(
                                                ix, favorite, &client, &theme, cx,
                                            )
                                        },
                                    ))
                                    .when(!local_favorites.is_empty() && !filtering, |section| {
                                        section.child(
                                            h_flex().px_1().pt_0p5().child(
                                                Button::new("upload-local-favorites")
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::CloudUpload)
                                                    .label(format!(
                                                        "Upload {} to ServiceNow",
                                                        local_favorites.len()
                                                    ))
                                                    .tooltip(
                                                        "Copy favorites saved only on this Mac \
                                                         into your ServiceNow favorites",
                                                    )
                                                    .on_click(cx.listener(
                                                        move |this, _, _, _| {
                                                            favorites::upload_local(
                                                                &this.services,
                                                                &upload_client,
                                                            );
                                                        },
                                                    )),
                                            ),
                                        )
                                    }),
                            )
                        },
                    )
                    .when(!self.show_table_catalog, |body| {
                        body.child(
                            v_flex()
                                .child(section_header("APPLICATIONS", &theme))
                                .when(application_rows.is_empty(), |section| {
                                    section.child(
                                        div()
                                            .px_2()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(if filtering {
                                                "No applications or modules match."
                                            } else {
                                                "No supported modules cached. Sync, or browse tables."
                                            }),
                                    )
                                })
                                .children(application_rows.iter().enumerate().map(
                                    |(ix, flat)| self.render_application_row(ix, flat, &theme, cx),
                                )),
                        )
                    })
                    .when(self.show_table_catalog, |body| {
                        body.child(
                            v_flex()
                                .child(section_header("TABLES", &theme))
                                .children(self.sidebar_tables.iter().cloned().enumerate().map(
                                    |(ix, table)| {
                                        let open_name = table.name.clone();
                                        let open_label = table.label.clone();
                                        let new_name = table.name.clone();
                                        tree_row(("sidebar-tbl", ix), 0, &theme)
                                            .on_click(cx.listener(
                                                move |this, event: &ClickEvent, window, cx| {
                                                    this.open_list_tab(
                                                        open_name.clone(),
                                                        Some(open_label.clone()),
                                                        String::new(),
                                                        event.modifiers().platform,
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .truncate()
                                                    .child(table.label),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .max_w(sp(96.0))
                                                    .text_size(sp(10.5))
                                                    .font_family(theme.mono_font_family.clone())
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(table.name),
                                            )
                                            .child(
                                                Button::new(("new-rec-side", ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Plus)
                                                    .tooltip("New record")
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.open_new_record_tab(
                                                                new_name.clone(),
                                                                this.split_enabled,
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                    },
                                )),
                        )
                    })
                    .when(!self.recents.is_empty(), |body| {
                        body.child(
                            v_flex().child(section_header("RECENT", &theme)).children(
                                self.recents
                                    .iter()
                                    .filter(|recent| recent.sys_id.is_some())
                                    .take(8)
                                    .cloned()
                                    .enumerate()
                                    .map(|(ix, recent)| {
                                        let table = recent.table.clone();
                                        let sys_id = recent.sys_id.clone().unwrap_or_default();
                                        tree_row(("recent-rec", ix), 0, &theme)
                                            .on_click(cx.listener(
                                                move |this, event: &ClickEvent, window, cx| {
                                                    this.open_record_tab(
                                                        table.clone(),
                                                        sys_id.clone(),
                                                        None,
                                                        event.modifiers().platform
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
                                                div()
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .child(recent.title),
                                            )
                                            .when_some(recent.subtitle, |row, subtitle| {
                                                row.child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_size(sp(10.5))
                                                        .text_color(theme.muted_foreground)
                                                        .truncate()
                                                        .child(subtitle),
                                                )
                                            })
                                    }),
                            ),
                        )
                    }),
            )
    }
}
