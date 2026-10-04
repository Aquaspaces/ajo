//! Requests to a particular live Studio connection. Replies are acknowledged by
//! the plugin; a queued command is never reported as a completed operation.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const MAX_CLIENTS: usize = 16;
const MAX_PENDING: usize = 16;
pub const MAX_SELECTION: usize = 128;
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

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum StudioAction {
    GetStatus,
    GetSelection,
    SetSelection { ids: Vec<Ref> },
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
        if let StudioAction::SetSelection { ids } = &action {
            if ids.len() > MAX_SELECTION {
                return Err(StudioError::Invalid(
                    "at most 128 selection IDs are allowed".into(),
                ));
            }
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

    #[tokio::test]
    async fn replies_are_owned_by_the_connection_and_only_complete_once() {
        let bridge = Arc::new(StudioBridge::new(true));
        let (first, mut requests) = bridge.register(info()).unwrap();
        let (second, _) = bridge.register(info()).unwrap();
        let request = bridge.request(&first.id, StudioAction::GetStatus);
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
        let (result, _) = tokio::join!(bridge.request(&old_id, StudioAction::GetStatus), async {
            requests.recv().await.unwrap();
            drop(connection);
        });
        assert!(matches!(result, Err(StudioError::Disconnected)));
        let (new, _) = bridge.register(info()).unwrap();
        assert_ne!(new.id, old_id);
        assert!(matches!(
            bridge.request(&old_id, StudioAction::GetStatus).await,
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
                StudioAction::GetStatus,
                Duration::from_millis(1),
            )
            .await;
        assert!(matches!(result, Err(StudioError::Timeout)));
        let command = requests.recv().await.unwrap();
        assert!(!connection.is_pending(&command.request_id));
        assert!(!connection.complete(&command.request_id, Ok(json!({}))));
        let mut request = Box::pin(bridge.request(&connection.id, StudioAction::GetStatus));
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
            let mut future = Box::pin(bridge.request(&connection.id, StudioAction::GetStatus));
            assert!(futures::poll!(&mut future).is_pending());
            pending.push(future);
        }
        assert!(matches!(
            bridge
                .request(&connection.id, StudioAction::GetStatus)
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
