//! Requests to a particular live Studio connection. Replies are acknowledged by
//! the plugin; a queued command is never reported as a completed operation.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const MAX_CLIENTS: usize = 16;
const MAX_PENDING: usize = 16;
pub const MAX_SELECTION: usize = 128;
pub const MAX_PLUGIN_CHANGES: usize = 100;
pub const MAX_PLUGIN_DIFF_BYTES: usize = 16 * 1024;
pub const MAX_PLUGIN_VALUE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PLUGIN_REVISION: u64 = 9_007_199_254_740_991;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StudioInfo {
    pub place_id: u64,
    pub game_id: u64,
    pub place_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioClient {
    pub client_id: String,
    #[serde(flatten)]
    pub info: StudioInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "camelCase", deny_unknown_fields)]
pub enum StudioAction {
    GetStatus {},
    GetSelection {},
    SetSelection {
        ids: Vec<Ref>,
    },
    GetPluginState {},
    GetPluginChanges {
        offset: usize,
        limit: usize,
    },
    GetPluginDiff {
        id: String,
        property: String,
        side: DiffSide,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_plugin_diff_limit")]
        limit: usize,
        #[serde(
            default,
            deserialize_with = "present_option",
            skip_serializing_if = "Option::is_none"
        )]
        revision: Option<u64>,
    },
    PluginAction {
        action: PluginAction,
    },
}

fn default_plugin_diff_limit() -> usize {
    8192
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffSide {
    Old,
    New,
}

pub fn validate_plugin_diff(
    id: &str,
    property: &str,
    offset: usize,
    limit: usize,
    revision: Option<u64>,
) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > 128 || property.is_empty() || property.len() > 1024 {
        return Err("id and property must identify a property from studio_plugin_changes");
    }
    if offset > MAX_PLUGIN_VALUE_BYTES || limit == 0 || limit > MAX_PLUGIN_DIFF_BYTES {
        return Err(
            "diff offset must be between 0 and 8388608, and limit between 1 and 16384 bytes",
        );
    }
    if (offset > 0 && revision.is_none())
        || revision.is_some_and(|revision| revision > MAX_PLUGIN_REVISION)
    {
        return Err("revision from studio_plugin_changes or studio_plugin_diff is required for offset > 0 and must be an integer between 0 and 9007199254740991");
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PluginAction {
    Connect {
        #[serde(
            default,
            deserialize_with = "present_option",
            skip_serializing_if = "Option::is_none"
        )]
        host: Option<String>,
        #[serde(
            default,
            deserialize_with = "present_option",
            skip_serializing_if = "Option::is_none"
        )]
        port: Option<u16>,
    },
    Disconnect {},
    Reconnect {},
    SetSettings {
        settings: Map<String, Value>,
    },
    RespondConfirmation {
        confirmation_id: String,
        decision: ConfirmationDecision,
    },
    SetWindow {
        enabled: bool,
    },
    OpenSettings {},
    CloseSettings {},
    DismissError {},
    ForgetProject {},
    Notification {
        id: u64,
        #[serde(
            default,
            deserialize_with = "present_option",
            skip_serializing_if = "Option::is_none"
        )]
        action: Option<String>,
    },
    CheckUpdates {},
    FocusChange {
        id: String,
    },
}

fn present_option<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ConfirmationDecision {
    Accept,
    Reject,
    Abort,
}

impl PluginAction {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Connect { host, port } => {
                if host.as_ref().is_some_and(|host| !valid_plugin_host(host)) {
                    return Err("host must be a hostname or IP address with an optional http/https scheme and no path, port, credentials, or whitespace");
                }
                if *port == Some(0) {
                    return Err("port must be between 1 and 65535");
                }
            }
            Self::SetSettings { settings } => {
                if settings.is_empty() || settings.len() > 64 {
                    return Err("settings must contain between 1 and 64 entries");
                }
                if settings.iter().any(|(name, value)| {
                    name.is_empty()
                        || name.len() > 128
                        || !matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
                        || value.as_str().is_some_and(|value| value.len() > 1024)
                }) {
                    return Err("settings must contain named boolean, number, or string values");
                }
            }
            Self::RespondConfirmation {
                confirmation_id, ..
            } => {
                if confirmation_id.is_empty() || confirmation_id.len() > 256 {
                    return Err("confirmationId must identify the current confirmation from studio_plugin_state");
                }
            }
            Self::Notification { id, action } => {
                if *id == 0
                    || *id > i32::MAX as u64
                    || action
                        .as_ref()
                        .is_some_and(|action| action.is_empty() || action.len() > 256)
                {
                    return Err("notification id and action must identify an entry from studio_plugin_state");
                }
            }
            Self::FocusChange { id } if id.is_empty() || id.len() > 128 => {
                return Err("id must identify an entry from studio_plugin_changes");
            }
            _ => {}
        }
        Ok(())
    }
}

