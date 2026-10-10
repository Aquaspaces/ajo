//! Local HTTP controls and an independent control socket for the Studio plugin.

use std::{net::SocketAddr, sync::Arc};

use futures::{SinkExt, StreamExt};
use hyper::{body::HttpBody, Body, Method, Request, Response, StatusCode};
use hyper_tungstenite::{is_upgrade_request, tungstenite::Message, upgrade, HyperWebsocket};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{
    serve_session::ServeSession,
    studio::{StudioAction, StudioCommand, StudioConnection, StudioError, StudioInfo},
    SessionId,
};

use super::{origin::canonical, util};

pub const MAX_PACKET_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandRequest {
    session_id: SessionId,
    client_id: String,
    #[serde(flatten)]
    action: StudioAction,
}

pub async fn call(
    session: Arc<ServeSession>,
    remote_addr: SocketAddr,
    mut request: Request<Body>,
) -> Response<Body> {
    if !canonical(remote_addr.ip()).is_loopback() {
        return util::json(
            json!({"error":"Studio controls require a local connection"}),
            StatusCode::FORBIDDEN,
        );
    }
    let bridge = session.studio_bridge();
    if !bridge.enabled() {
        return error(StudioError::Disabled);
    }
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/api/studio/socket") => {
            if !is_upgrade_request(&request) {
                return error(StudioError::Invalid(
                    "/api/studio/socket requires a WebSocket upgrade".into(),
                ));
            }
            let config = hyper_tungstenite::tungstenite::protocol::WebSocketConfig {
                max_message_size: Some(MAX_PACKET_BYTES),
                max_frame_size: Some(MAX_PACKET_BYTES),
                ..Default::default()
            };
            let (response, socket) = match upgrade(&mut request, Some(config)) {
                Ok(upgraded) => upgraded,
                Err(err) => return error(StudioError::Invalid(err.to_string())),
            };
            tokio::spawn(async move {
                if let Err(err) = handle_control_socket(session, socket).await {
                    log::debug!("Studio control socket closed: {err}");
                }
            });
            response
        }
        (&Method::GET, "/api/studio/clients") => match bridge.clients() {
            Ok(clients) => util::json(
                json!({"sessionId":session.session_id(),"clients":clients}),
                StatusCode::OK,
            ),
            Err(err) => error(err),
        },
        (&Method::POST, "/api/studio/command") => {
            let content_type = request
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if content_type.split(';').next().unwrap_or("").trim() != "application/json" {
                return error(StudioError::Invalid(
                    "Content-Type must be application/json".into(),
                ));
            }
            let mut body = request.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        return error(StudioError::Invalid("could not read request body".into()))
                    }
                };
                if bytes.len().saturating_add(chunk.len()) > MAX_REQUEST_BYTES {
                    return util::json(
                        json!({"error":"Studio request body is too large"}),
                        StatusCode::PAYLOAD_TOO_LARGE,
                    );
                }
                bytes.extend_from_slice(&chunk);
            }
            let request: CommandRequest = match serde_json::from_slice(&bytes) {
                Ok(request) => request,
                Err(err) => return error(StudioError::Invalid(err.to_string())),
            };
            if request.session_id != session.session_id() {
                return util::json(
                    json!({"error":"Wrong session ID; reconnect to the server"}),
                    StatusCode::CONFLICT,
                );
            }
            match bridge.request(&request.client_id, request.action).await {
                Ok(result) => util::json(
                    json!({"sessionId":session.session_id(),"result":result}),
                    StatusCode::OK,
                ),
                Err(err) => error(err),
            }
        }
        _ => util::json(
            json!({"error":"Studio route not found"}),
            StatusCode::NOT_FOUND,
        ),
    }
}

async fn handle_control_socket(
    session: Arc<ServeSession>,
    socket: HyperWebsocket,
) -> anyhow::Result<()> {
    let mut socket = socket.await?;
    let mut connection: Option<StudioConnection> = None;
    let mut commands: Option<mpsc::Receiver<StudioCommand>> = None;
    loop {
        tokio::select! {
            command = async {
                match commands.as_mut() {
                    Some(commands) => commands.recv().await,
                    None => futures::future::pending().await,
                }
            } => {
                let Some(command) = command else { break; };
                if connection.as_ref().is_some_and(|connection| connection.is_pending(&command.request_id)) {
                    let packet = json!({"sessionId":session.session_id(), "packetType":"studioCommand", "body":command});
                    socket.send(Message::Text(serde_json::to_string(&packet)?)).await?;
                }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(err) = handle_packet(&session, &text, &mut connection, &mut commands) {
                            let _ = socket.send(Message::Close(None)).await;
                            return Err(err);
                        }
                    }
                    Some(Ok(Message::Ping(_))) => socket.flush().await?,
                    Some(Ok(Message::Pong(_))) => {},
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {
                        let _ = socket.send(Message::Close(None)).await;
                        anyhow::bail!("Studio control socket requires JSON text packets");
                    }
                    Some(Err(err)) => return Err(err.into()),
                }
            }
        }
    }
    Ok(())
}

