//! ServiceNow `/amb` (Asynchronous Message Bus) CometD / Bayeux WebSocket protocol client.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sn_api::SnClient;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, warn};

/// Channel encoding convention used by ServiceNow AMB Record Watcher (`/rw/default/...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbChannelEncoding {
    /// ServiceNow replaces trailing `=` padding with `-` and `/` with `_` in Bayeux channel segments.
    ServiceNowEscaped,
}

/// Encodes a table + encoded query filter into a ServiceNow Record Watcher Bayeux channel.
///
/// Example: `encode_record_watcher_channel("incident", "sys_id=abc")` -> `"/rw/default/incident/c3lzX2lkPWFiYw--"`.
pub fn encode_record_watcher_channel(table: &str, filter: &str) -> String {
    let b64 = BASE64_STD
        .encode(filter.as_bytes())
        .replace('=', "-")
        .replace('/', "_");
    format!("/rw/default/{table}/{b64}")
}

#[derive(Debug, Clone)]
pub enum AmbCommand {
    Subscribe { channel: String },
    Unsubscribe { channel: String },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmbNotification {
    Connected,
    RecordWatcherAction {
        channel: String,
        table: String,
        sys_id: String,
        action: String,
    },
    Disconnected {
        reason: String,
        unsupported_or_unauthorized: bool,
    },
}

#[derive(Debug, Deserialize)]
struct BayeuxFrame {
    channel: String,
    #[serde(default)]
    successful: Option<bool>,
    #[serde(default, rename = "clientId")]
    client_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WatcherDataPayload {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    sys_id: Option<String>,
    #[serde(default)]
    table_name: Option<String>,
}

/// Connects to `wss://<instance>/amb`, negotiates CometD `/meta/handshake` + `/meta/connect`,
/// manages `/rw/default/...` subscriptions, and forwards notifications until closed or rejected.
pub async fn run_amb_session(
    client: SnClient,
    initial_channels: Vec<String>,
    mut cmd_rx: mpsc::Receiver<AmbCommand>,
    event_tx: mpsc::Sender<AmbNotification>,
) {
    let ws_url = match client.profile().amb_ws_url() {
        Ok(u) => u,
        Err(e) => {
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: e.to_string(),
                    unsupported_or_unauthorized: true,
                })
                .await;
            return;
        }
    };

