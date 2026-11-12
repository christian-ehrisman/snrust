//! Read-only synchronization of account-scoped classic UI metadata over the Table API.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use sn_core::{
    AccountIdentity, BookmarkGroupRow, BookmarkRow, ClassicFormSection, ClassicFormView,
    ClassicTarget, FieldValue, ModuleRow, NavNode, SavedFilter, SnError, SnResult,
    classic::{group_favorites, group_modules, is_sys_id, normalize_view, valid_table_name},
};

use crate::{
    SnClient,
    metadata::{display_or_raw, raw_str},
};

type Row = IndexMap<String, FieldValue>;

impl SnClient {
    /// Resolve the bearer token's principal, never the first visible sys_user record.
    pub async fn current_account(&self) -> SnResult<AccountIdentity> {
        let rows = self
            .metadata_rows(
                "sys_user",
                "sys_id=javascript:gs.getUserID()",
                "sys_id,user_name,name",
            )
            .await?;
        let row = rows.first().filter(|_| rows.len() == 1).ok_or_else(|| {
            SnError::InvalidConfig(
                "ServiceNow did not return the signed-in user; check sys_user read access.".into(),
            )
        })?;
        let account = AccountIdentity {
            sys_id: raw_str(row, "sys_id"),
            user_name: raw_str(row, "user_name"),
            display_name: raw_str(row, "name"),
        };
        if account.sys_id.is_empty() || account.user_name.is_empty() {
            return Err(SnError::InvalidConfig(
                "ServiceNow hid the signed-in user's sys_id or user_name.".into(),
            ));
        }
        Ok(account)
    }

    /// Pagination is essential on large instances: a truncated catalog silently loses parents.
    pub(crate) async fn metadata_rows(
        &self,
        table: &str,
        query: &str,
        fields: &str,
    ) -> SnResult<Vec<Row>> {
        let mut all = Vec::new();
        loop {
            let rows = self
                .get_raw_table_rows(
                    table,
                    &[
                        ("sysparm_query", query.to_string()),
                        ("sysparm_fields", fields.to_string()),
                        ("sysparm_limit", "1000".into()),
                        ("sysparm_offset", all.len().to_string()),
                        ("sysparm_no_count", "true".into()),
                    ],
                )
                .await?;
            let done = rows.len() < 1000;
            all.extend(rows);
            if done {
                return Ok(all);
            }
        }
    }

