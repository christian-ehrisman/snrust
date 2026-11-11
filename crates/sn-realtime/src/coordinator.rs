//! `RealtimeCoordinator` orchestrating AMB WebSocket push and adaptive REST delta-polling.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use sn_api::{ListRecordsParams, SnClient};
use sn_cache::SnCache;
use sn_core::{EncodedQuery, InstanceId, JournalEntry, SnRecord};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::amb::{AmbCommand, AmbNotification, encode_record_watcher_channel, run_amb_session};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeTransportMode {
    Connecting,
    AmbWebSocket,
    RestDeltaPolling,
    Offline { reason: String },
}

impl RealtimeTransportMode {
    pub fn badge_label(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting…",
            Self::AmbWebSocket => "Live · AMB",
            Self::RestDeltaPolling => "Live · Delta",
            Self::Offline { .. } => "Cached · Offline",
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::AmbWebSocket | Self::RestDeltaPolling)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    AmbPush,
    DeltaPoll,
}

#[derive(Debug, Clone)]
pub enum WatchTarget {
    Record {
        table: String,
        sys_id: String,
        last_mod_count: u64,
        last_updated_on: Option<String>,
        last_journal_ts: Option<String>,
    },
    List {
        table: String,
        encoded_query: String,
        fields: Vec<String>,
        high_water_updated_on: Option<String>,
    },
}

impl WatchTarget {
    pub fn amb_channel(&self) -> String {
        match self {
            Self::Record { table, sys_id, .. } => {
                encode_record_watcher_channel(table, &format!("sys_id={sys_id}"))
            }
            Self::List {
                table,
                encoded_query,
                ..
            } => encode_record_watcher_channel(table, encoded_query),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RealtimeEvent {
    TransportChanged {
        instance_id: InstanceId,
        mode: RealtimeTransportMode,
        latency_ms: Option<u64>,
    },
    RecordUpdated {
        instance_id: InstanceId,
        table: String,
        sys_id: String,
        record: SnRecord,
        source: UpdateSource,
    },
    JournalsAdded {
        instance_id: InstanceId,
        table: String,
        sys_id: String,
        entries: Vec<JournalEntry>,
    },
    ListDelta {
        instance_id: InstanceId,
        table: String,
        encoded_query: String,
        updated_records: Vec<SnRecord>,
    },
    RecordDeleted {
        instance_id: InstanceId,
        table: String,
        sys_id: String,
    },
}

/// Tuning knobs for AMB WebSocket and adaptive REST delta-polling cadences.
#[derive(Debug, Clone)]
pub struct RealtimeConfig {
    pub enable_amb: bool,
    pub active_record_poll_interval: Duration,
    pub unfocused_record_poll_interval: Duration,
    pub list_poll_interval: Duration,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            enable_amb: true,
            active_record_poll_interval: Duration::from_millis(1500),
            unfocused_record_poll_interval: Duration::from_secs(10),
            list_poll_interval: Duration::from_secs(5),
        }
    }
}

struct InstanceWorkerHandle {
    subscriptions: Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>>,
    amb_cmd_tx: Arc<Mutex<Option<mpsc::Sender<AmbCommand>>>>,
    wake_notify: Arc<Notify>,
    task: JoinHandle<()>,
}

struct CoordinatorInner {
    tokio_handle: Handle,
    cache: SnCache,
    config: RealtimeConfig,
    app_focused: AtomicBool,
    next_sub_id: AtomicU64,
    subscribers: Mutex<Vec<async_channel::Sender<RealtimeEvent>>>,
    transports: RwLock<HashMap<InstanceId, (RealtimeTransportMode, Option<u64>)>>,
    workers: Mutex<HashMap<InstanceId, InstanceWorkerHandle>>,
}

/// Drop guard that automatically unsubscribes a watched Form or List when its tab closes.
pub struct SubscriptionGuard {
    coordinator: RealtimeCoordinator,
    instance_id: InstanceId,
    sub_id: SubscriptionId,
}

impl SubscriptionGuard {
    pub fn id(&self) -> SubscriptionId {
        self.sub_id
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.coordinator.unsubscribe(&self.instance_id, self.sub_id);
    }
}

/// Thread-safe coordinator managing real-time subscriptions across one or more ServiceNow instances.
#[derive(Clone)]
pub struct RealtimeCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl RealtimeCoordinator {
    pub fn new(tokio_handle: Handle, cache: SnCache, config: RealtimeConfig) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                tokio_handle,
                cache,
                config,
                app_focused: AtomicBool::new(true),
                next_sub_id: AtomicU64::new(1),
                subscribers: Mutex::new(Vec::new()),
                transports: RwLock::new(HashMap::new()),
                workers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Updates window focus state so REST delta-polling immediately speeds up to 1.5s on focus
    /// and relaxes to 10s when the macOS app is in the background.
    pub fn set_app_focused(&self, focused: bool) {
        let prev = self.inner.app_focused.swap(focused, Ordering::SeqCst);
        if !prev && focused {
            let workers = self.inner.workers.lock();
            for w in workers.values() {
                w.wake_notify.notify_one();
            }
        }
    }

