# snrust — High-Speed macOS-Native ServiceNow Client

`snrust` is a **120fps Metal-accelerated macOS desktop client** for ServiceNow built in Rust on **`gpui-kit` (`gpui` + `gpui-component` `0.9`)** and **Tokio**. It eliminates browser DOM/bundle lag with a 3-tier local SQLite WAL + in-memory LRU cache, OAuth 2.0 PKCE authentication backed by macOS Keychain, classic List and Form views with an interactive **Rearrangeable 2-Column Auto-Grid**, and **real-time inbound field updates** over ServiceNow AMB WebSockets with automatic adaptive REST delta-polling fallback.

---

## Development Status (MVP)

| Area | Status | Notes |
|---|---|---|
| **OAuth 2.0 + PKCE & Keychain** | Complete | Loopback PKCE (`S256`), optional `client_secret`, verified account identity, independent users on the same instance, isolated Keychain tokens/cache, single-flight auto-refresh, `401` retry |
| **3-Tier Local Cache (`sn-cache`)** | Complete | Bundled SQLite (WAL + `mmap` + FTS5) + `moka` hot LRU; 0ms SWR List-to-Form hydration & partial-safe `SnRecord` merging |
| **Real-Time Engine (`sn-realtime`)** | Complete | ServiceNow `/amb` CometD/Bayeux Record Watcher primary + automatic 1.5s/10s REST watermark delta-polling fallback & journal tailing |
| **Classic List View** | Complete | Virtualized `DataTable`, inherited/global audit fields, personal/shared ServiceNow column layouts, sortable/resizable/reorderable columns, docked column chooser with dot-walking, condition builder with AND / OR / new-query sets, breadcrumb chips, saved-filter pills (`sys_filter`), clickable references with hover preview, copy display/raw values |
| **Classic Form Views & Auto-Grid** | Complete | Named ServiceNow views with ordered sections/fields, searchable Add Field, per-account/table/view local layouts, typed controls and reference lookup, Activity Stream, dirty-safe live updates, explicit `⌘S` Save / `⇧⌘S` Update |
| **Application navigation & favorites** | Complete for classic targets | Role-filtered application menus nested under their module separators, two-way ServiceNow favorites with groups, native list/form routing; excludes workspaces, portals, reports, scripted modules and external links |
| **Settings** | Complete | Appearance (light / dark / match system), UI scale 80–150%, density, rows per page, timing readouts, accounts, local cache |
| **Local Live Demo Sandbox** | Complete | One-click embedded loopback ServiceNow simulator (180+ incidents, users, groups, CIs, and live server push button) |

### Post-MVP Roadmap / TODOs
- Declarative ServiceNow UI Policy (`sys_ui_policy` / `sys_ui_policy_action`) condition evaluation in Rust.
- Related Lists tabs (`task_ci`, `incident` child tasks, approvals) at the bottom of Form View.
- Attachment upload/preview (`sys_attachment` API).

---

## Workspace Architecture

```text
snrust/
├── Cargo.toml                  # Workspace root & dependency pins
├── scripts/
│   ├── bundle-macos.sh         # Builds native dist/snrust.app (Retina + 120Hz ProMotion)
│   └── servicenow/
│       └── diagnose-oauth-access.js  # Read-only Scripts - Background report for OAuth 401/403 issues
├── vendor/
│   └── gpui-pre-macos/         # Stable rustc 1.97 compatibility shim for gpui-pre-macos 0.3.12
└── crates/
    ├── sn-core/                # Domain models, sys_db_object inheritance, EncodedQuery, FormGridLayout, FormSessionState
    ├── sn-cache/               # 3-tier SQLite WAL + FTS5 + moka in-memory LRU cache
    ├── sn-api/                 # OAuth 2.0 PKCE loopback auth, macOS Keychain vault, HTTP/2 Table & Metadata REST client
    ├── sn-realtime/            # /amb CometD WebSocket Record Watcher + adaptive REST watermark delta-poller
    └── sn-app/                 # macOS GPUI desktop frontend (WorkspaceShell, ListViewPanel, FormViewPanel, CommandPalette)
```