    /// Personal list layouts precede shared layouts, with child-table and named-view fallback.
    /// Reads only this token's user rows and shared rows, never another user's preferences.
    pub async fn fetch_list_columns(
        &self,
        chain: &[String],
        view: &str,
    ) -> SnResult<Option<Vec<String>>> {
        let rows = self.metadata_rows("sys_ui_list",
            &format!("nameIN{}^parentISEMPTY^user=javascript:gs.getUserID()^ORuserISEMPTY^ORDERBYsys_id", chain.join(",")),
            "sys_id,name,user,view,view.name").await?;
        let requested = normalize_view(view);
        let mut candidates: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                let table_rank = chain
                    .iter()
                    .position(|name| *name == raw_str(row, "name"))?;
                let candidate_view = row_view(row);
                let view_rank = if candidate_view == requested {
                    0
                } else if candidate_view.is_empty() {
                    1
                } else {
                    return None;
                };
                let shared = raw_str(row, "user").is_empty();
                Some(((view_rank, table_rank, shared), row))
            })
            .collect();
        candidates.sort_by_key(|(rank, _)| *rank);
        for (_, row) in candidates {
            let id = raw_str(row, "sys_id");
            let elements = self
                .metadata_rows(
                    "sys_ui_list_element",
                    &format!("list_id={id}^ORDERBYposition^ORDERBYsys_id"),
                    "element,position",
                )
                .await?;
            let mut columns = elements
                .iter()
                .filter_map(|row| {
                    let element = raw_str(row, "element");
                    (!element.is_empty() && !element.starts_with('.'))
                        .then_some((position(row), element))
                })
                .collect::<Vec<_>>();
            columns.sort_by_key(|(position, _)| *position);
            if !columns.is_empty() {
                return Ok(Some(
                    columns.into_iter().map(|(_, element)| element).collect(),
                ));
            }
        }
        Ok(None)
    }

    /// Classic form sections are ordered by sys_ui_form_section, not by their record IDs.
    pub async fn fetch_form_views(&self, chain: &[String]) -> SnResult<Vec<ClassicFormView>> {
        let rows = self
            .metadata_rows(
                "sys_ui_form",
                &format!("nameIN{}^ORDERBYsys_id", chain.join(",")),
                "sys_id,name,view,view.name,view.title",
            )
            .await?;
        let mut chosen: HashMap<String, &Row> = HashMap::new();
        for table in chain.iter().rev() {
            for row in rows.iter().filter(|row| raw_str(row, "name") == *table) {
                let name = row_view(row);
                let label = display_or_raw(row, "view").unwrap_or_default();
                if name.to_lowercase().contains("workspace")
                    || name == "sow"
                    || name.starts_with("sow_")
                    || label.to_lowercase().contains("workspace")
                {
                    continue;
                }
                chosen.insert(name, row);
            }
        }
        if chosen.is_empty() {
            return Ok(Vec::new());
        }
        let form_ids = chosen
            .values()
            .map(|row| raw_str(row, "sys_id"))
            .collect::<Vec<_>>()
            .join(",");
        let links = self
            .metadata_rows(
                "sys_ui_form_section",
                &format!("sys_ui_formIN{form_ids}^ORDERBYposition^ORDERBYsys_id"),
                "sys_ui_form,sys_ui_section,position",
            )
            .await?;
        let ids = links
            .iter()
            .map(|row| raw_str(row, "sys_ui_section"))
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let section_query = format!("sys_idIN{ids}^ORDERBYsys_id");
        let element_query = format!("sys_ui_sectionIN{ids}^ORDERBYposition^ORDERBYsys_id");
        let (sections, elements) = tokio::try_join!(
            self.metadata_rows("sys_ui_section", &section_query, "sys_id,caption"),
            self.metadata_rows(
                "sys_ui_element",
                &element_query,
                "sys_ui_section,element,position"
            )
        )?;
        let mut views = Vec::new();
        for (name, form) in chosen {
            let mut form_links: Vec<_> = links
                .iter()
                .filter(|row| raw_str(row, "sys_ui_form") == raw_str(form, "sys_id"))
                .collect();
            form_links.sort_by_key(|row| position(row));
            let mut form_sections = Vec::new();
            for link in form_links {
                let id = raw_str(link, "sys_ui_section");
                let title = sections
                    .iter()
                    .find(|row| raw_str(row, "sys_id") == id)
                    .and_then(|row| display_or_raw(row, "caption"))
                    .unwrap_or_default();
                let mut fields: Vec<_> = elements
                    .iter()
                    .filter(|row| raw_str(row, "sys_ui_section") == id)
                    .collect();
                fields.sort_by_key(|row| position(row));
                form_sections.push(ClassicFormSection {
                    id,
                    title,
                    elements: fields
                        .into_iter()
                        .map(|row| raw_str(row, "element"))
                        .collect(),
                });
            }
            if !form_sections.is_empty() {
                let label = if name.is_empty() {
                    "Default view".into()
                } else {
                    display_or_raw(form, "view").unwrap_or_else(|| name.clone())
                };
                views.push(ClassicFormView {
                    name,
                    label,
                    sections: form_sections,
                });
            }
        }
        views.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(views)
    }

    /// Menu role checks are visibility hints only; ServiceNow still enforces all data ACLs.
    ///
    /// Returns one group per application. Inside it, modules nest under titled separators as
    /// in the classic navigator (see [`group_modules`]).
    pub async fn fetch_application_menu(&self) -> SnResult<Vec<NavNode>> {
        let (apps, modules, role_rows) = tokio::try_join!(
            self.metadata_rows("sys_app_application", "active=true^ORDERBYorder^ORDERBYtitle", "sys_id,title,roles"),
            self.metadata_rows("sys_app_module", "active=true^link_typeINLIST,NEW,DETAIL,DIRECT,SEPARATOR^ORDERBYorder^ORDERBYtitle",
                "sys_id,title,application,roles,override_menu_roles,link_type,name,filter,query,view_name,order"),
            self.metadata_rows("sys_user_has_role", "user=javascript:gs.getUserID()^ORDERBYsys_id", "role.name")
        )?;
        let roles: HashSet<_> = role_rows
            .iter()
            .map(|row| raw_str(row, "role.name"))
            .collect();
        let mut result = Vec::new();
        for app in apps {
            let app_id = raw_str(&app, "sys_id");
            let parent_visible = roles_allow(&raw_str(&app, "roles"), &roles);
            let rows = modules
                .iter()
                .filter(|module| {
                    raw_str(module, "application") == app_id
                        && (parent_visible
                            || matches!(
                                raw_str(module, "override_menu_roles").as_str(),
                                "true" | "1"
                            ))
                        && roles_allow(&raw_str(module, "roles"), &roles)
                })
                .map(|row| {
                    let is_separator = raw_str(row, "link_type") == "SEPARATOR";
                    ModuleRow {
                        id: raw_str(row, "sys_id"),
                        title: raw_str(row, "title"),
                        is_separator,
                        target: if is_separator {
                            None
                        } else {
                            module_target(self.profile().base_url.as_str(), row)
                        },
                    }
                });
            let children = group_modules(rows);
            if !children.is_empty() {
                result.push(NavNode::group(app_id, raw_str(&app, "title"), children));
            }
        }
        Ok(result)
    }

    /// The signed-in user's favorites as a tree: bookmark groups hold their bookmarks.
    ///
    /// Bookmarks that do not open a classic list or form are omitted. Instances without the
    /// bookmark group table (or without read access to it) yield a flat list.
    pub async fn fetch_user_favorites(&self) -> SnResult<Vec<NavNode>> {
        let rows = self
            .metadata_rows(
                BOOKMARK_TABLE,
                "user=javascript:gs.getUserID()^ORDERBYorder^ORDERBYsys_id",
                "sys_id,title,url,group,order",
            )
            .await?;
        let groups = match self
            .metadata_rows(
                BOOKMARK_GROUP_TABLE,
                "user=javascript:gs.getUserID()^ORDERBYorder^ORDERBYsys_id",
                "sys_id,title,order",
            )
            .await
        {
            Ok(groups) => groups,
            Err(err) => {
                tracing::warn!(%err, "Cannot read favorite groups; showing favorites ungrouped");
                Vec::new()
            }
        };
        let bookmarks = rows.iter().filter_map(|row| {
            let target = ClassicTarget::from_url(&self.profile().base_url, &raw_str(row, "url"))?;
            let group = raw_str(row, "group");
            Some(BookmarkRow {
                id: raw_str(row, "sys_id"),
                title: raw_str(row, "title"),
                order: order(row),
                group_id: (!group.is_empty()).then_some(group),
                target,
            })
        });
        let groups = groups.iter().map(|row| BookmarkGroupRow {
            id: raw_str(row, "sys_id"),
            title: raw_str(row, "title"),
            order: order(row),
        });
        Ok(group_favorites(groups, bookmarks))
    }

    async fn current_user_sys_id(&self) -> SnResult<String> {
        match &self.profile().account {
            Some(account) if !account.sys_id.is_empty() => Ok(account.sys_id.clone()),
            _ => Ok(self.current_account().await?.sys_id),
        }
    }

    /// Adds a favorite for the signed-in user and returns its `sys_id`.
    pub async fn create_favorite(
        &self,
        title: &str,
        target: &ClassicTarget,
        group_id: Option<&str>,
        order: i64,
    ) -> SnResult<String> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), title.into());
        payload.insert("url".into(), target.to_url().into());
        payload.insert("user".into(), self.current_user_sys_id().await?.into());
        payload.insert("order".into(), order.to_string().into());
        if let Some(group_id) = group_id {
            payload.insert("group".into(), group_id.into());
        }
        Ok(self.create_record(BOOKMARK_TABLE, &payload).await?.sys_id)
    }

    /// Applies any combination of rename, regroup (`Some(None)` ungroups) and reorder.
    pub async fn update_favorite(&self, sys_id: &str, change: &FavoriteChange) -> SnResult<()> {
        let mut payload = serde_json::Map::new();
        if let Some(title) = &change.title {
            payload.insert("title".into(), title.clone().into());
        }
        if let Some(group) = &change.group_id {
            payload.insert("group".into(), group.clone().unwrap_or_default().into());
        }
        if let Some(order) = change.order {
            payload.insert("order".into(), order.to_string().into());
        }
        if payload.is_empty() {
            return Ok(());
        }
        self.patch_record(BOOKMARK_TABLE, sys_id, &payload).await?;
        Ok(())
    }

    pub async fn delete_favorite(&self, sys_id: &str) -> SnResult<()> {
        self.delete_record(BOOKMARK_TABLE, sys_id).await
    }

    /// Creates a favorites group for the signed-in user and returns its `sys_id`.
    pub async fn create_favorite_group(&self, title: &str, order: i64) -> SnResult<String> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), title.into());
        payload.insert("user".into(), self.current_user_sys_id().await?.into());
        payload.insert("order".into(), order.to_string().into());
        Ok(self
            .create_record(BOOKMARK_GROUP_TABLE, &payload)
            .await?
            .sys_id)
    }

    /// Renames and/or reorders a favorites group; `None` leaves a field as it is.
    pub async fn update_favorite_group(
        &self,
        sys_id: &str,
        title: Option<&str>,
        order: Option<i64>,
    ) -> SnResult<()> {
        let mut payload = serde_json::Map::new();
        if let Some(title) = title {
            payload.insert("title".into(), title.into());
        }
        if let Some(order) = order {
            payload.insert("order".into(), order.to_string().into());
        }
        if payload.is_empty() {
            return Ok(());
        }
        self.patch_record(BOOKMARK_GROUP_TABLE, sys_id, &payload)
            .await?;
        Ok(())
    }

    /// Deletes a group after moving `member_ids` to the top level, so removing a group never
    /// removes the favorites inside it.
    pub async fn delete_favorite_group(&self, sys_id: &str, member_ids: &[String]) -> SnResult<()> {
        let ungroup = FavoriteChange {
            group_id: Some(None),
            ..FavoriteChange::default()
        };
        for member in member_ids {
            self.update_favorite(member, &ungroup).await?;
        }
        self.delete_record(BOOKMARK_GROUP_TABLE, sys_id).await
    }
}

