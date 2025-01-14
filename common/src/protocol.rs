use std::fmt::Display;

use crate::{
    log_types::{BackendAddr, LoggableTime},
    names::{BackendActionName, BackendName},
    typed_socket::ChannelMessage,
    types::{
        backend_state::TerminationReason, BackendState, BearerToken, ClusterName, KeyConfig,
        NodeId, SecretToken, Subdomain, TerminationKind,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum ApiErrorKind {
    FailedToAcquireKey,
    KeyUnheldNoSpawnConfig,
    KeyHeldUnhealthy,
    KeyHeld,
    NoDroneAvailable,
    FailedToRemoveKey,
    DatabaseError,
    NoClusterProvided,
    NotFound,
    InvalidClusterName,
    Other,
}

#[derive(thiserror::Error, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub id: String,
    pub kind: ApiErrorKind,
    pub message: String,
}

impl Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, valuable::Valuable, PartialEq)]
pub struct KeyDeadlines {
    /// When the key should be renewed.
    pub renew_at: LoggableTime,

    /// When the backend should be soft-terminated if the key could not be renewed.
    pub soft_terminate_at: LoggableTime,

    /// When the backend should be hard-terminated if the key could not be renewed.
    pub hard_terminate_at: LoggableTime,
}

#[derive(Serialize, Deserialize, Debug, Clone, valuable::Valuable, PartialEq)]
pub struct AcquiredKey {
    /// Details of the key itself.
    pub key: KeyConfig,

    /// Deadlines for key expiration stages.
    pub deadlines: KeyDeadlines,

    /// A unique key associated with a key for the duration it is acquired. This does not
    /// change across renewals, but is incremented when the key is released and then acquired.
    /// This is used internally to track the key during renewals, but can also be exposed to
    /// backends as a fencing token.
    /// (https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html).
    pub token: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BackendAction {
    Spawn {
        executable: Value,
        key: AcquiredKey,
        static_token: Option<BearerToken>,
    },
    Terminate {
        kind: TerminationKind,
        reason: TerminationReason,
    },
}

impl valuable::Valuable for BackendAction {
    fn as_value(&self) -> valuable::Value {
        valuable::Value::Mappable(self)
    }

    fn visit(&self, visit: &mut dyn valuable::Visit) {
        match self {
            BackendAction::Spawn {
                key, static_token, ..
            } => {
                visit.visit_entry(valuable::Value::String("key"), key.as_value());
                visit.visit_entry(
                    valuable::Value::String("static_token"),
                    static_token.as_value(),
                );
            }
            BackendAction::Terminate { kind, reason } => {
                visit.visit_entry(valuable::Value::String("kind"), kind.as_value());
                visit.visit_entry(valuable::Value::String("reason"), reason.as_value());
            }
        }
    }
}

impl valuable::Mappable for BackendAction {
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            BackendAction::Spawn { .. } => (2, Some(2)),
            BackendAction::Terminate { .. } => (2, Some(2)),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, valuable::Valuable)]
pub struct BackendStateMessage {
    pub event_id: BackendEventId,
    pub backend_id: BackendName,
    pub state: BackendState,

    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub address: Option<BackendAddr>,
    pub timestamp: LoggableTime,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, valuable::Valuable)]
pub struct BackendEventId(i64);

impl From<i64> for BackendEventId {
    fn from(i: i64) -> Self {
        Self(i)
    }
}