    pub fn transport_status(
        &self,
        instance_id: &InstanceId,
    ) -> (RealtimeTransportMode, Option<u64>) {
        self.inner
            .transports
            .read()
            .get(instance_id)
            .cloned()
            .unwrap_or((RealtimeTransportMode::Connecting, None))
    }

    /// Subscribes an async receiver (`async_channel::Receiver<RealtimeEvent>`) for UI entities.
    pub fn subscribe_events(&self) -> async_channel::Receiver<RealtimeEvent> {
        let (tx, rx) = async_channel::bounded(256);
        self.inner.subscribers.lock().push(tx);
        rx
    }

    /// Ensures a background worker is running for `client` and registers `target`,
    /// returning an RAII [`SubscriptionGuard`] that cleans up on drop.
    pub fn watch(&self, client: SnClient, target: WatchTarget) -> SubscriptionGuard {
        let instance_id = client.profile().id.clone();
        let sub_id = SubscriptionId(self.inner.next_sub_id.fetch_add(1, Ordering::Relaxed));
        let channel = target.amb_channel();

        let mut workers = self.inner.workers.lock();
        let worker = workers
            .entry(instance_id.clone())
            .or_insert_with(|| self.spawn_instance_worker(client.clone()));

        worker.subscriptions.write().insert(sub_id, target);
        if let Some(tx) = worker.amb_cmd_tx.lock().as_ref() {
            let _ = tx.try_send(AmbCommand::Subscribe { channel });
        }
        worker.wake_notify.notify_one();
        drop(workers);

        SubscriptionGuard {
            coordinator: self.clone(),
            instance_id,
            sub_id,
        }
    }

    /// Updates watermark metadata on an active record watch (for example after an explicit `Cmd+S` save)
    /// so our own write isn't re-fetched as an external server change.
    pub fn advance_record_watermark(
        &self,
        instance_id: &InstanceId,
        sub_id: SubscriptionId,
        mod_count: u64,
        updated_on: Option<String>,
        latest_journal_ts: Option<String>,
    ) {
        let workers = self.inner.workers.lock();
        if let Some(worker) = workers.get(instance_id) {
            let mut subs = worker.subscriptions.write();
            if let Some(WatchTarget::Record {
                last_mod_count,
                last_updated_on,
                last_journal_ts,
                ..
            }) = subs.get_mut(&sub_id)
            {
                if mod_count >= *last_mod_count {
                    *last_mod_count = mod_count;
                }
                if updated_on.is_some() {
                    *last_updated_on = updated_on;
                }
                if latest_journal_ts.is_some() {
                    *last_journal_ts = latest_journal_ts;
                }
            }
        }
    }

    pub fn unsubscribe(&self, instance_id: &InstanceId, sub_id: SubscriptionId) {
        let workers = self.inner.workers.lock();
        if let Some(worker) = workers.get(instance_id) {
            let removed = worker.subscriptions.write().remove(&sub_id);
            if let Some(target) = removed {
                let ch = target.amb_channel();
                let still_used = worker
                    .subscriptions
                    .read()
                    .values()
                    .any(|t| t.amb_channel() == ch);
                if !still_used && let Some(tx) = worker.amb_cmd_tx.lock().as_ref() {
                    let _ = tx.try_send(AmbCommand::Unsubscribe { channel: ch });
                }
            }
        }
    }

