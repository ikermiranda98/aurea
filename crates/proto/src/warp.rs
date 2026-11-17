//! WARP ring protocol types (the free-model egress rotation surface).
//!
//! Minimal shapes for the `WarpStatus`/`WarpRotate`/`WarpHistory` RPCs; the
//! crate that produces them lives in `crates/warp` (lane C).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarpState {
    /// warp-cli reports a healthy tunnel.
    Connected,
    /// A connect/rotate is in flight.
    Connecting,
    /// Installed (or installable) but not connected.
    Disconnected,
    /// warp-cli is not installed on this machine; the ring degrades gracefully.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarpStatus {
    pub state: WarpState,
    /// The UTC day the ring is budgeting for (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    /// The current egress /64 prefix, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Epoch ms of the last successful rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rotate_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarpRotateResult {
    pub state: WarpState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarpHistoryEntry {
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Why the ring rotated ("rejected", "daily", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