fn valid_plugin_host(host: &str) -> bool {
    if host.is_empty()
        || host.len() > 261
        || host.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '\\'
        })
    {
        return false;
    }
    let authority = host
        .strip_prefix("http://")
        .or_else(|| host.strip_prefix("https://"))
        .unwrap_or(host);
    if authority.is_empty()
        || authority.contains('/')
        || (authority.starts_with('[') && !authority.ends_with(']'))
        || (!authority.starts_with('[') && authority.contains(':'))
    {
        return false;
    }
    let origin = format!("http://{authority}");
    reqwest::Url::parse(&origin).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.port().is_none()
    })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioCommand {
    pub request_id: String,
    #[serde(flatten)]
    pub action: StudioAction,
}

#[derive(Debug, Error)]
pub enum StudioError {
    #[error("Studio controls are disabled; start rojo serve with --enable-studio-controls")]
    Disabled,
    #[error("Studio connection was not found; list connected Studios again")]
    NotFound,
    #[error("Studio control capacity exceeded; try again after pending requests finish")]
    Busy,
    #[error("Studio disconnected before acknowledging the command")]
    Disconnected,
    #[error("Studio did not acknowledge the command before its deadline; it may already have executed, so inspect Studio before retrying")]
    Timeout,
    #[error("Studio rejected the command: {0}")]
    Rejected(String),
    #[error("Invalid Studio request: {0}")]
    Invalid(String),
}

type Reply = Result<Value, StudioError>;

struct Client {
    info: StudioInfo,
    sender: mpsc::Sender<StudioCommand>,
    pending: HashMap<String, oneshot::Sender<Reply>>,
}

pub struct StudioBridge {
    enabled: bool,
    clients: Mutex<HashMap<String, Client>>,
}

impl StudioBridge {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn clients(&self) -> Result<Vec<StudioClient>, StudioError> {
        self.require_enabled()?;
        let mut clients: Vec<_> = self
            .clients
            .lock()
            .unwrap()
            .iter()
            .map(|(id, client)| StudioClient {
                client_id: id.clone(),
                info: client.info.clone(),
            })
            .collect();
        clients.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        Ok(clients)
    }

    pub fn register(
        self: &Arc<Self>,
        info: StudioInfo,
    ) -> Result<(StudioConnection, mpsc::Receiver<StudioCommand>), StudioError> {
        self.require_enabled()?;
        if info.place_name.len() > 1024 {
            return Err(StudioError::Invalid("placeName is too long".into()));
        }
        let mut clients = self.clients.lock().unwrap();
        if clients.len() >= MAX_CLIENTS {
            return Err(StudioError::Busy);
        }
        let id = Uuid::new_v4().to_string();
        let (sender, receiver) = mpsc::channel(MAX_PENDING);
        clients.insert(
            id.clone(),
            Client {
                info,
                sender,
                pending: HashMap::new(),
            },
        );
        Ok((
            StudioConnection {
                bridge: Arc::clone(self),
                id,
            },
            receiver,
        ))
    }

    pub async fn request(self: &Arc<Self>, client_id: &str, action: StudioAction) -> Reply {
        self.request_with_timeout(client_id, action, COMMAND_TIMEOUT)
            .await
    }

