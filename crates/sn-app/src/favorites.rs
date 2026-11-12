//! Two-way favorites.
//!
//! Favorites are the signed-in user's own `sys_ui_bookmark` and `sys_ui_bookmark_group`
//! rows, so changes made here appear in the ServiceNow browser UI and vice versa. Every
//! change is applied to the cached tree first, then written to ServiceNow, then the tree is
//! refetched so the sidebar always ends on what the instance actually holds. If ServiceNow
//! refuses a new favorite it is kept on this Mac instead, so starring never silently fails.

use std::future::Future;

use sn_api::{FavoriteChange, SnClient};
use sn_cache::{FavoriteItem, FavoriteKind};
use sn_core::{ClassicTarget, InstanceId, NavNode, NavigationSnapshot, SnResult};

use crate::state::{AppServices, WorkspaceCommand};

/// Cache key for the synced navigation tree. Versioned: the earlier flat shape is ignored.
pub const NAVIGATION_KEY: &str = "navigation.v2";

/// Marks a favorite that has been shown optimistically but not yet created in ServiceNow.
const PENDING_PREFIX: &str = "pending:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FavoriteRef {
    /// A `sys_ui_bookmark.sys_id`.
    Synced(String),
    /// A favorite stored only in the local cache.
    Local(String),
}

pub fn load_navigation(services: &AppServices, instance_id: &InstanceId) -> NavigationSnapshot {
    match services
        .cache
        .get_account_metadata(instance_id, NAVIGATION_KEY)
    {
        Ok(snapshot) => snapshot.unwrap_or_default(),
        Err(err) => {
            tracing::warn!(%err, "Cannot read cached navigation");
            NavigationSnapshot::default()
        }
    }
}

fn save_navigation(
    services: &AppServices,
    instance_id: &InstanceId,
    snapshot: &NavigationSnapshot,
) {
    if let Err(err) = services
        .cache
        .save_account_metadata(instance_id, NAVIGATION_KEY, snapshot)
    {
        tracing::warn!(%err, "Cannot cache navigation");
    }
}

/// Where a locally stored favorite points.
pub fn local_target(item: &FavoriteItem) -> ClassicTarget {
    match &item.kind {
        FavoriteKind::Classic { target } => target.clone(),
        FavoriteKind::Table { table, query, .. } => ClassicTarget::List {
            table: table.clone(),
            query: query.clone(),
            view: String::new(),
        },
        FavoriteKind::Record { table, sys_id, .. } => ClassicTarget::Form {
            table: table.clone(),
            sys_id: sys_id.clone(),
            query: String::new(),
            view: String::new(),
        },
    }
}

/// The favorite that opens exactly `target`, preferring the synced one.
pub fn find(
    services: &AppServices,
    instance_id: &InstanceId,
    target: &ClassicTarget,
) -> Option<FavoriteRef> {
    if let Some(node) = load_navigation(services, instance_id).favorite_for(target) {
        return Some(FavoriteRef::Synced(node.id.clone()));
    }
    services
        .cache
        .list_favorites(instance_id)
        .unwrap_or_default()
        .into_iter()
        .find(|item| local_target(item) == *target)
        .map(|item| FavoriteRef::Local(item.id))
}

/// Shows `edit` immediately, performs `write`, then resyncs from ServiceNow either way so a
/// refused write is rolled back by the refetch.
fn apply(
    services: &AppServices,
    client: &SnClient,
    edit: impl FnOnce(&mut NavigationSnapshot),
    write: impl Future<Output = SnResult<()>> + Send + 'static,
    failure: &'static str,
) {
    let instance_id = client.profile().id.clone();
    let mut snapshot = load_navigation(services, &instance_id);
    edit(&mut snapshot);
    save_navigation(services, &instance_id, &snapshot);
    services.send_cmd(WorkspaceCommand::RefreshSidebar);

    let services = services.clone();
    services.clone().tokio_handle.spawn(async move {
        if let Err(err) = write.await {
            services.send_cmd(WorkspaceCommand::ToastError(format!("{failure}: {err}")));
        }
        services.send_cmd(WorkspaceCommand::SyncNavigation);
    });
}

pub fn add(services: &AppServices, client: &SnClient, title: String, target: ClassicTarget) {
    let instance_id = client.profile().id.clone();
    let order = load_navigation(services, &instance_id).next_favorite_order(None);
    let pending = NavNode::link(
        format!("{PENDING_PREFIX}{}", uuid::Uuid::new_v4()),
        title.clone(),
        target.clone(),
    )
    .with_order(order);
    let fallback = services.clone();
    let write_client = client.clone();
    apply(
        services,
        client,
        |snapshot| snapshot.favorites.push(pending),
        async move {
            if let Err(err) = write_client
                .create_favorite(&title, &target, None, order)
                .await
            {
                // Keep the star working without write access to sys_ui_bookmark.
                let local = FavoriteItem {
                    id: format!("fav:local:{}", uuid::Uuid::new_v4()),
                    kind: FavoriteKind::Classic { target },
                    title,
                    subtitle: None,
                };
                fallback.cache.upsert_favorite(&instance_id, &local)?;
                fallback.send_cmd(WorkspaceCommand::ToastInfo(format!(
                    "ServiceNow did not accept the favorite, so it is saved on this Mac only. \
                     ({err})"
                )));
            }
            Ok(())
        },
        "Cannot save favorite",
    );
}