    pub fn shutdown_instance(&self, instance_id: &InstanceId) {
        if let Some(worker) = self.inner.workers.lock().remove(instance_id) {
            if let Some(tx) = worker.amb_cmd_tx.lock().take() {
                let _ = tx.try_send(AmbCommand::Shutdown);
            }
            worker.task.abort();
        }
        self.inner.transports.write().remove(instance_id);
    }

    fn emit(&self, event: RealtimeEvent) {
        if let RealtimeEvent::TransportChanged {
            ref instance_id,
            ref mode,
            latency_ms,
        } = event
        {
            self.inner
                .transports
                .write()
                .insert(instance_id.clone(), (mode.clone(), latency_ms));
        }
        let mut subs = self.inner.subscribers.lock();
        subs.retain(|tx| tx.try_send(event.clone()).is_ok() || !tx.is_closed());
    }

    fn spawn_instance_worker(&self, client: SnClient) -> InstanceWorkerHandle {
        let subscriptions: Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let amb_cmd_tx: Arc<Mutex<Option<mpsc::Sender<AmbCommand>>>> = Arc::new(Mutex::new(None));
        let wake_notify = Arc::new(Notify::new());

        let coord = self.clone();
        let subs_ref = Arc::clone(&subscriptions);
        let amb_tx_slot = Arc::clone(&amb_cmd_tx);
        let wake_ref = Arc::clone(&wake_notify);

        let task = self.inner.tokio_handle.spawn(async move {
            coord
                .run_worker_loop(client, subs_ref, amb_tx_slot, wake_ref)
                .await;
        });

        InstanceWorkerHandle {
            subscriptions,
            amb_cmd_tx,
            wake_notify,
            task,
        }
    }