fn error(err: StudioError) -> Response<Body> {
    let status = match &err {
        StudioError::Disabled => StatusCode::FORBIDDEN,
        StudioError::NotFound => StatusCode::NOT_FOUND,
        StudioError::Busy => StatusCode::TOO_MANY_REQUESTS,
        StudioError::Disconnected => StatusCode::SERVICE_UNAVAILABLE,
        StudioError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        StudioError::Rejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
        StudioError::Invalid(_) => StatusCode::BAD_REQUEST,
    };
    util::json(json!({"error":err.to_string()}), status)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientPacket {
    session_id: SessionId,
    packet_type: String,
    body: Value,
}

pub fn handle_packet(
    session: &Arc<ServeSession>,
    text: &str,
    connection: &mut Option<StudioConnection>,
    commands: &mut Option<mpsc::Receiver<StudioCommand>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(text.len() <= MAX_PACKET_BYTES, "Studio packet is too large");
    let packet: ClientPacket = serde_json::from_str(text)?;
    anyhow::ensure!(
        packet.session_id == session.session_id(),
        "Wrong Studio session ID"
    );
    match packet.packet_type.as_str() {
        "studioHello" => {
            let info: StudioInfo = serde_json::from_value(packet.body)?;
            // Opened may race with the client's ConnectionState check. A second
            // hello must not replace the identity owning outstanding requests.
            if let Some(connection) = connection {
                connection.update_info(info)?;
            } else {
                let (registered, receiver) = session.studio_bridge().register(info)?;
                *connection = Some(registered);
                *commands = Some(receiver);
            }
        }
        "studioResult" => {
            let body = packet
                .body
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Invalid Studio result"))?;
            anyhow::ensure!(
                body.len() == 2,
                "Studio result must contain requestId and exactly one of result/error"
            );
            let id = body
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing Studio requestId"))?;
            let reply = match (body.get("result"), body.get("error")) {
                (Some(value), None) => Ok(value.clone()),
                (None, Some(Value::String(message))) => Err(StudioError::Rejected(message.clone())),
                _ => anyhow::bail!("Studio result must contain exactly one of result/error"),
            };
            let connection = connection
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Studio has not registered"))?;
            // Late/duplicate replies are harmless, and cannot complete requests
            // belonging to a different socket or a reconnected Studio.
            connection.complete(id, reply);
        }
        _ => anyhow::bail!("Unknown Studio packet type"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_requests_reject_unrelated_fields_and_invalid_actions() {
        for invalid in [
            json!({"command":"getPluginState", "ids":[]}),
            json!({"command":"getStatus", "action":{"type":"disconnect"}}),
            json!({"command":"getPluginChanges", "offset":0, "limit":2, "extra":true}),
            json!({"command":"pluginAction", "action":{"type":"disconnect", "host":"localhost"}}),
            json!({"command":"pluginAction", "action":{"type":"setWindow", "enabled":"true"}}),
            json!({"command":"pluginAction", "action":{"type":"respondConfirmation", "confirmationId":"one", "decision":"yes"}}),
        ] {
            let mut request = invalid;
            request["sessionId"] = json!(SessionId::new());
            request["clientId"] = json!("client");
            assert!(
                serde_json::from_value::<CommandRequest>(request.clone()).is_err(),
                "{request}"
            );
        }
    }

    #[test]
    fn commands_preserve_plugin_parameters_and_confirmation_identity() {
        let request: CommandRequest = serde_json::from_value(json!({
            "sessionId":SessionId::new(), "clientId":"client", "command":"pluginAction",
            "action":{"type":"respondConfirmation", "confirmationId":"confirmation-7", "decision":"Reject"}
        })).unwrap();
        assert_eq!(
            serde_json::to_value(request.action).unwrap(),
            json!({
                "command":"pluginAction", "action":{"type":"respondConfirmation", "confirmationId":"confirmation-7", "decision":"Reject"}
            })
        );
    }
}