pub fn remove(services: &AppServices, client: &SnClient, favorite: FavoriteRef) {
    match favorite {
        FavoriteRef::Local(id) => {
            if let Err(err) = services.cache.remove_favorite(&client.profile().id, &id) {
                services.send_cmd(WorkspaceCommand::ToastError(err.to_string()));
            }
            services.send_cmd(WorkspaceCommand::RefreshSidebar);
        }
        FavoriteRef::Synced(id) if id.starts_with(PENDING_PREFIX) => {
            services.send_cmd(WorkspaceCommand::ToastInfo(
                "That favorite is still being saved. Try again in a moment.".into(),
            ));
        }
        FavoriteRef::Synced(id) => {
            let write_client = client.clone();
            let write_id = id.clone();
            apply(
                services,
                client,
                |snapshot| {
                    snapshot.take_favorite(&id);
                },
                async move { write_client.delete_favorite(&write_id).await },
                "Cannot remove favorite",
            );
        }
    }
}

pub fn rename(
    services: &AppServices,
    client: &SnClient,
    id: String,
    title: String,
    is_group: bool,
) {
    let write_client = client.clone();
    let (write_id, write_title) = (id.clone(), title.clone());
    apply(
        services,
        client,
        |snapshot| {
            snapshot.rename_favorite(&id, &title);
        },
        async move {
            if is_group {
                write_client
                    .update_favorite_group(&write_id, Some(&write_title), None)
                    .await
            } else {
                let change = FavoriteChange {
                    title: Some(write_title),
                    ..FavoriteChange::default()
                };
                write_client.update_favorite(&write_id, &change).await
            }
        },
        "Cannot rename favorite",
    );
}

/// Moves a favorite or group to `group` (top level for `None`), placing it before `before`
/// or last. Covers both regrouping and reordering. Groups cannot be nested.
pub fn place(
    services: &AppServices,
    client: &SnClient,
    id: String,
    group: Option<String>,
    before: Option<String>,
) {
    let instance_id = client.profile().id.clone();
    let mut preview = load_navigation(services, &instance_id);
    let Some(old_parent) = preview.favorite_parent(&id) else {
        return;
    };
    let Some(node) = preview.take_favorite(&id) else {
        return;
    };
    if before.as_deref() == Some(id.as_str()) || (node.is_group() && group.is_some()) {
        return;
    }
    let Some(changed) = preview.place_favorite(node, group.as_deref(), before.as_deref()) else {
        return;
    };
    let group_ids: Vec<String> = preview
        .favorite_groups()
        .map(|group| group.id.clone())
        .collect();
    let regrouped = old_parent != group;
    let write_client = client.clone();
    apply(
        services,
        client,
        |snapshot| *snapshot = preview,
        async move {
            for (changed_id, order) in changed {
                if group_ids.contains(&changed_id) {
                    write_client
                        .update_favorite_group(&changed_id, None, Some(order))
                        .await?;
                } else {
                    let change = FavoriteChange {
                        title: None,
                        group_id: (changed_id == id && regrouped).then(|| group.clone()),
                        order: Some(order),
                    };
                    write_client.update_favorite(&changed_id, &change).await?;
                }
            }
            Ok(())
        },
        "Cannot move favorite",
    );
}

/// Creates a group, optionally moving one favorite into it.
pub fn create_group(
    services: &AppServices,
    client: &SnClient,
    title: String,
    move_in: Option<String>,
) {
    let order = load_navigation(services, &client.profile().id).next_favorite_order(None);
    let write_client = client.clone();
    apply(
        services,
        client,
        |_| {},
        async move {
            let group_id = write_client.create_favorite_group(&title, order).await?;
            if let Some(favorite) = move_in {
                let change = FavoriteChange {
                    group_id: Some(Some(group_id)),
                    order: Some(10),
                    title: None,
                };
                write_client.update_favorite(&favorite, &change).await?;
            }
            Ok(())
        },
        "Cannot create favorites group",
    );
}

/// Removes a group. The favorites inside it move to the top level.
pub fn delete_group(services: &AppServices, client: &SnClient, group_id: String) {
    let members: Vec<String> = load_navigation(services, &client.profile().id)
        .favorite_siblings(Some(&group_id))
        .iter()
        .map(|node| node.id.clone())
        .collect();
    let write_client = client.clone();
    let write_group = group_id.clone();
    apply(
        services,
        client,
        |snapshot| {
            if let Some(group) = snapshot.take_favorite(&group_id) {
                snapshot.favorites.extend(group.children);
            }
        },
        async move {
            write_client
                .delete_favorite_group(&write_group, &members)
                .await
        },
        "Cannot remove favorites group",
    );
}

/// Copies favorites stored only on this Mac to ServiceNow, removing each local copy once
/// ServiceNow has accepted it.
pub fn upload_local(services: &AppServices, client: &SnClient) {
    let instance_id = client.profile().id.clone();
    let locals = services
        .cache
        .list_favorites(&instance_id)
        .unwrap_or_default();
    if locals.is_empty() {
        return;
    }
    let mut order = load_navigation(services, &instance_id).next_favorite_order(None);
    let cleanup = services.clone();
    let write_client = client.clone();
    apply(
        services,
        client,
        |_| {},
        async move {
            let total = locals.len();
            let mut uploaded = 0;
            let mut last_error = None;
            for item in locals {
                match write_client
                    .create_favorite(&item.title, &local_target(&item), None, order)
                    .await
                {
                    Ok(_) => {
                        cleanup.cache.remove_favorite(&instance_id, &item.id)?;
                        uploaded += 1;
                        order += 10;
                    }
                    Err(err) => last_error = Some(err),
                }
            }
            cleanup.send_cmd(WorkspaceCommand::ToastInfo(format!(
                "Uploaded {uploaded} of {total} favorites to ServiceNow."
            )));
            match last_error {
                Some(err) => Err(err),
                None => Ok(()),
            }
        },
        "Some favorites could not be uploaded",
    );
}