    async fn run_worker_loop(
        &self,
        client: SnClient,
        subscriptions: Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>>,
        amb_tx_slot: Arc<Mutex<Option<mpsc::Sender<AmbCommand>>>>,
        wake_notify: Arc<Notify>,
    ) {
        let instance_id = client.profile().id.clone();
        self.emit(RealtimeEvent::TransportChanged {
            instance_id: instance_id.clone(),
            mode: RealtimeTransportMode::Connecting,
            latency_ms: None,
        });

        let mut amb_permanently_disabled = !self.inner.config.enable_amb;
        let mut amb_event_rx: Option<mpsc::Receiver<AmbNotification>> = None;
        let mut last_amb_attempt: Option<Instant> = None;
        let mut last_list_poll = Instant::now()
            .checked_sub(self.inner.config.list_poll_interval)
            .unwrap_or_else(Instant::now);
        let mut tick_counter: u64 = 0;

        loop {
            let has_subs = !subscriptions.read().is_empty();
            if !has_subs {
                tokio::select! {
                    _ = wake_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                continue;
            }

            // 1. Attempt AMB WebSocket connection if enabled and not currently active.
            if !amb_permanently_disabled && amb_event_rx.is_none() {
                let should_try = last_amb_attempt
                    .map(|t| t.elapsed() >= Duration::from_secs(30))
                    .unwrap_or(true);
                if should_try {
                    last_amb_attempt = Some(Instant::now());
                    let initial_channels: Vec<String> = subscriptions
                        .read()
                        .values()
                        .map(WatchTarget::amb_channel)
                        .collect();
                    let (cmd_tx, cmd_rx) = mpsc::channel(64);
                    let (evt_tx, evt_rx) = mpsc::channel(64);
                    *amb_tx_slot.lock() = Some(cmd_tx);
                    amb_event_rx = Some(evt_rx);

                    let amb_client = client.clone();
                    tokio::spawn(async move {
                        run_amb_session(amb_client, initial_channels, cmd_rx, evt_tx).await;
                    });
                }
            }

            let focused = self.inner.app_focused.load(Ordering::Relaxed);
            let record_interval = if focused {
                self.inner.config.active_record_poll_interval
            } else {
                self.inner.config.unfocused_record_poll_interval
            };

            // When AMB WebSocket is live, relax background safety reconciliation to 15s;
            // when in REST Delta Polling mode, poll at the crisp 1.5s active cadence!
            let current_mode = self.transport_status(&instance_id).0;
            let sleep_dur = if current_mode == RealtimeTransportMode::AmbWebSocket {
                Duration::from_secs(15)
            } else {
                record_interval
            };

            tokio::select! {
                _ = wake_notify.notified() => {}
                _ = tokio::time::sleep(sleep_dur) => {
                    tick_counter = tick_counter.wrapping_add(1);
                    self.poll_watched_targets(
                        &client,
                        &subscriptions,
                        &mut last_list_poll,
                        tick_counter,
                        current_mode == RealtimeTransportMode::AmbWebSocket,
                    ).await;
                }
                Some(amb_notif) = async {
                    match amb_event_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match amb_notif {
                        AmbNotification::Connected => {
                            debug!("AMB WebSocket active for instance {}", instance_id);
                            self.emit(RealtimeEvent::TransportChanged {
                                instance_id: instance_id.clone(),
                                mode: RealtimeTransportMode::AmbWebSocket,
                                latency_ms: Some(12),
                            });
                        }
                        AmbNotification::RecordWatcherAction { table, sys_id, action, .. } => {
                            if action == "delete" {
                                self.emit(RealtimeEvent::RecordDeleted {
                                    instance_id: instance_id.clone(),
                                    table,
                                    sys_id,
                                });
                            } else if !table.is_empty() {
                                self.hydrate_and_emit_record(
                                    &client,
                                    &subscriptions,
                                    &table,
                                    &sys_id,
                                    UpdateSource::AmbPush,
                                ).await;
                            }
                        }
                        AmbNotification::Disconnected { reason, unsupported_or_unauthorized } => {
                            *amb_tx_slot.lock() = None;
                            amb_event_rx = None;
                            if unsupported_or_unauthorized {
                                amb_permanently_disabled = true;
                            }
                            debug!("Switching {} to adaptive REST delta-polling ({reason})", instance_id);
                            self.emit(RealtimeEvent::TransportChanged {
                                instance_id: instance_id.clone(),
                                mode: RealtimeTransportMode::RestDeltaPolling,
                                latency_ms: None,
                            });
                            // Immediately perform a delta poll so there is zero gap after fallback.
                            self.poll_watched_targets(
                                &client,
                                &subscriptions,
                                &mut last_list_poll,
                                tick_counter,
                                false,
                            ).await;
                        }
                    }
                }
            }
        }
    }

    async fn poll_watched_targets(
        &self,
        client: &SnClient,
        subscriptions: &Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>>,
        last_list_poll: &mut Instant,
        tick_counter: u64,
        amb_active: bool,
    ) {
        let instance_id = client.profile().id.clone();
        let snapshot: Vec<(SubscriptionId, WatchTarget)> = subscriptions
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        let should_poll_lists =
            !amb_active && last_list_poll.elapsed() >= self.inner.config.list_poll_interval;
        if should_poll_lists {
            *last_list_poll = Instant::now();
        }

        for (sub_id, target) in snapshot {
            match target {
                WatchTarget::Record {
                    table,
                    sys_id,
                    last_mod_count,
                    last_updated_on,
                    last_journal_ts,
                } => {
                    let started = Instant::now();
                    match client.get_record_watermark(&table, &sys_id).await {
                        Ok(wm) => {
                            let rtt_ms = started.elapsed().as_millis() as u64;
                            if !amb_active {
                                self.emit(RealtimeEvent::TransportChanged {
                                    instance_id: instance_id.clone(),
                                    mode: RealtimeTransportMode::RestDeltaPolling,
                                    latency_ms: Some(rtt_ms),
                                });
                            }

                            let changed = wm.sys_mod_count > last_mod_count
                                || (last_updated_on.is_some()
                                    && last_updated_on.as_deref()
                                        != Some(wm.sys_updated_on.as_str()));

                            if changed {
                                self.hydrate_and_emit_record(
                                    client,
                                    subscriptions,
                                    &table,
                                    &sys_id,
                                    UpdateSource::DeltaPoll,
                                )
                                .await;
                            } else if tick_counter.is_multiple_of(3) {
                                // Check journal entries periodically even if mod_count was unchanged.
                                self.poll_journals_delta(
                                    client,
                                    subscriptions,
                                    sub_id,
                                    &table,
                                    &sys_id,
                                    last_journal_ts.as_deref(),
                                )
                                .await;
                            }
                        }
                        Err(err) => {
                            warn!("Watermark poll error on {table}/{sys_id}: {err}");
                            self.emit(RealtimeEvent::TransportChanged {
                                instance_id: instance_id.clone(),
                                mode: RealtimeTransportMode::Offline {
                                    reason: err.to_string(),
                                },
                                latency_ms: None,
                            });
                        }
                    }
                }
                WatchTarget::List {
                    table,
                    encoded_query,
                    fields,
                    high_water_updated_on,
                } => {
                    if !should_poll_lists {
                        continue;
                    }
                    let Some(watermark) = high_water_updated_on else {
                        continue;
                    };
                    let delta_query =
                        EncodedQuery::parse(&encoded_query).with_updated_after(&watermark);
                    let params = ListRecordsParams {
                        encoded_query: delta_query,
                        fields,
                        offset: 0,
                        limit: 50,
                        include_total_count: false,
                    };
                    if let Ok(page) = client.list_records(&table, &params).await
                        && !page.records.is_empty()
                    {
                        let _ = self
                            .inner
                            .cache
                            .apply_list_delta_async(
                                instance_id.clone(),
                                table.clone(),
                                page.records.clone(),
                            )
                            .await;

                        if let Some(new_max) = page.max_updated_on
                            && let Some(WatchTarget::List {
                                high_water_updated_on: hw,
                                ..
                            }) = subscriptions.write().get_mut(&sub_id)
                        {
                            *hw = Some(new_max);
                        }

                        self.emit(RealtimeEvent::ListDelta {
                            instance_id: instance_id.clone(),
                            table,
                            encoded_query,
                            updated_records: page.records,
                        });
                    }
                }
            }
        }
    }

    async fn hydrate_and_emit_record(
        &self,
        client: &SnClient,
        subscriptions: &Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>>,
        table: &str,
        sys_id: &str,
        source: UpdateSource,
    ) {
        let instance_id = client.profile().id.clone();

        // Find current journal watermark if subscribed.
        let prev_journal_ts = subscriptions.read().values().find_map(|t| match t {
            WatchTarget::Record {
                table: t_tbl,
                sys_id: t_id,
                last_journal_ts,
                ..
            } if t_tbl == table && t_id == sys_id => last_journal_ts.clone(),
            _ => None,
        });

        let (rec_res, jrn_res) = tokio::join!(
            client.get_record(table, sys_id),
            client.get_journal_entries(sys_id, prev_journal_ts.as_deref(), 50)
        );

        if let Ok(record) = rec_res {
            let cache = self.inner.cache.clone();
            let inst_clone = instance_id.clone();
            let rec_clone = record.clone();
            let _ = cache
                .run_blocking(move |c| c.upsert_record(&inst_clone, &rec_clone))
                .await;

            // Advance matching watch watermarks so subsequent polls don't re-trigger.
            {
                let mut subs = subscriptions.write();
                for target in subs.values_mut() {
                    if let WatchTarget::Record {
                        table: t_tbl,
                        sys_id: t_id,
                        last_mod_count,
                        last_updated_on,
                        ..
                    } = target
                        && t_tbl == table
                        && t_id == sys_id
                    {
                        *last_mod_count = record.sys_mod_count;
                        *last_updated_on = record.sys_updated_on.clone();
                    }
                }
            }

            self.emit(RealtimeEvent::RecordUpdated {
                instance_id: instance_id.clone(),
                table: table.to_string(),
                sys_id: sys_id.to_string(),
                record,
                source,
            });
        }

        if let Ok(entries) = jrn_res
            && !entries.is_empty()
        {
            let _ = self
                .inner
                .cache
                .save_journals_async(
                    instance_id.clone(),
                    table.to_string(),
                    sys_id.to_string(),
                    entries.clone(),
                )
                .await;

            if let Some(newest_ts) = entries.iter().map(|e| e.sys_created_on.clone()).max() {
                let mut subs = subscriptions.write();
                for target in subs.values_mut() {
                    if let WatchTarget::Record {
                        table: t_tbl,
                        sys_id: t_id,
                        last_journal_ts,
                        ..
                    } = target
                        && t_tbl == table
                        && t_id == sys_id
                    {
                        *last_journal_ts = Some(newest_ts.clone());
                    }
                }
            }

            self.emit(RealtimeEvent::JournalsAdded {
                instance_id,
                table: table.to_string(),
                sys_id: sys_id.to_string(),
                entries,
            });
        }
    }

    async fn poll_journals_delta(
        &self,
        client: &SnClient,
        subscriptions: &Arc<RwLock<HashMap<SubscriptionId, WatchTarget>>>,
        sub_id: SubscriptionId,
        table: &str,
        sys_id: &str,
        created_after: Option<&str>,
    ) {
        let instance_id = client.profile().id.clone();
        if let Ok(entries) = client.get_journal_entries(sys_id, created_after, 30).await
            && !entries.is_empty()
        {
            let _ = self
                .inner
                .cache
                .save_journals_async(
                    instance_id.clone(),
                    table.to_string(),
                    sys_id.to_string(),
                    entries.clone(),
                )
                .await;

            if let Some(newest_ts) = entries.iter().map(|e| e.sys_created_on.clone()).max()
                && let Some(WatchTarget::Record {
                    last_journal_ts, ..
                }) = subscriptions.write().get_mut(&sub_id)
            {
                *last_journal_ts = Some(newest_ts);
            }

            self.emit(RealtimeEvent::JournalsAdded {
                instance_id,
                table: table.to_string(),
                sys_id: sys_id.to_string(),
                entries,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sn_api::{MemoryCredentialStore, StoredCredentials};
    use sn_core::InstanceProfile;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn amb_401_falls_back_to_rest_delta_polling_and_streams_record_update() {
        let mock_server = MockServer::start().await;

        // 1. `/amb` returns 401 (strict OAuth-only ServiceNow instance without UI cookie).
        Mock::given(method("GET"))
            .and(path("/amb"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        // 2. Table API returns updated record (mod_count = 2 -> triggers RecordUpdated!).
        Mock::given(method("GET"))
            .and(path("/api/now/table/incident/inc_sys_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": {
                    "sys_id": { "value": "inc_sys_1", "display_value": "inc_sys_1" },
                    "number": { "value": "INC0010001", "display_value": "INC0010001" },
                    "state": { "value": "2", "display_value": "In Progress" },
                    "short_description": { "value": "Live server update!", "display_value": "Live server update!" },
                    "sys_mod_count": { "value": "2", "display_value": "2" },
                    "sys_updated_on": { "value": "2026-11-11 15:00:00", "display_value": "2026-11-11 10:00:00" },
                    "sys_updated_by": { "value": "admin", "display_value": "System Administrator" }
                }
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/now/table/sys_journal_field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [{
                    "sys_id": { "value": "jrn_1", "display_value": "jrn_1" },
                    "element": { "value": "work_notes", "display_value": "work_notes" },
                    "element_id": { "value": "inc_sys_1", "display_value": "inc_sys_1" },
                    "value": { "value": "Automated triage completed", "display_value": "Automated triage completed" },
                    "sys_created_on": { "value": "2026-11-11 15:00:01", "display_value": "2026-11-11 15:00:01" },
                    "sys_created_by": { "value": "flow.engine", "display_value": "flow.engine" }
                }]
            })))
            .mount(&mock_server)
            .await;

        let profile = InstanceProfile::new("Test", &mock_server.uri(), "cid", false).unwrap();
        let creds = StoredCredentials {
            access_token: "tok_123".into(),
            refresh_token: Some("ref_123".into()),
            expires_at: None,
            client_secret: None,
        };
        let cred_store = Arc::new(MemoryCredentialStore::with_credentials(
            profile.id.clone(),
            creds,
        ));
        let client = SnClient::new(profile.clone(), cred_store).unwrap();
        let cache = SnCache::open_in_memory().unwrap();

        let coordinator = RealtimeCoordinator::new(
            Handle::current(),
            cache.clone(),
            RealtimeConfig {
                enable_amb: true,
                active_record_poll_interval: Duration::from_millis(120),
                unfocused_record_poll_interval: Duration::from_secs(2),
                list_poll_interval: Duration::from_millis(250),
            },
        );

        let rx = coordinator.subscribe_events();
        let _guard = coordinator.watch(
            client,
            WatchTarget::Record {
                table: "incident".into(),
                sys_id: "inc_sys_1".into(),
                last_mod_count: 1,
                last_updated_on: Some("2026-11-11 14:55:00".into()),
                last_journal_ts: None,
            },
        );

        let mut saw_fallback_mode = false;
        let mut saw_record_update = false;
        let mut saw_journal = false;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while tokio::time::Instant::now() < deadline
            && !(saw_fallback_mode && saw_record_update && saw_journal)
        {
            if let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                match evt {
                    RealtimeEvent::TransportChanged {
                        mode: RealtimeTransportMode::RestDeltaPolling,
                        ..
                    } => {
                        saw_fallback_mode = true;
                    }
                    RealtimeEvent::RecordUpdated { record, .. } => {
                        assert_eq!(record.get_display("state"), "In Progress");
                        assert_eq!(
                            record.get_display("short_description"),
                            "Live server update!"
                        );
                        saw_record_update = true;
                    }
                    RealtimeEvent::JournalsAdded { entries, .. } => {
                        assert_eq!(entries[0].value, "Automated triage completed");
                        saw_journal = true;
                    }
                    _ => {}
                }
            }
        }

        assert!(
            saw_fallback_mode,
            "Expected automatic fallback to RestDeltaPolling"
        );
        assert!(saw_record_update, "Expected live RecordUpdated event");
        assert!(saw_journal, "Expected live JournalsAdded event");

        // Verify SnCache was also updated automatically!
        let cached = cache
            .get_record(&profile.id, "incident", "inc_sys_1")
            .unwrap()
            .expect("record cached");
        assert_eq!(cached.data.get_display("state"), "In Progress");
    }

    #[tokio::test]
    async fn amb_websocket_push_streams_live_update() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let mock_http = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/now/table/incident/inc_ws_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": {
                    "sys_id": { "value": "inc_ws_1", "display_value": "inc_ws_1" },
                    "number": { "value": "INC0099999", "display_value": "INC0099999" },
                    "state": { "value": "6", "display_value": "Resolved" },
                    "sys_mod_count": { "value": "5", "display_value": "5" },
                    "sys_updated_on": { "value": "2026-11-11 16:00:00", "display_value": "2026-11-11 16:00:00" }
                }
            })))
            .mount(&mock_http)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/now/table/sys_journal_field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": [] })))
            .mount(&mock_http)
            .await;

