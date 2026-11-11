# snrust — High-Speed macOS-Native ServiceNow Client

`snrust` is a **120fps Metal-accelerated macOS desktop client** for ServiceNow built in Rust on **`gpui-kit` (`gpui` + `gpui-component` `0.9`)** and **Tokio**. It eliminates browser DOM/bundle lag with a 3-tier local SQLite WAL + in-memory LRU cache, OAuth 2.0 PKCE authentication backed by macOS Keychain, classic List and Form views with an interactive **Rearrangeable 2-Column Auto-Grid**, and **real-time inbound field updates** over ServiceNow AMB WebSockets with automatic adaptive REST delta-polling fallback.

---

## Development Status (MVP)

| Area | Status | Notes |
|---|---|---|
| **OAuth 2.0 + PKCE & Keychain** | Complete | Loopback PKCE (`S256`), optional `client_secret`, macOS Keychain storage, single-flight auto-refresh, `401` retry, multi-instance switcher |
| **3-Tier Local Cache (`sn-cache`)** | Complete | Bundled SQLite (WAL + `mmap` + FTS5) + `moka` hot LRU; 0ms SWR List-to-Form hydration & partial-safe `SnRecord` merging |
| **Real-Time Engine (`sn-realtime`)** | Complete | ServiceNow `/amb` CometD/Bayeux Record Watcher primary + automatic 1.5s/10s REST watermark delta-polling fallback & journal tailing |
| **Classic List View** | Complete | 120fps virtualized `DataTable`, sortable/resizable/reorderable columns, visual filter chips + raw `sysparm_query`, column chooser |
| **Classic Form View & Auto-Grid** | Complete | Dictionary-driven typed controls, async cached Reference lookup + jump-to-record, drag-and-drop & `1-col`/`2-col` grid editor, Activity Stream, dirty-safe live conflict badges, explicit `⌘S` Save / `⇧⌘S` Update |
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
│   └── bundle-macos.sh         # Builds native dist/snrust.app (Retina + 120Hz ProMotion)
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
1. Launch `snrust` and click **Connect ServiceNow / Demo Sandbox** (or press `⌘,`).
2. Click **Launch Demo Sandbox** in the left panel.
3. Open any incident (`INC0010001`), edit a field (or leave it clean), and click **Simulate Live Push** in the form header to watch real-time inbound field updates, live highlight pulses, Activity Stream updates, and dirty-field conflict protection in action.

---

## Connecting a Real ServiceNow Instance (OAuth 2.0 + PKCE)

1. In your ServiceNow instance, navigate to **System OAuth → Application Registry** (`oauth_entity.list`).
2. Click **New → Create an OAuth API endpoint for external clients**:
   - **Name**: `snrust Desktop`
   - **Redirect URL**: `http://127.0.0.1:8910/oauth/callback` *(one-click copyable inside `snrust`'s Connect dialog; port is configurable per instance)*
   - **PKCE / Public Client**: Either enable *Public Client* (no secret needed) or copy the generated *Client Secret* (`snrust` supports both and stores secrets exclusively in macOS Keychain).
   - **Accessible from**: All application scopes.
3. In `snrust`, press `⌘,`, enter your **Instance Name/URL** (e.g. `dev12345` or `https://acme.service-now.com`) and **Client ID** (plus optional **Client Secret**), and click **Sign in with OAuth 2.0 + PKCE**.
4. Authenticate in your default macOS browser (SSO / Okta / Entra MFA supported); `snrust` captures the loopback callback automatically and stores tokens in **macOS Keychain** (`dev.snrust.oauth`).

---

## Keyboard Shortcuts & Omnibox Commands

| Shortcut | Action |
|---|---|
| `⌘K` | **Command Palette / Omnibox** — type `incident.list`, `incident.filter`, `incident.do`, `INC0010001`, or fuzzy table names |
| `⌘S` | **Save** active Form (`PATCH` dirty fields & pending journal notes only; stays on form) |
| `⇧⌘S` | **Update & Close** active Form |
| `⇧⌘E` | Toggle **Customize Form Auto-Grid** (drag `⋮⋮` handles to reorder, toggle `1 col` / `2 col` spans, hide/add fields, add sections) |
| `⌘\` | Toggle **Side-by-Side Split View** (keep List View on the left and Live Form View on the right) |
| `⌘Click` | Open any List row or Reference link directly in the Split Pane |
| `⌘R` | Force-refresh active List or Form from ServiceNow |
| `⌘B` | Toggle collapsible Table Navigator Sidebar |
| `⌘N` | Create a new record in the active table |
| `⌘W` | Close active tab |
| `⌘,` | Open ServiceNow Instance Connections manager |

---

## Running Tests & Lints

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