---

## Quick Start

### Build & Run
```bash
cargo run -p sn-app --release
```

### Build Native macOS `.app` Bundle (`dist/snrust.app`)
```bash
./scripts/bundle-macos.sh
open dist/snrust.app
```

### Try Immediately Without Credentials (Demo Sandbox)
1. Launch `snrust` and click **Connect ServiceNow / Demo Sandbox** (or press `⇧⌘,`).
2. Click **Launch Demo Sandbox** in the left panel.
3. Open any incident (`INC0010001`), edit a field (or leave it clean), and click **Simulate Live Push** in the form header to watch real-time inbound field updates, live highlight pulses, Activity Stream updates, and dirty-field conflict protection in action.

---

## Connecting a Real ServiceNow Instance (OAuth 2.0 + PKCE)

1. In your ServiceNow instance, navigate to **System OAuth → Application Registry** (`oauth_entity.list`).
2. Click **New → Create an OAuth API endpoint for external clients**:
   - **Name**: `snrust Desktop`
   - **Redirect URL**: `http://127.0.0.1:8910/oauth/callback` *(one-click copyable inside `snrust`'s Connect dialog; port is configurable per instance)*
   - **PKCE / Public Client**: Either enable *Public Client* (no secret needed) or copy the generated *Client Secret* (`snrust` supports both and stores secrets exclusively in macOS Keychain).
   - **Scope Restriction**: *Broadly scoped* (or *Useraccount scoped* with the `useraccount` auth scope). *Securely scoped* only allows REST APIs mapped to the registry's auth scopes, so every Table API call returns `403`.
   - **Enforce Token Restrictions** and **Bind Access Token to Session**: leave unchecked.
   - **Accessible from**: All application scopes.
3. In `snrust`, press `⇧⌘,` (or click the instance pill in the title bar), enter your **Instance Name/URL** (e.g. `dev12345` or `https://acme.service-now.com`) and **Client ID** (plus optional **Client Secret**), and click **Sign in with OAuth 2.0 + PKCE**.
4. Authenticate in your default macOS browser (SSO / Okta / Entra MFA supported); `snrust` captures the loopback callback automatically and stores tokens in **macOS Keychain** (`dev.snrust.oauth`).
5. `snrust` resolves the token's current user through `sys_user` (`sys_id=javascript:gs.getUserID()`) before saving the connection. This requires read access to that user's `sys_id`, `user_name`, and `name`. If access fails, the dialog reports the error rather than saving an unverified account.

### Multiple users on one instance

In **Instance Connections** (`⇧⌘,`), click **Add user** on an existing connection, then authenticate as the other user. The instance URL and OAuth Client ID can be reused; confidential clients still need their secret supplied for the new connection. The saved connection and title bar display the verified username.

The browser may reuse its current ServiceNow/SSO session. Sign out there first, or copy the authorization URL into a private browser window. snrust cannot silently switch the browser's identity.

Tokens, record caches, navigation, favorites, and layout overrides are isolated by account profile ID, **not by instance hostname**. Signing in as a different user creates a separate profile instead of replacing another user's cached data. Reconnecting the same verified account reuses its profile. Legacy profiles without verified identities remain available; re-signing in creates a verified profile rather than assuming ownership of the old cache.

### Troubleshooting: signed in, but every request returns `403`

ServiceNow issued a token but its OAuth/REST configuration refuses to let it call the Table API. ServiceNow's detail (shown in the sign-in dialog and the log) is usually `Missing required api access scope`. Common causes, in order:

1. The registry's **Auth Scopes** list omits `useraccount` while a REST API Auth Scope record (`sys_api_access_scope`) gates the Table API. New registries start with no auth scopes.
2. Application Registry **Scope Restriction** is *Securely scoped*, or **Enforce Token Restrictions** is checked.
3. The `glide.oauth.token.scope.useraccount` system property no longer points at the `useraccount` auth scope record, so `useraccount` tokens lose their bypass of the scope check.
4. A REST API Access Policy (`sys_api_access_policy.list`) covers the Table API without an OAuth inbound authentication profile. This usually surfaces as `401` instead.

To pinpoint it, run [`scripts/servicenow/diagnose-oauth-access.js`](scripts/servicenow/diagnose-oauth-access.js) as an admin in **System Definition → Scripts - Background** (global scope). It is read-only and prints no secrets. After changing the registry, sign in again from Instance Connections (`⇧⌘,`) so ServiceNow issues a new token.

`snrust` logs every rejected request (method, URL, status, and ServiceNow's error body; never tokens) to `~/Library/Logs/snrust/snrust.log`. The log rotates to `snrust.log.1` at 5 MB, and `RUST_LOG=debug` raises verbosity.

---

## Classic UI Synchronization

### Columns and inherited fields

- The paginated table catalog resolves complete parent chains, including custom/task extensions such as `rm_story`. Child dictionary definitions override parent definitions; global audit fields include **Created** (`sys_created_on`) and **Created by** (`sys_created_by`).
- Lists read `sys_ui_list` and ordered `sys_ui_list_element` records. For the requested view, the most-specific table wins and the current user's layout takes precedence over the shared layout. Missing named views fall back to the default view.
- Column selection and reordering in snrust are **local overrides**, not changes to ServiceNow. Choose **Use ServiceNow columns** in the column chooser to remove the override and resync. **⌘R** refreshes server metadata and records.
- **Columns** opens a chooser docked beside the table, so the headers stay visible and update as you toggle. The **Shown** section reorders by dragging the grip or with **⌥↑ / ⌥↓**; **All fields** is grouped by defining table. Type to search by label or field name, **↑ ↓** to move, **↵** to toggle, **Esc** to close. Expanding a reference field lists the fields of the table it points to, which adds dot-walked columns such as `caller_id.email` (up to two levels).

### Filters

- **Filter** opens the condition builder inline above the table: one row per condition with a field, an operator suited to the field's type, and a value editor (choice list, true/false, reference picker, or text). **AND** and **OR** add conditions; **OR conditions** starts a second set (`^NQ`). **Run** is **⌘↵**. **Edit as text** shows the same filter as an encoded query; both edit one model, and anything the builder cannot represent (`RLQUERY`, `GROUPBY`) is kept as an editable text row in place.
- The active filter is shown as breadcrumb chips using field labels and display values. Click a chip to open the builder, **✕** to remove that condition, or the **›** after a chip to keep only the conditions up to it. **All** clears the filter.
- The pills above the list are ServiceNow saved filters (`sys_filter`) for the table and its parents: your own, plus global ones (marked with a lock, read-only). **+** saves the current filter. Right-click one of yours to rename it, update it to the current filter, duplicate or delete it. Filters shared with a group are not listed.
- Dot-walked fields can be filtered in text mode; the builder's field list covers the table's own fields.

### References

A reference value in a list cell or form field is a link: **click** opens the record in a tab, **⌘-click** opens it in the split pane, and hovering shows a preview card with its key fields. In forms, the rest of the field (and its search icon) opens the picker to change the value.

### Form views

Use **View: Default** in the form header to choose a named classic ServiceNow view. The client reads `sys_ui_form`, `sys_ui_form_section`, `sys_ui_section`, and `sys_ui_element`, preserving section/field order and split-column layout. Switching views does not discard unsaved field or journal edits.

**Customize Grid → Add Field** has a search box. Local customizations are saved independently for each account, table, and view. **Reset to SN View** removes the local override. When view metadata is unavailable, the client retains cached views or uses the dictionary-generated grid and reports the sync failure.

This is a native rendering of classic metadata, not an embedded ServiceNow browser. UI macros/formatters, client scripts, scripted view rules, UI policies, and related lists are not executed. Literal new-record module defaults are supported; scripted defaults are not evaluated locally.

### Applications and favorites

The sidebar lists **Applications**, built from active `sys_app_application`/`sys_app_module` records and filtered by the current user's roles. Modules nest under titled module separators as in the classic navigator. Search matches application, section and module names and table names, and keeps the parents of anything that matches. **Browse tables** retains direct table access; the sidebar refresh button resyncs menus and favorites. Which nodes are open is remembered per account.

**Favorites** are your ServiceNow favorites (`sys_ui_bookmark`), nested in their groups (`sys_ui_bookmark_group`), and are **two-way**:

- The star on a list or record adds or removes a favorite in ServiceNow. A list favorite keeps its filter, sort and view.
- Right-click a favorite to rename it, move it to a group (or a new group), or remove it. Right-click a group to rename or remove it; removing a group keeps its favorites. Drag a favorite onto a group to file it, or onto another favorite to reorder.
- Changes show immediately and are then confirmed against ServiceNow. If ServiceNow refuses a new favorite or saved filter (for example, no create access), it is kept on this Mac instead and marked as such; **Upload to ServiceNow** in the Favorites section retries local favorites.

Only same-instance classic list, existing-record, and new-record destinations are opened natively. Workspaces, portals, reports, scripts, and external destinations are omitted.

**What snrust writes.** Only rows owned by the signed-in user in `sys_ui_bookmark`, `sys_ui_bookmark_group` and `sys_filter`. It does not write application menus, list or form layouts, or any other UI configuration, and it never touches another user's rows.

These features require Table API read access to the metadata tables above, plus `sys_user_has_role` for role-aware menu filtering. ACL failures are reported; cached navigation and direct table browsing remain available. No additional admin roles or ACL changes are applied automatically.

### Settings

**⌘,** opens Settings: appearance (light, dark, or match macOS), UI scale from 80% to 150% (also **⌘+**, **⌘−**, **⌘0**), compact or comfortable density, rows per page, whether cache and network timings are shown, account connections, and the local cache. Settings are stored on this Mac and apply to every account.

### Copying values

List cells, form fields (including read-only/reference/choice controls), and audit metadata have **Copy** buttons. A normal click copies the displayed value; **Option-click** on list/form field Copy buttons copies the raw value, such as a reference sys_id or choice code. Editable text controls retain their normal text-copy behavior.

---

## Keyboard Shortcuts & Omnibox Commands

| Shortcut | Action |
|---|---|
| `⌘K` | **Command Palette / Omnibox** — type `incident.list`, `incident.filter`, `incident.do`, `INC0010001`, or fuzzy table names |
| `⌘S` | **Save** active Form (`PATCH` dirty fields & pending journal notes only; stays on form) |
| `⇧⌘S` | **Update & Close** active Form |
| `⇧⌘E` | Toggle **Customize Form Auto-Grid** (drag `⋮⋮` handles to reorder, toggle `1 col` / `2 col` spans, hide/add fields, add sections) |
| `⌘\` | Toggle **Side-by-Side Split View** (keep List View on the left and Live Form View on the right) |
| `Click` / `⌘Click` | Open a reference link in a tab / in the Split Pane (`⌘Click` also works on List rows) |
| `⌘R` | Force-refresh active List or Form from ServiceNow |
| `⌘B` | Toggle the Applications / Tables sidebar |
| `⌘N` | Create a new record in the active table |
| `⌘W` | Close active tab |
| `⌘,` | Open Settings |
| `⇧⌘,` | Open ServiceNow Instance Connections manager |
| `⌘+` / `⌘−` / `⌘0` | Zoom the UI in / out / back to 100% |
| `⌘↵` | Run the filter being built |

---

## Running Tests & Lints

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`crates/sn-app/src/ui_tests.rs` holds headless UI tests: they open the real workspace in a GPUI test window against the Demo Sandbox and drive it with simulated clicks, drags and keystrokes. They check behaviour and state, not pixels.