    let (token, cookies) = match client.bearer_token_and_cookies().await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: e.to_string(),
                    unsupported_or_unauthorized: false,
                })
                .await;
            return;
        }
    };

    let mut req = match ws_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: format!("Invalid AMB WebSocket request: {e}"),
                    unsupported_or_unauthorized: true,
                })
                .await;
            return;
        }
    };

    if let Ok(auth_val) = HeaderValue::from_str(&format!("Bearer {token}")) {
        req.headers_mut().insert("Authorization", auth_val);
    }
    if let Some(cookie_str) = cookies
        && let Ok(cookie_val) = HeaderValue::from_str(&cookie_str)
    {
        req.headers_mut().insert("Cookie", cookie_val);
    }
    if let Ok(origin_val) = HeaderValue::from_str(&client.profile().base_url) {
        req.headers_mut().insert("Origin", origin_val);
    }

    let connect_res = tokio::time::timeout(Duration::from_secs(10), connect_async(req)).await;
    let (ws_stream, _) = match connect_res {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => {
            let msg = err.to_string();
            let auth_rejected = msg.contains("401") || msg.contains("403") || msg.contains("404");
            debug!("AMB WebSocket connect declined ({msg}), falling back to REST delta polling");
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: msg,
                    unsupported_or_unauthorized: auth_rejected,
                })
                .await;
            return;
        }
        Err(_) => {
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: "AMB WebSocket handshake timed out".into(),
                    unsupported_or_unauthorized: false,
                })
                .await;
            return;
        }
    };

    let (mut sink, mut stream) = ws_stream.split();
    let mut next_msg_id: u64 = 1;

    let handshake = json!([{
        "id": next_msg_id.to_string(),
        "version": "1.0",
        "minimumVersion": "1.0",
        "channel": "/meta/handshake",
        "supportedConnectionTypes": ["websocket", "long-polling"]
    }]);
    next_msg_id += 1;

    if let Err(e) = sink.send(Message::Text(handshake.to_string().into())).await {
        let _ = event_tx
            .send(AmbNotification::Disconnected {
                reason: format!("Failed sending AMB handshake: {e}"),
                unsupported_or_unauthorized: false,
            })
            .await;
        return;
    }

    // Wait for `/meta/handshake` reply.
    let client_id = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(txt)))) => match parse_handshake_client_id(&txt) {
            Ok(cid) => cid,
            Err(reason) => {
                let _ = event_tx
                    .send(AmbNotification::Disconnected {
                        reason,
                        unsupported_or_unauthorized: true,
                    })
                    .await;
                return;
            }
        },
        other => {
            let _ = event_tx
                .send(AmbNotification::Disconnected {
                    reason: format!("Unexpected AMB handshake response: {other:?}"),
                    unsupported_or_unauthorized: true,
                })
                .await;
            return;
        }
    };

    let _ = event_tx.send(AmbNotification::Connected).await;

    // Subscribe initial channels and send initial `/meta/connect`.
    let mut channel_tables: HashMap<String, String> = HashMap::new();
    for ch in initial_channels {
        if let Some(tbl) = extract_table_from_rw_channel(&ch) {
            channel_tables.insert(ch.clone(), tbl);
        }
        let sub_frame = build_subscribe_frame(&mut next_msg_id, &client_id, &ch, true);
        let _ = sink.send(Message::Text(sub_frame.into())).await;
    }

    let connect_frame = build_connect_frame(&mut next_msg_id, &client_id);
    let _ = sink.send(Message::Text(connect_frame.into())).await;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let frame = build_connect_frame(&mut next_msg_id, &client_id);
                if let Err(e) = sink.send(Message::Text(frame.into())).await {
                    let _ = event_tx.send(AmbNotification::Disconnected {
                        reason: format!("AMB heartbeat failed: {e}"),
                        unsupported_or_unauthorized: false,
                    }).await;
                    break;
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(AmbCommand::Subscribe { channel }) => {
                        if let Some(tbl) = extract_table_from_rw_channel(&channel) {
                            channel_tables.insert(channel.clone(), tbl);
                        }
                        let frame = build_subscribe_frame(&mut next_msg_id, &client_id, &channel, true);
                        let _ = sink.send(Message::Text(frame.into())).await;
                    }
                    Some(AmbCommand::Unsubscribe { channel }) => {
                        channel_tables.remove(&channel);
                        let frame = build_subscribe_frame(&mut next_msg_id, &client_id, &channel, false);
                        let _ = sink.send(Message::Text(frame.into())).await;
                    }
                    Some(AmbCommand::Shutdown) | None => {
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            maybe_ws = stream.next() => {
                match maybe_ws {
                    Some(Ok(Message::Text(txt))) => {
                        for notif in parse_bayeux_messages(&txt, &channel_tables) {
                            let _ = event_tx.send(notif).await;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = event_tx.send(AmbNotification::Disconnected {
                            reason: "AMB WebSocket closed by server".into(),
                            unsupported_or_unauthorized: false,
                        }).await;
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("AMB stream error: {e}");
                        let _ = event_tx.send(AmbNotification::Disconnected {
                            reason: e.to_string(),
                            unsupported_or_unauthorized: false,
                        }).await;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn parse_handshake_client_id(raw: &str) -> Result<String, String> {
    let frames: Vec<BayeuxFrame> =
        serde_json::from_str(raw).map_err(|e| format!("Invalid Bayeux handshake JSON: {e}"))?;
    for f in frames {
        if f.channel == "/meta/handshake" {
            if f.successful == Some(true)
                && let Some(cid) = f.client_id
                && !cid.is_empty()
            {
                return Ok(cid);
            }
            return Err(f
                .error
                .unwrap_or_else(|| "Bayeux /meta/handshake unsuccessful".to_string()));
        }
    }
    Err("Missing /meta/handshake frame".to_string())
}

fn build_connect_frame(next_id: &mut u64, client_id: &str) -> String {
    let id = *next_id;
    *next_id += 1;
    json!([{
        "id": id.to_string(),
        "channel": "/meta/connect",
        "clientId": client_id,
        "connectionType": "websocket"
    }])
    .to_string()
}

fn build_subscribe_frame(
    next_id: &mut u64,
    client_id: &str,
    sub_channel: &str,
    subscribe: bool,
) -> String {
    let id = *next_id;
    *next_id += 1;
    let meta = if subscribe {
        "/meta/subscribe"
    } else {
        "/meta/unsubscribe"
    };
    json!([{
        "id": id.to_string(),
        "channel": meta,
        "clientId": client_id,
        "subscription": sub_channel
    }])
    .to_string()
}

fn extract_table_from_rw_channel(channel: &str) -> Option<String> {
    let rest = channel.strip_prefix("/rw/default/")?;
    let (table, _) = rest.split_once('/')?;
    (!table.is_empty()).then(|| table.to_string())
}

fn parse_bayeux_messages(
    raw: &str,
    channel_tables: &HashMap<String, String>,
) -> Vec<AmbNotification> {
    let Ok(frames) = serde_json::from_str::<Vec<BayeuxFrame>>(raw) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for frame in frames {
        if frame.channel.starts_with("/meta/") {
            continue;
        }
        if let Some(data_val) = frame.data {
            // ServiceNow AMB Record Watcher nests record metadata either at `data` or `data.data`.
            let inner = data_val.get("data").cloned().unwrap_or(data_val);
            if let Ok(parsed) = serde_json::from_value::<WatcherDataPayload>(inner)
                && let Some(sys_id) = parsed.sys_id.filter(|s| !s.is_empty())
            {
                let table = parsed
                    .table_name
                    .filter(|s| !s.is_empty())
                    .or_else(|| channel_tables.get(&frame.channel).cloned())
                    .or_else(|| extract_table_from_rw_channel(&frame.channel))
                    .unwrap_or_default();
                let action = parsed.action.unwrap_or_else(|| "update".to_string());
                out.push(AmbNotification::RecordWatcherAction {
                    channel: frame.channel,
                    table,
                    sys_id,
                    action,
                });
            }
        }
    }
    out
}