        // Spin up a miniature CometD `/amb` WebSocket server that proxies HTTP REST to `mock_http`?
        // Even cleaner: verify `encode_record_watcher_channel` and Bayeux handshake over a local WS listener!
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((stream, _)) = ws_listener.accept().await {
                let mut ws = accept_async(stream).await.unwrap();
                // 1. Read handshake
                let _ = ws.next().await;
                ws.send(tokio_tungstenite::tungstenite::protocol::Message::Text(
                    json!([{
                        "id": "1",
                        "channel": "/meta/handshake",
                        "successful": true,
                        "clientId": "amb_client_42"
                    }])
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

                // Read subscribe + connect, then push a Record Watcher update!
                let _ = ws.next().await;
                let ch = encode_record_watcher_channel("incident", "sys_id=inc_ws_1");
                ws.send(tokio_tungstenite::tungstenite::protocol::Message::Text(
                    json!([{
                        "channel": ch,
                        "data": {
                            "action": "update",
                            "sys_id": "inc_ws_1",
                            "table_name": "incident"
                        }
                    }])
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        });

        let ws_profile =
            InstanceProfile::new("WS", &format!("http://{ws_addr}"), "cid", false).unwrap();
        let creds = StoredCredentials {
            access_token: "tok_ws".into(),
            refresh_token: None,
            expires_at: None,
            client_secret: None,
        };
        let cred_store = Arc::new(MemoryCredentialStore::with_credentials(
            ws_profile.id.clone(),
            creds,
        ));
        let ws_client = SnClient::new(ws_profile, cred_store).unwrap();
        let (evt_tx, mut evt_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let ch = encode_record_watcher_channel("incident", "sys_id=inc_ws_1");

        tokio::spawn(async move {
            run_amb_session(ws_client, vec![ch], cmd_rx, evt_tx).await;
        });

        let first = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, AmbNotification::Connected);

        let second = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            second,
            AmbNotification::RecordWatcherAction {
                ref table,
                ref sys_id,
                ref action,
                ..
            } if table == "incident" && sys_id == "inc_ws_1" && action == "update"
        ));
    }
}
