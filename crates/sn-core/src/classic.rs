//! Portable classic UI destinations and server-defined form layouts. No server scripts are run.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{ColSpan, FormGridLayout, GridItem, TableSchema};

/// A native-supported destination. `sys_id=-1` denotes a new record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClassicTarget {
    List {
        table: String,
        query: String,
        view: String,
    },
    Form {
        table: String,
        sys_id: String,
        query: String,
        view: String,
    },
}

impl ClassicTarget {
    /// Accepts same-instance classic links, including nav_to wrappers. Workspace, portal,
    /// processor and external links are deliberately rejected rather than approximated.
    pub fn from_url(origin: &str, link: &str) -> Option<Self> {
        Self::parse(origin, link, 0)
    }

    fn parse(origin: &str, link: &str, depth: usize) -> Option<Self> {
        if depth > 3 {
            return None;
        }
        let base = Url::parse(&format!("{}/", origin.trim_end_matches('/'))).ok()?;
        let url = base.join(link).ok()?;
        if url.origin() != base.origin() {
            return None;
        }
        if url.path() == "/nav_to.do" {
            let uri = url.query_pairs().find(|(k, _)| k == "uri")?.1.into_owned();
            return Self::parse(origin, &uri, depth + 1);
        }
        if let Some(target) = url
            .path()
            .strip_prefix("/now/nav/ui/classic/params/target/")
        {
            return Self::parse(origin, &urlencoding::decode(target).ok()?, depth + 1);
        }
        let path = url.path().strip_prefix('/')?;
        if path.contains('/') {
            return None;
        }
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let query = params.get("sysparm_query").cloned().unwrap_or_default();
        let view = normalize_view(params.get("sysparm_view").map(String::as_str).unwrap_or(""));
        if let Some(table) = path
            .strip_suffix("_list.do")
            .or_else(|| path.strip_suffix(".list"))
        {
            return valid_table_name(table).then(|| Self::List {
                table: table.into(),
                query,
                view,
            });
        }
        let table = path.strip_suffix(".do")?;
        let sys_id = params.get("sys_id")?;
        if !valid_table_name(table) || !(sys_id == "-1" || is_sys_id(sys_id)) {
            return None;
        }
        Some(Self::Form {
            table: table.into(),
            sys_id: sys_id.clone(),
            query,
            view,
        })
    }

    pub fn table(&self) -> &str {
        match self {
            Self::List { table, .. } | Self::Form { table, .. } => table,
        }
    }

    /// The instance-relative classic URL for this destination, as stored in
    /// `sys_ui_bookmark.url`. [`Self::from_url`] reads it back to an equal target.
    pub fn to_url(&self) -> String {
        let (path, sys_id, query, view) = match self {
            Self::List { table, query, view } => (format!("{table}_list.do"), None, query, view),
            Self::Form {
                table,
                sys_id,
                query,
                view,
            } => (format!("{table}.do"), Some(sys_id), query, view),
        };
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        if let Some(sys_id) = sys_id {
            params.append_pair("sys_id", sys_id);
        }
        if !query.is_empty() {
            params.append_pair("sysparm_query", query);
        }
        if !view.is_empty() {
            params.append_pair("sysparm_view", view);
        }
        let params = params.finish();
        if params.is_empty() {
            path
        } else {
            format!("{path}?{params}")
        }
    }
}

pub fn valid_table_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn is_sys_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn normalize_view(view: &str) -> String {
    if view.is_empty()
        || view.eq_ignore_ascii_case("default")
        || view.eq_ignore_ascii_case("Default view")
    {
        String::new()
    } else {
        view.to_string()
    }
}

/// Default-view keys remain compatible with existing local customizations.
pub fn view_cache_key(table: &str, view: &str) -> String {
    let view = normalize_view(view);
    if view.is_empty() {
        table.into()
    } else {
        format!("{table}@{view}")
    }
}

