//! Real-time ServiceNow synchronization engine (`sn-realtime`).
//!
//! Provides [`RealtimeCoordinator`], which streams live record updates, journal entries, and
//! list deltas into [`sn_cache::SnCache`] and GPUI views using:
//! 1. **Primary**: ServiceNow Asynchronous Message Bus (`/amb` CometD/Bayeux over WebSocket)
//!    Record Watcher channels (`/rw/default/<table>/<filter_b64>`).
//! 2. **Automatic Fallback**: Adaptive high-frequency REST watermark polling (`sys_mod_count` /
//!    `sys_updated_on` every 1.5s while focused, 10s when unfocused) whenever `/amb` is
//!    unavailable or rejects pure OAuth tokens without browser UI cookies.

pub mod amb;
pub mod coordinator;

pub use amb::{AmbChannelEncoding, encode_record_watcher_channel};
pub use coordinator::{
    RealtimeConfig, RealtimeCoordinator, RealtimeEvent, RealtimeTransportMode, SubscriptionGuard,
    SubscriptionId, UpdateSource, WatchTarget,
};