impl From<BackendEventId> for i64 {
    fn from(id: BackendEventId) -> Self {
        id.0
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RenewKeyRequest {
    pub backend: BackendName,

    pub local_time: LoggableTime,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Heartbeat {
    pub local_time: LoggableTime,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MessageFromDrone {
    Heartbeat(Heartbeat),
    BackendEvent(BackendStateMessage),
    BackendMetrics(BackendMetricsMessage),
    AckAction { action_id: BackendActionName },
    RenewKey(RenewKeyRequest),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BackendMetricsMessage {
    pub backend_id: BackendName,
    /// Memory used by backend excluding inactive file cache, same as use shown by docker stats
    /// ref: https://github.com/docker/cli/blob/master/cli/command/container/stats_helpers.go#L227C45-L227C45
    pub mem_used: u64,
    /// Memory used by backend in bytes
    /// (calculated using kernel memory used by cgroup + page cache memory used by cgroup)
    pub mem_total: u64,
    /// Active memory (non reclaimable)
    pub mem_active: u64,
    /// Inactive memory (reclaimable)
    pub mem_inactive: u64,
    /// Unevictable memory (mlock etc)
    pub mem_unevictable: u64,
    /// The backend's memory limit
    pub mem_limit: u64,
    /// Nanoseconds of CPU used by backend since last message
    pub cpu_used: u64,
    /// Total CPU nanoseconds for system since last message
    pub sys_cpu: u64,
}

impl ChannelMessage for MessageFromDrone {
    type Reply = MessageToDrone;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RenewKeyResponse {
    /// The backend whose associated key was renewed.
    pub backend: BackendName,

    /// The key that was renewed, if successful.
    /// If the key was not renewed, this will be None.
    pub deadlines: Option<KeyDeadlines>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendActionMessage {
    pub action_id: BackendActionName,
    pub backend_id: BackendName,
    pub drone_id: NodeId,
    pub action: BackendAction,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MessageToDrone {
    Action(BackendActionMessage),
    /// Acknowledge that the container has received and processed a backend event.
    AckEvent {
        event_id: BackendEventId,
    },
    RenewKeyResponse(RenewKeyResponse),
}

impl ChannelMessage for MessageToDrone {
    type Reply = MessageFromDrone;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub backend_id: BackendName,
    pub address: BackendAddr,
    pub secret_token: SecretToken,
    pub cluster: ClusterName,
    pub user: Option<String>,
    pub user_data: Option<serde_json::Value>,
    pub subdomain: Option<Subdomain>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CertManagerRequest {
    /// Request a certificate lease for a cluster.
    CertLeaseRequest,

    /// Set the TXT record for a cluster. Fails if another proxy
    /// has more recently been granted the lease.
    SetTxtRecord { txt_value: String },

    /// Release a certificate lease for a cluster so that another
    /// proxy can request it immediately.
    ReleaseCertLease,
}

impl ChannelMessage for CertManagerRequest {
    type Reply = CertManagerResponse;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, valuable::Valuable)]
pub enum CertManagerResponse {
    /// Acknowledge a lease request and indicate whether it was accepted.
    CertLeaseResponse { accepted: bool },

    /// Acknowledge a TXT record update and indicate whether it was accepted.
    SetTxtRecordResponse { accepted: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RouteInfoRequest {
    pub token: BearerToken,
}

impl ChannelMessage for RouteInfoRequest {
    type Reply = RouteInfoResponse;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RouteInfoResponse {
    pub token: BearerToken,
    pub route_info: Option<RouteInfo>,
}

impl ChannelMessage for RouteInfoResponse {
    type Reply = RouteInfoRequest;
}

impl ChannelMessage for CertManagerResponse {
    type Reply = CertManagerRequest;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MessageFromProxy {
    RouteInfoRequest(RouteInfoRequest),
    KeepAlive(BackendName),
    CertManagerRequest(CertManagerRequest),
}

impl ChannelMessage for MessageFromProxy {
    type Reply = MessageToProxy;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MessageToProxy {
    RouteInfoResponse(RouteInfoResponse),
    CertManagerResponse(CertManagerResponse),
    BackendRemoved { backend: BackendName },
}

impl ChannelMessage for MessageToProxy {
    type Reply = MessageFromProxy;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, valuable::Valuable)]
pub enum MessageFromDns {
    TxtRecordRequest { cluster: ClusterName },
}

impl ChannelMessage for MessageFromDns {
    type Reply = MessageToDns;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MessageToDns {
    TxtRecordResponse {
        cluster: ClusterName,
        txt_value: Option<String>,
    },
}

impl ChannelMessage for MessageToDns {
    type Reply = MessageFromDns;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StatusResponse {
    pub status: String,
    pub version: String,
    pub hash: String,
}