impl SnClient {
    /// Saved filters for a table and its parents: the signed-in user's own (editable) and
    /// global ones (read-only). Filters shared with a group are not listed.
    pub async fn fetch_saved_filters(&self, chain: &[String]) -> SnResult<Vec<SavedFilter>> {
        let rows = self
            .metadata_rows(
                FILTER_TABLE,
                &format!(
                    "tableIN{}^user=javascript:gs.getUserID()^ORuserISEMPTY^groupISEMPTY^ORDERBYtitle",
                    chain.join(",")
                ),
                "sys_id,title,table,filter,user",
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| SavedFilter {
                id: raw_str(row, "sys_id"),
                title: raw_str(row, "title"),
                table: raw_str(row, "table"),
                filter: raw_str(row, "filter"),
                editable: !raw_str(row, "user").is_empty(),
            })
            .filter(|filter| !filter.title.is_empty())
            .collect())
    }

    /// Saves a personal filter and returns its `sys_id`.
    pub async fn create_saved_filter(
        &self,
        title: &str,
        table: &str,
        filter: &str,
    ) -> SnResult<String> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), title.into());
        payload.insert("table".into(), table.into());
        payload.insert("filter".into(), filter.into());
        payload.insert("user".into(), self.current_user_sys_id().await?.into());
        Ok(self.create_record(FILTER_TABLE, &payload).await?.sys_id)
    }

    /// Renames and/or replaces the query of a saved filter; `None` leaves a field as it is.
    pub async fn update_saved_filter(
        &self,
        sys_id: &str,
        title: Option<&str>,
        filter: Option<&str>,
    ) -> SnResult<()> {
        let mut payload = serde_json::Map::new();
        if let Some(title) = title {
            payload.insert("title".into(), title.into());
        }
        if let Some(filter) = filter {
            payload.insert("filter".into(), filter.into());
        }
        if payload.is_empty() {
            return Ok(());
        }
        self.patch_record(FILTER_TABLE, sys_id, &payload).await?;
        Ok(())
    }

    pub async fn delete_saved_filter(&self, sys_id: &str) -> SnResult<()> {
        self.delete_record(FILTER_TABLE, sys_id).await
    }
}