/// What a navigation tree node does when activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NavKind {
    /// An application, a module separator, or a favorites group. Holds children only.
    Group,
    Link {
        target: ClassicTarget,
    },
}

/// A node of the application menu or favorites tree. ServiceNow nests both: modules sit
/// under separators inside an application, and bookmarks sit inside bookmark groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavNode {
    /// The ServiceNow `sys_id` of the record behind the node.
    pub id: String,
    pub title: String,
    pub kind: NavKind,
    /// ServiceNow `order` among siblings. Only meaningful for favorites, which can be
    /// reordered from the client.
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub children: Vec<NavNode>,
}

impl NavNode {
    pub fn link(id: impl Into<String>, title: impl Into<String>, target: ClassicTarget) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: NavKind::Link { target },
            order: 0,
            children: Vec::new(),
        }
    }

    pub fn group(id: impl Into<String>, title: impl Into<String>, children: Vec<NavNode>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: NavKind::Group,
            order: 0,
            children,
        }
    }

    pub fn with_order(mut self, order: i64) -> Self {
        self.order = order;
        self
    }

    pub fn is_group(&self) -> bool {
        self.kind == NavKind::Group
    }

    pub fn target(&self) -> Option<&ClassicTarget> {
        match &self.kind {
            NavKind::Link { target } => Some(target),
            NavKind::Group => None,
        }
    }

    /// Every link in this subtree, depth first.
    pub fn links(&self) -> Vec<&NavNode> {
        let mut out = Vec::new();
        if !self.is_group() {
            out.push(self);
        }
        for child in &self.children {
            out.extend(child.links());
        }
        out
    }

    /// The subtree narrowed to `needle` (lower-case): a node whose title or table matches
    /// keeps everything beneath it, otherwise only its matching descendants survive.
    pub fn filtered(&self, needle: &str) -> Option<NavNode> {
        if needle.is_empty() || self.title.to_lowercase().contains(needle) {
            return Some(self.clone());
        }
        if self
            .target()
            .is_some_and(|target| target.table().contains(needle))
        {
            return Some(self.clone());
        }
        let children: Vec<NavNode> = self
            .children
            .iter()
            .filter_map(|child| child.filtered(needle))
            .collect();
        (!children.is_empty()).then(|| NavNode {
            children,
            ..self.clone()
        })
    }
}

/// One `sys_app_module` row, in menu order, already resolved to a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRow {
    pub id: String,
    pub title: String,
    pub is_separator: bool,
    /// `None` for separators and for modules snrust cannot open natively.
    pub target: Option<ClassicTarget>,
}

/// Nests one application's modules the way the classic navigator does: a titled separator
/// starts a section that owns the modules after it, up to the next separator. An untitled
/// separator is only a divider, so it ends the section. Unsupported modules are dropped and
/// sections left empty by that are omitted.
pub fn group_modules(rows: impl IntoIterator<Item = ModuleRow>) -> Vec<NavNode> {
    let mut top: Vec<NavNode> = Vec::new();
    let mut section: Option<NavNode> = None;
    let close = |section: &mut Option<NavNode>, top: &mut Vec<NavNode>| {
        if let Some(done) = section.take()
            && !done.children.is_empty()
        {
            top.push(done);
        }
    };
    for row in rows {
        if row.is_separator {
            close(&mut section, &mut top);
            if !row.title.trim().is_empty() {
                section = Some(NavNode::group(row.id, row.title, Vec::new()));
            }
            continue;
        }
        let Some(target) = row.target else {
            continue;
        };
        let node = NavNode::link(row.id, row.title, target);
        match &mut section {
            Some(open) => open.children.push(node),
            None => top.push(node),
        }
    }
    close(&mut section, &mut top);
    top
}

/// One `sys_ui_bookmark` row resolved to a native destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkRow {
    pub id: String,
    pub title: String,
    pub order: i64,
    /// `sys_id` of its `sys_ui_bookmark_group`, when grouped.
    pub group_id: Option<String>,
    pub target: ClassicTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkGroupRow {
    pub id: String,
    pub title: String,
    pub order: i64,
}