    async fn request_with_timeout(
        self: &Arc<Self>,
        client_id: &str,
        action: StudioAction,
        timeout: Duration,
    ) -> Reply {
        self.require_enabled()?;
        match &action {
            StudioAction::SetSelection { ids } if ids.len() > MAX_SELECTION => {
                return Err(StudioError::Invalid(
                    "at most 128 selection IDs are allowed".into(),
                ));
            }
            StudioAction::GetPluginChanges { limit, .. }
                if *limit == 0 || *limit > MAX_PLUGIN_CHANGES =>
            {
                return Err(StudioError::Invalid(
                    "limit must be between 1 and 100".into(),
                ));
            }
            StudioAction::PluginAction { action } => {
                action
                    .validate()
                    .map_err(|message| StudioError::Invalid(message.into()))?;
            }
            StudioAction::GetPluginDiff {
                id,
                property,
                offset,
                limit,
                revision,
                ..
            } => {
                validate_plugin_diff(id, property, *offset, *limit, *revision)
                    .map_err(|message| StudioError::Invalid(message.into()))?;
            }
            _ => {}
        }
        let request_id = Uuid::new_v4().to_string();
        let receiver = {
            let mut clients = self.clients.lock().unwrap();
            let client = clients.get_mut(client_id).ok_or(StudioError::NotFound)?;
            if client.pending.len() >= MAX_PENDING {
                return Err(StudioError::Busy);
            }
            let (sender, receiver) = oneshot::channel();
            client
                .sender
                .try_send(StudioCommand {
                    request_id: request_id.clone(),
                    action,
                })
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => StudioError::Busy,
                    mpsc::error::TrySendError::Closed(_) => StudioError::Disconnected,
                })?;
            client.pending.insert(request_id.clone(), sender);
            receiver
        };
        // Also remove requests when their HTTP future is cancelled. The socket
        // checks this map before sending, so expired queued commands are skipped.
        let _pending = PendingRequest {
            bridge: Arc::clone(self),
            client_id: client_id.to_owned(),
            request_id,
        };
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(StudioError::Disconnected),
            Err(_) => Err(StudioError::Timeout),
        }
    }

    fn require_enabled(&self) -> Result<(), StudioError> {
        if self.enabled {
            Ok(())
        } else {
            Err(StudioError::Disabled)
        }
    }
}

pub struct StudioConnection {
    bridge: Arc<StudioBridge>,
    id: String,
}

impl StudioConnection {
    pub fn update_info(&self, info: StudioInfo) -> Result<(), StudioError> {
        if info.place_name.len() > 1024 {
            return Err(StudioError::Invalid("placeName is too long".into()));
        }
        let mut clients = self.bridge.clients.lock().unwrap();
        let client = clients.get_mut(&self.id).ok_or(StudioError::Disconnected)?;
        client.info = info;
        Ok(())
    }

    pub fn is_pending(&self, request_id: &str) -> bool {
        self.bridge
            .clients
            .lock()
            .unwrap()
            .get(&self.id)
            .is_some_and(|client| client.pending.contains_key(request_id))
    }

    pub fn complete(&self, request_id: &str, reply: Reply) -> bool {
        let sender = self
            .bridge
            .clients
            .lock()
            .unwrap()
            .get_mut(&self.id)
            .and_then(|client| client.pending.remove(request_id));
        sender.is_some_and(|sender| sender.send(reply).is_ok())
    }
}

impl Drop for StudioConnection {
    fn drop(&mut self) {
        if let Some(client) = self.bridge.clients.lock().unwrap().remove(&self.id) {
            for (_, sender) in client.pending {
                let _ = sender.send(Err(StudioError::Disconnected));
            }
        }
    }
}