const FILTER_TABLE: &str = "sys_filter";
const BOOKMARK_TABLE: &str = "sys_ui_bookmark";
const BOOKMARK_GROUP_TABLE: &str = "sys_ui_bookmark_group";

/// Fields to change on a favorite; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FavoriteChange {
    pub title: Option<String>,
    /// `Some(None)` moves the favorite out of its group.
    pub group_id: Option<Option<String>>,
    pub order: Option<i64>,
}

fn order(row: &Row) -> i64 {
    raw_str(row, "order").trim().parse().unwrap_or(0)
}

fn row_view(row: &Row) -> String {
    let name = raw_str(row, "view.name");
    if !name.is_empty() {
        normalize_view(&name)
    } else {
        normalize_view(&display_or_raw(row, "view").unwrap_or_default())
    }
}

fn position(row: &Row) -> i32 {
    raw_str(row, "position").parse().unwrap_or(0)
}

fn roles_allow(required: &str, roles: &HashSet<String>) -> bool {
    required.trim().is_empty()
        || roles.contains("admin")
        || required.split(',').any(|role| roles.contains(role.trim()))
}

fn module_target(origin: &str, row: &Row) -> Option<ClassicTarget> {
    let table = raw_str(row, "name");
    let args = raw_str(row, "query");
    let mut view = normalize_view(&raw_str(row, "view_name"));
    let mut filter = raw_str(row, "filter");
    if args.trim_start_matches('?').starts_with("sysparm_") {
        for (key, value) in url::form_urlencoded::parse(args.trim_start_matches('?').as_bytes()) {
            match key.as_ref() {
                "sysparm_query" => filter = value.into_owned(),
                "sysparm_view" => view = normalize_view(&value),
                _ => {}
            }
        }
    } else if filter.is_empty() && matches!(raw_str(row, "link_type").as_str(), "LIST" | "NEW") {
        filter = args.clone();
    }
    match raw_str(row, "link_type").as_str() {
        "LIST" if valid_table_name(&table) => Some(ClassicTarget::List {
            table,
            query: filter,
            view,
        }),
        "NEW" if valid_table_name(&table) => Some(ClassicTarget::Form {
            table,
            sys_id: "-1".into(),
            query: filter,
            view,
        }),
        "DETAIL" if valid_table_name(&table) => {
            let id = filter.strip_prefix("sys_id=").unwrap_or(&args);
            if is_sys_id(id) {
                Some(ClassicTarget::Form {
                    table,
                    sys_id: id.into(),
                    query: String::new(),
                    view,
                })
            } else {
                ClassicTarget::from_url(origin, &format!("{table}.do?{args}")).map(|mut target| {
                    if let ClassicTarget::Form {
                        view: target_view, ..
                    } = &mut target
                        && target_view.is_empty()
                    {
                        *target_view = view;
                    }
                    target
                })
            }
        }
        "DIRECT" => ClassicTarget::from_url(origin, &args),
        _ => None,
    }
}