/// Builds the favorites tree. Groups and ungrouped favorites share one ordering at the top
/// level; rows with equal `order` keep the order ServiceNow returned them in. Groups are kept even when empty so favorites can be moved into them; a favorite
/// naming a group that was not returned (deleted, or hidden by an ACL) stays visible at the
/// top level rather than vanishing.
pub fn group_favorites(
    groups: impl IntoIterator<Item = BookmarkGroupRow>,
    bookmarks: impl IntoIterator<Item = BookmarkRow>,
) -> Vec<NavNode> {
    let mut top: Vec<(i64, NavNode)> = groups
        .into_iter()
        .map(|group| {
            (
                group.order,
                NavNode::group(group.id, group.title, Vec::new()).with_order(group.order),
            )
        })
        .collect();
    let mut bookmarks: Vec<BookmarkRow> = bookmarks.into_iter().collect();
    bookmarks.sort_by_key(|bookmark| bookmark.order);
    for bookmark in bookmarks {
        let node =
            NavNode::link(bookmark.id, bookmark.title, bookmark.target).with_order(bookmark.order);
        let parent = bookmark.group_id.as_ref().and_then(|group_id| {
            top.iter_mut()
                .find(|(_, candidate)| candidate.is_group() && candidate.id == *group_id)
        });
        match parent {
            Some((_, group)) => group.children.push(node),
            None => top.push((bookmark.order, node)),
        }
    }
    top.sort_by_key(|(order, _)| *order);
    top.into_iter().map(|(_, node)| node).collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationSnapshot {
    /// One group per application, holding its modules and module sections.
    pub applications: Vec<NavNode>,
    pub favorites: Vec<NavNode>,
}

impl NavigationSnapshot {
    /// The synced favorite that opens exactly `target`, if there is one.
    pub fn favorite_for(&self, target: &ClassicTarget) -> Option<&NavNode> {
        self.favorites
            .iter()
            .flat_map(NavNode::links)
            .find(|node| node.target() == Some(target))
    }

    pub fn favorite_groups(&self) -> impl Iterator<Item = &NavNode> {
        self.favorites.iter().filter(|node| node.is_group())
    }

    /// The favorites in `group`, or the top level for `None`.
    pub fn favorite_siblings(&self, group: Option<&str>) -> &[NavNode] {
        match group {
            None => &self.favorites,
            Some(group) => self
                .favorite_groups()
                .find(|node| node.id == group)
                .map(|node| node.children.as_slice())
                .unwrap_or(&[]),
        }
    }

    fn favorite_siblings_mut(&mut self, group: Option<&str>) -> Option<&mut Vec<NavNode>> {
        match group {
            None => Some(&mut self.favorites),
            Some(group) => self
                .favorites
                .iter_mut()
                .find(|node| node.is_group() && node.id == group)
                .map(|node| &mut node.children),
        }
    }

    /// The group holding favorite `id`: `Some(None)` at the top level, `None` if unknown.
    pub fn favorite_parent(&self, id: &str) -> Option<Option<String>> {
        if self.favorites.iter().any(|node| node.id == id) {
            return Some(None);
        }
        self.favorite_groups()
            .find(|group| group.children.iter().any(|node| node.id == id))
            .map(|group| Some(group.id.clone()))
    }

    /// An `order` that sorts after everything currently in `group`.
    pub fn next_favorite_order(&self, group: Option<&str>) -> i64 {
        self.favorite_siblings(group)
            .iter()
            .map(|node| node.order)
            .max()
            .unwrap_or(0)
            + 10
    }

    /// Detaches a favorite or group from wherever it is.
    pub fn take_favorite(&mut self, id: &str) -> Option<NavNode> {
        if let Some(ix) = self.favorites.iter().position(|node| node.id == id) {
            return Some(self.favorites.remove(ix));
        }
        self.favorites.iter_mut().find_map(|group| {
            let ix = group.children.iter().position(|node| node.id == id)?;
            Some(group.children.remove(ix))
        })
    }

    /// Places `node` in `group` immediately before `before` (or last), then renumbers that
    /// level 10, 20, 30… Returns the `(id, order)` pairs whose order changed and therefore
    /// need writing back. Returns `None`, leaving the tree untouched, if `group` is unknown.
    pub fn place_favorite(
        &mut self,
        node: NavNode,
        group: Option<&str>,
        before: Option<&str>,
    ) -> Option<Vec<(String, i64)>> {
        let siblings = self.favorite_siblings_mut(group)?;
        let at = before
            .and_then(|before| siblings.iter().position(|sibling| sibling.id == before))
            .unwrap_or(siblings.len());
        let placed = node.id.clone();
        siblings.insert(at, node);
        let mut changed = Vec::new();
        for (ix, sibling) in siblings.iter_mut().enumerate() {
            let order = (ix as i64 + 1) * 10;
            if sibling.order != order || sibling.id == placed {
                sibling.order = order;
                changed.push((sibling.id.clone(), order));
            }
        }
        Some(changed)
    }

    pub fn rename_favorite(&mut self, id: &str, title: &str) -> bool {
        fn rename(nodes: &mut [NavNode], id: &str, title: &str) -> bool {
            nodes.iter_mut().any(|node| {
                if node.id == id {
                    node.title = title.to_string();
                    true
                } else {
                    rename(&mut node.children, id, title)
                }
            })
        }
        rename(&mut self.favorites, id, title)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassicFormSection {
    pub id: String,
    pub title: String,
    pub elements: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassicFormView {
    /// Empty is the Default view; otherwise the internal `sys_ui_view.name`.
    pub name: String,
    pub label: String,
    pub sections: Vec<ClassicFormSection>,
}

impl ClassicFormView {
    /// Preserves section/field order and split-column markers; formatters and scripts are omitted.
    pub fn layout(&self, schema: &TableSchema) -> FormGridLayout {
        let mut layout = FormGridLayout::generate_smart_default(schema);
        layout.items.clear();
        let mut placed = HashSet::new();
        for section in &self.sections {
            if !section.title.is_empty() {
                layout.items.push(GridItem::SectionDivider {
                    id: section.id.clone(),
                    title: section.title.clone(),
                    collapsed: false,
                });
            }
            let mut split = false;
            let mut left: Vec<String> = Vec::new();
            let mut right: Vec<String> = Vec::new();
            let mut on_right = false;
            for element in section
                .elements
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(".end_split"))
            {
                match element {
                    ".start_split" => {
                        split = true;
                        on_right = false;
                    }
                    ".split" => on_right = true,
                    ".end_split" => {
                        for i in 0..left.len().max(right.len()) {
                            for column in [&left, &right] {
                                if let Some(element) = column.get(i) {
                                    layout.items.push(GridItem::Field {
                                        element: element.clone(),
                                        span: ColSpan::One,
                                    });
                                }
                            }
                        }
                        left.clear();
                        right.clear();
                        split = false;
                        on_right = false;
                    }
                    _ => {
                        let Some(field) = schema.fields.get(element) else {
                            continue;
                        };
                        if field.is_journal() || !placed.insert(element.to_string()) {
                            continue;
                        }
                        if split {
                            if on_right {
                                right.push(element.to_string());
                            } else {
                                left.push(element.to_string());
                            }
                        } else {
                            layout.items.push(GridItem::Field {
                                element: element.into(),
                                span: ColSpan::Two,
                            });
                        }
                    }
                }
            }
        }
        layout.hidden_fields = schema
            .fields
            .values()
            .filter(|f| !placed.contains(&f.element) && !f.is_journal())
            .map(|f| f.element.clone())
            .collect();
        layout
    }
}