struct PendingRequest {
    bridge: Arc<StudioBridge>,
    client_id: String,
    request_id: String,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Some(client) = self.bridge.clients.lock().unwrap().get_mut(&self.client_id) {
            client.pending.remove(&self.request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info() -> StudioInfo {
        StudioInfo {
            place_id: 1,
            game_id: 2,
            place_name: "Test".into(),
        }
    }

    #[test]
    fn plugin_hosts_accept_ui_endpoints_without_credentials_or_paths() {
        for host in [
            "localhost",
            "127.0.0.1",
            "[::1]",
            "http://localhost",
            "https://example.com",
            "http://[::1]",
        ] {
            assert!(valid_plugin_host(host), "{host}");
        }
        for host in [
            "",
            " localhost",
            "host\n",
            "http://user@host",
            "host/path",
            "host?query",
            "host#fragment",
            "ftp://host",
            "host:34872",
            "host\\path",
            "http://localhost/",
            "http://localhost:80",
            "https://localhost:443",
            "http://[::1]:80",
        ] {
            assert!(!valid_plugin_host(host), "{host}");
        }
    }

    #[tokio::test]
    async fn refreshed_metadata_preserves_client_identity_and_pending_requests() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (connection, mut requests) = bridge.register(info()).unwrap();
        let (result, _) = tokio::join!(
            bridge.request(&connection.id, StudioAction::GetPluginState {}),
            async {
                let request = requests.recv().await.unwrap();
                connection
                    .update_info(StudioInfo {
                        place_id: 99,
                        game_id: 100,
                        place_name: "New place".into(),
                    })
                    .unwrap();
                let clients = bridge.clients().unwrap();
                assert_eq!(clients.len(), 1);
                assert_eq!(clients[0].client_id, connection.id);
                assert_eq!(clients[0].info.place_id, 99);
                assert_eq!(clients[0].info.place_name, "New place");
                assert!(connection.complete(&request.request_id, Ok(json!({"placeId":99}))));
            }
        );
        assert_eq!(result.unwrap(), json!({"placeId":99}));
    }

    #[tokio::test]
    async fn invalid_plugin_requests_fail_before_queueing() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (connection, mut requests) = bridge.register(info()).unwrap();
        for action in [
            StudioAction::GetPluginChanges {
                offset: 0,
                limit: 0,
            },
            StudioAction::GetPluginChanges {
                offset: 0,
                limit: 101,
            },
            StudioAction::PluginAction {
                action: PluginAction::Connect {
                    host: None,
                    port: Some(0),
                },
            },
            StudioAction::PluginAction {
                action: PluginAction::SetSettings {
                    settings: Map::new(),
                },
            },
            StudioAction::PluginAction {
                action: PluginAction::RespondConfirmation {
                    confirmation_id: String::new(),
                    decision: ConfirmationDecision::Accept,
                },
            },
        ] {
            assert!(matches!(
                bridge.request(&connection.id, action).await,
                Err(StudioError::Invalid(_))
            ));
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn replies_are_owned_by_the_connection_and_only_complete_once() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (first, mut requests) = bridge.register(info()).unwrap();
        let (second, _) = bridge.register(info()).unwrap();
        let request = bridge.request(&first.id, StudioAction::GetStatus {});
        let reply = async {
            let command = requests.recv().await.unwrap();
            assert!(!second.complete(&command.request_id, Ok(json!("wrong client"))));
            assert!(first.complete(&command.request_id, Ok(json!({"isEdit":true}))));
            assert!(!first.complete(&command.request_id, Ok(json!("duplicate"))));
        };
        let (result, _) = tokio::join!(request, reply);
        assert_eq!(result.unwrap(), json!({"isEdit":true}));
    }

    #[tokio::test]
    async fn disconnect_fails_pending_requests_and_reconnect_gets_a_new_id() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (connection, mut requests) = bridge.register(info()).unwrap();
        let old_id = connection.id.clone();
        let (result, _) =
            tokio::join!(bridge.request(&old_id, StudioAction::GetStatus {}), async {
                requests.recv().await.unwrap();
                drop(connection);
            });
        assert!(matches!(result, Err(StudioError::Disconnected)));
        let (new, _) = bridge.register(info()).unwrap();
        assert_ne!(new.id, old_id);
        assert!(matches!(
            bridge.request(&old_id, StudioAction::GetStatus {}).await,
            Err(StudioError::NotFound)
        ));
    }

    #[tokio::test]
    async fn timeout_and_cancellation_remove_pending_commands() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (connection, mut requests) = bridge.register(info()).unwrap();
        let result = bridge
            .request_with_timeout(
                &connection.id,
                StudioAction::GetStatus {},
                Duration::from_millis(1),
            )
            .await;
        assert!(matches!(result, Err(StudioError::Timeout)));
        let command = requests.recv().await.unwrap();
        assert!(!connection.is_pending(&command.request_id));
        assert!(!connection.complete(&command.request_id, Ok(json!({}))));
        let mut request = Box::pin(bridge.request(&connection.id, StudioAction::GetStatus {}));
        assert!(futures::poll!(&mut request).is_pending());
        let command = requests.recv().await.unwrap();
        drop(request);
        assert!(!connection.is_pending(&command.request_id));
    }

    #[tokio::test]
    async fn disabled_and_overloaded_bridges_fail_without_queueing() {
        let disabled = Arc::new(StudioBridge::new(false));
        assert!(matches!(disabled.clients(), Err(StudioError::Disabled)));
        assert!(matches!(
            disabled.register(info()),
            Err(StudioError::Disabled)
        ));
        let bridge = Arc::new(StudioBridge::new(true));
        let (connection, _requests) = bridge.register(info()).unwrap();
        let mut pending = Vec::new();
        for _ in 0..MAX_PENDING {
            let mut future = Box::pin(bridge.request(&connection.id, StudioAction::GetStatus {}));
            assert!(futures::poll!(&mut future).is_pending());
            pending.push(future);
        }
        assert!(matches!(
            bridge
                .request(&connection.id, StudioAction::GetStatus {})
                .await,
            Err(StudioError::Busy)
        ));
        drop(pending);
        assert!(bridge
            .clients
            .lock()
            .unwrap()
            .get(&connection.id)
            .unwrap()
            .pending
            .is_empty());
    }
}
