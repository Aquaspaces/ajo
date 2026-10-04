//! A stdio MCP adapter for an existing local Rojo serve session.

use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Write},
    net::IpAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, ensure, Context};
use clap::Parser;
use crossbeam_channel::{bounded, Sender, TrySendError};
use rbx_dom_weak::types::Ref;
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{web::util::deserialize_msgpack, web_api::ReadResponse, SessionId};

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_INSTANCE_IDS: usize = 128;
const MAX_PENDING_REQUESTS: usize = 16;
const MAX_QUEUED_MESSAGES: usize = 64;

/// Give MCP agents access to an existing local Rojo server and connected Studio clients.
#[derive(Debug, Parser)]
pub struct McpCommand {
    /// Local Rojo server origin. Start it with `rojo serve` separately.
    #[clap(long, default_value = "http://127.0.0.1:34872")]
    pub server: String,
}

impl McpCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let mut server = McpServer::new(&self.server)?;
        server.run(io::BufReader::new(io::stdin()), io::stdout().lock())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Uninitialized,
    AwaitingInitialized,
    Ready,
}

struct McpServer {
    backend: Backend,
    state: State,
}

impl McpServer {
    fn new(server: &str) -> anyhow::Result<Self> {
        Ok(Self {
            backend: Backend::new(server)?,
            state: State::Uninitialized,
        })
    }

    fn run(
        &mut self,
        input: impl BufRead + Send + 'static,
        mut output: impl Write,
    ) -> anyhow::Result<()> {
        let pending = Arc::new(Mutex::new(PendingRequests::default()));
        let (sender, receiver) = bounded(MAX_QUEUED_MESSAGES);
        let reader_pending = Arc::clone(&pending);
        let reader = thread::spawn(move || read_messages(input, sender, reader_pending));
        let result = (|| {
            while let Ok(incoming) = receiver.recv() {
                check_input_failure(&pending, &mut output)?;
                let response = match incoming {
                    Incoming::Response(response) => Some(response),
                    Incoming::Message { message, request } => {
                        let cancelled = request.as_ref().map(|(_, request)| &request.cancelled);
                        let response = if is_cancelled(cancelled) {
                            None
                        } else {
                            self.handle_request(message, cancelled)
                        };
                        if let Some((id, request)) = request {
                            let mut pending = pending.lock().unwrap();
                            pending.requests.remove(&id);
                            if request.cancelled.load(Ordering::Relaxed) {
                                None
                            } else {
                                response
                            }
                        } else {
                            response
                        }
                    }
                };
                if let Some(response) = response {
                    write_response(&mut output, &response)?;
                }
            }
            check_input_failure(&pending, &mut output)
        })();
        drop(receiver);
        // A blocking stdin read cannot be interrupted portably. On output failure,
        // return without joining it; process shutdown will release stdin. Normal
        // EOF closes the queue only after all accepted requests have been drained.
        if result.is_ok() || reader.is_finished() {
            let joined = reader.join();
            if result.is_ok() && joined.is_err() {
                bail!("MCP input thread panicked");
            }
        }
        result
    }

    #[cfg(test)]
    fn handle(&mut self, message: Value) -> Option<Value> {
        self.handle_request(message, None)
    }

    fn handle_request(&mut self, message: Value, cancelled: Option<&AtomicBool>) -> Option<Value> {
        let Some(request) = message.as_object() else {
            return Some(rpc_error(Value::Null, -32600, "Expected a JSON-RPC object"));
        };
        let id = request.get("id");
        let id_is_valid = id.is_none_or(valid_id);
        if request.get("jsonrpc") != Some(&json!("2.0"))
            || request.get("method").and_then(Value::as_str).is_none()
            || !id_is_valid
            || request.get("params").is_some_and(|v| !v.is_object())
        {
            return Some(rpc_error(
                if id_is_valid {
                    id.cloned().unwrap_or(Value::Null)
                } else {
                    Value::Null
                },
                -32600,
                "Invalid JSON-RPC request",
            ));
        }
        let method = request["method"].as_str().unwrap();
        let Some(id) = id else {
            if method == "notifications/initialized" && self.state == State::AwaitingInitialized {
                self.state = State::Ready;
            }
            return None;
        };
        let empty = Map::new();
        let params = request
            .get("params")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let result = match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            _ if self.state != State::Ready => {
                Err((-32002, "Initialize the MCP connection before using tools"))
            }
            "tools/list" => {
                if params.keys().any(|key| key != "_meta") {
                    Err((
                        -32602,
                        "tools/list does not accept a cursor or other parameters",
                    ))
                } else {
                    Ok(json!({"tools": tools()}))
                }
            }
            "tools/call" => self.call_tool(params, cancelled),
            _ => Err((-32601, "Method not found")),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => rpc_error(id.clone(), code, message),
        })
    }

    fn initialize(&mut self, params: &Map<String, Value>) -> RpcResult {
        if self.state != State::Uninitialized {
            return Err((-32600, "This MCP connection is already initialized"));
        }
        let client = params.get("clientInfo").and_then(Value::as_object);
        if params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_none()
            || params
                .get("capabilities")
                .and_then(Value::as_object)
                .is_none()
            || client
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .is_none()
            || client
                .and_then(|v| v.get("version"))
                .and_then(Value::as_str)
                .is_none()
        {
            return Err((-32602, "initialize requires protocolVersion, capabilities, and clientInfo with name and version"));
        }
        self.state = State::AwaitingInitialized;
        Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rojo", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Rojo instance reads describe the served filesystem project. Studio tools report live state from an explicitly selected connected Studio client. Start rojo serve with --enable-studio-controls to opt in to Studio controls."
        }))
    }

    fn call_tool(&self, params: &Map<String, Value>, cancelled: Option<&AtomicBool>) -> RpcResult {
        if params
            .keys()
            .any(|key| !["name", "arguments", "_meta"].contains(&key.as_str()))
        {
            return Err((-32602, "Unknown tools/call parameter"));
        }
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call requires a tool name"))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            Some(Value::Object(args)) => args,
            None => &empty,
            _ => return Err((-32602, "Tool arguments must be an object")),
        };
        let call = validate_tool(name, args).map_err(|message| (-32602, message))?;
        let result = self.backend.call(call, cancelled);
        Ok(match result {
            Ok(value) => json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "isError": false
            }),
            Err(error) => json!({
                "content": [{"type": "text", "text": format!("{error:#}")}],
                "isError": true
            }),
        })
    }
}

type RpcResult = Result<Value, (i64, &'static str)>;

struct PendingRequest {
    cancelled: AtomicBool,
    cancellable: bool,
}

#[derive(Default)]
struct PendingRequests {
    requests: HashMap<String, Arc<PendingRequest>>,
    failure: Option<InputFailure>,
}

struct InputFailure {
    message: String,
    response: Option<Value>,
}

enum Incoming {
    Message {
        message: Value,
        request: Option<(String, Arc<PendingRequest>)>,
    },
    Response(Value),
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed))
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.as_number().is_some_and(|n| n.is_i64() || n.is_u64())
}

fn read_messages(
    mut input: impl BufRead,
    sender: Sender<Incoming>,
    pending: Arc<Mutex<PendingRequests>>,
) {
    loop {
        let mut line = Vec::new();
        let count = match input
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(count) => count,
            Err(error) => {
                fail_input(&pending, format!("Could not read MCP input: {error}"), None);
                return;
            }
        };
        if count == 0 {
            return;
        }
        if count > MAX_MESSAGE_BYTES {
            fail_input(
                &pending,
                "MCP message exceeds 1 MiB; closing the transport".to_owned(),
                Some(rpc_error(Value::Null, -32600, "MCP message exceeds 1 MiB")),
            );
            return;
        }
        let incoming = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => {
                let envelope_valid = message["jsonrpc"] == "2.0"
                    && message["method"].is_string()
                    && message.get("params").is_none_or(Value::is_object)
                    && message.get("id").is_none_or(valid_id);
                if envelope_valid
                    && message.get("id").is_none()
                    && message["method"] == "notifications/cancelled"
                {
                    if let Some(id) = message["params"].get("requestId").filter(|id| valid_id(id)) {
                        let pending = pending.lock().unwrap();
                        if let Some(request) = pending.requests.get(&id.to_string()) {
                            if request.cancellable {
                                request.cancelled.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    continue;
                }
                if let Some(id) = message.get("id").filter(|_| envelope_valid) {
                    let key = id.to_string();
                    let mut pending = pending.lock().unwrap();
                    if pending.requests.contains_key(&key) {
                        // A duplicate cannot be correlated unambiguously with
                        // the original request, which remains outstanding.
                        Incoming::Response(rpc_error(
                            Value::Null,
                            -32600,
                            "Duplicate outstanding request ID",
                        ))
                    } else if pending.requests.len() >= MAX_PENDING_REQUESTS {
                        Incoming::Response(rpc_error(
                            id.clone(),
                            -32000,
                            "Too many pending MCP requests; retry after earlier requests finish",
                        ))
                    } else {
                        let request = Arc::new(PendingRequest {
                            cancelled: AtomicBool::new(false),
                            cancellable: message["method"] != "initialize",
                        });
                        pending.requests.insert(key.clone(), Arc::clone(&request));
                        Incoming::Message {
                            message,
                            request: Some((key, request)),
                        }
                    }
                } else {
                    Incoming::Message {
                        message,
                        request: None,
                    }
                }
            }
            Err(_) => Incoming::Response(rpc_error(Value::Null, -32700, "Invalid JSON")),
        };
        match sender.try_send(incoming) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(_)) => {
                // Never block the reader behind a slow Studio command, because
                // doing so would also prevent it from observing cancellation.
                fail_input(
                    &pending,
                    "MCP input queue is full; closing the transport".to_owned(),
                    Some(rpc_error(Value::Null, -32000, "MCP input queue is full")),
                );
                return;
            }
        }
    }
}

fn fail_input(pending: &Mutex<PendingRequests>, message: String, response: Option<Value>) {
    let mut pending = pending.lock().unwrap();
    for request in pending.requests.values() {
        request.cancelled.store(true, Ordering::Relaxed);
    }
    pending.failure = Some(InputFailure { message, response });
}

fn check_input_failure(
    pending: &Mutex<PendingRequests>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    if let Some(failure) = pending.lock().unwrap().failure.take() {
        if let Some(response) = failure.response {
            write_response(output, &response)?;
        }
        bail!(failure.message);
    }
    Ok(())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn write_response(output: &mut impl Write, response: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *output, response)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn tools() -> Vec<Value> {
    let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let client = json!({
        "type": "object",
        "properties": {"clientId": {"type": "string", "format": "uuid"}},
        "required": ["clientId"],
        "additionalProperties": false
    });
    let ids = json!({
        "type": "array", "maxItems": MAX_INSTANCE_IDS,
        "items": {"type": "string", "pattern": "^[0-9a-fA-F]{32}$"}
    });
    let mut read_ids = ids.clone();
    read_ids["minItems"] = json!(1);
    let mut selection = client.clone();
    selection["properties"]["ids"] = ids;
    selection["required"] = json!(["clientId", "ids"]);
    vec![
        json!({"name": "rojo_project", "description": "Read the served project's name, session ID, root instance ID, and server information.", "inputSchema": empty}),
        json!({"name": "rojo_read_instances", "description": "Read served filesystem instances and their descendants by Rojo ID. Missing IDs are omitted. This is not the live Studio hierarchy.", "inputSchema": {"type": "object", "properties": {"ids": read_ids}, "required": ["ids"], "additionalProperties": false}}),
        json!({"name": "studio_list", "description": "List Studio clients connected to this server's opt-in Studio controls. Select an explicit clientId for subsequent commands.", "inputSchema": empty}),
        json!({"name": "studio_status", "description": "Request live status from a specific Studio client and wait for its response.", "inputSchema": client}),
        json!({"name": "studio_get_selection", "description": "Read the current selection in a specific Studio client. Unsynced selections may have no Rojo ID.", "inputSchema": client}),
        json!({"name": "studio_set_selection", "description": "Change a specific Studio client's selection to these synced Rojo IDs and wait for Studio's acknowledgement. An empty ids array clears the selection.", "inputSchema": selection}),
    ]
}

enum ToolCall {
    Project,
    Read(Vec<String>),
    StudioList,
    StudioCommand {
        client_id: String,
        command: &'static str,
        ids: Option<Vec<String>>,
    },
}

fn validate_tool(name: &str, args: &Map<String, Value>) -> Result<ToolCall, &'static str> {
    let allowed: &[&str] = match name {
        "rojo_project" | "studio_list" => &[],
        "rojo_read_instances" => &["ids"],
        "studio_status" | "studio_get_selection" => &["clientId"],
        "studio_set_selection" => &["clientId", "ids"],
        _ => return Err("Unknown tool"),
    };
    if args.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unknown tool argument");
    }
    match name {
        "rojo_project" => Ok(ToolCall::Project),
        "rojo_read_instances" => Ok(ToolCall::Read(parse_ids(args, false)?)),
        "studio_list" => Ok(ToolCall::StudioList),
        _ => {
            let client_id = args
                .get("clientId")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or("clientId must be a Studio client UUID from studio_list")?
                .to_string();
            let (command, ids) = match name {
                "studio_status" => ("getStatus", None),
                "studio_get_selection" => ("getSelection", None),
                "studio_set_selection" => ("setSelection", Some(parse_ids(args, true)?)),
                _ => unreachable!(),
            };
            Ok(ToolCall::StudioCommand {
                client_id,
                command,
                ids,
            })
        }
    }
}

fn parse_ids(args: &Map<String, Value>, allow_empty: bool) -> Result<Vec<String>, &'static str> {
    let ids = args
        .get("ids")
        .and_then(Value::as_array)
        .ok_or("ids must be an array of Rojo instance IDs")?;
    if ids.len() > MAX_INSTANCE_IDS || (!allow_empty && ids.is_empty()) {
        return Err("ids must contain at most 128 entries, and reads require at least one ID");
    }
    ids.iter()
        .map(|id| {
            let id = id.as_str().ok_or("Every instance ID must be a string")?;
            if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Every instance ID must contain exactly 32 hexadecimal characters");
            }
            Ref::from_str(id)
                .map(|id| id.to_string())
                .map_err(|_| "Invalid Rojo instance ID")
        })
        .collect()
}

struct Backend {
    origin: Url,
    client: Client,
}

impl Backend {
    fn new(server: &str) -> anyhow::Result<Self> {
        let origin = local_origin(server)?;
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(3))
            // The Studio broker itself waits up to ten seconds for an acknowledgement.
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { origin, client })
    }

    fn call(&self, call: ToolCall, cancelled: Option<&AtomicBool>) -> anyhow::Result<Value> {
        ensure!(!is_cancelled(cancelled), "MCP request was cancelled");
        match call {
            ToolCall::Project => self.project(),
            ToolCall::Read(ids) => {
                let body = self.request(&format!("/api/read/{}", ids.join(",")), None, false)?;
                let response: ReadResponse<'_> = deserialize_msgpack(&body)
                    .context("Rojo returned an invalid instance response")?;
                serde_json::to_value(response).context("Could not convert Rojo instances to JSON")
            }
            ToolCall::StudioList => {
                let body = self.request("/api/studio/clients", None, true)?;
                serde_json::from_slice(&body).context("Rojo returned invalid Studio client JSON")
            }
            ToolCall::StudioCommand {
                client_id,
                command,
                ids,
            } => {
                let project = self.project()?;
                let session_id = project
                    .get("sessionId")
                    .context("Rojo did not return a session ID")?;
                let _: SessionId = serde_json::from_value(session_id.clone())
                    .context("Rojo returned an invalid session ID")?;
                let mut request =
                    json!({"sessionId": session_id, "clientId": client_id, "command": command});
                if let Some(ids) = ids {
                    request["ids"] = json!(ids);
                }
                // Cancellation cannot undo a command already sent to Studio. The
                // transport suppresses its result, but stop before dispatch when
                // cancellation arrived while obtaining the current serve session.
                ensure!(!is_cancelled(cancelled), "MCP request was cancelled");
                let body = self.request("/api/studio/command", Some(&request), true)?;
                let response: Value = serde_json::from_slice(&body)
                    .context("Rojo returned invalid Studio command JSON")?;
                ensure!(response.get("sessionId") == Some(session_id), "Rojo restarted while executing the Studio command; refresh studio_list before retrying");
                ensure!(
                    response.get("result").is_some(),
                    "Rojo returned a Studio command response without a result"
                );
                Ok(response)
            }
        }
    }

    fn project(&self) -> anyhow::Result<Value> {
        let body = self.request("/api/rojo", None, false)?;
        deserialize_msgpack(&body).context("Rojo returned invalid project information")
    }

    fn request(&self, path: &str, body: Option<&Value>, is_json: bool) -> anyhow::Result<Vec<u8>> {
        let url = self.origin.join(path)?;
        let request = match body {
            Some(body) => self.client.post(url).json(body),
            None => self.client.get(url),
        };
        let response = request
            .send()
            .context("Could not contact the local Rojo server")?;
        let status = response.status();
        ensure!(
            response
                .content_length()
                .is_none_or(|len| len <= MAX_RESPONSE_BYTES as u64),
            "Rojo response exceeds 16 MiB; request a smaller instance subtree"
        );
        let mut data = Vec::new();
        response
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut data)
            .context("Could not read the Rojo response")?;
        ensure!(
            data.len() <= MAX_RESPONSE_BYTES,
            "Rojo response exceeds 16 MiB; request a smaller instance subtree"
        );
        if !status.is_success() {
            let error: Option<Value> = if is_json {
                serde_json::from_slice(&data).ok()
            } else {
                deserialize_msgpack(&data).ok()
            };
            let detail = error
                .as_ref()
                .and_then(|value| value.get("error").or_else(|| value.get("details")))
                .and_then(Value::as_str)
                .unwrap_or(
                    "Check that Rojo is running and Studio controls are enabled for Studio tools",
                );
            bail!("Rojo returned HTTP {status}: {detail}");
        }
        Ok(data)
    }
}

fn local_origin(server: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(server).context("--server must be a local HTTP origin")?;
    ensure!(url.scheme() == "http", "--server must use http");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "--server must not contain credentials"
    );
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "--server must be an origin without a path, query, or fragment"
    );
    let host = url.host_str().context("--server requires a host")?;
    if host == "localhost" {
        // Pin localhost to loopback so local DNS or hosts configuration cannot redirect it.
        url.set_host(Some("127.0.0.1"))?;
    } else {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let ip: IpAddr = host
            .parse()
            .context("--server must use localhost or a loopback IP address")?;
        let canonical = match ip {
            IpAddr::V6(ip) => ip
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(ip)),
            ip => ip,
        };
        ensure!(
            canonical.is_loopback(),
            "--server must use a loopback IP address"
        );
    }
    ensure!(
        url.port_or_known_default() != Some(0),
        "--server requires a nonzero port"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Cursor, net::TcpListener, thread};

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn initialize(server: &mut McpServer) -> Value {
        server
            .handle(request(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1"}
                }),
            ))
            .unwrap()
    }

    fn ready_server() -> McpServer {
        let mut server = McpServer::new("http://127.0.0.1:1").unwrap();
        initialize(&mut server);
        assert!(server
            .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        server
    }

    #[test]
    fn negotiates_version_and_requires_initialization_notification() {
        let mut server = McpServer::new("http://127.0.0.1:1").unwrap();
        let list = request(json!("list"), "tools/list", json!({}));
        assert_eq!(
            server.handle(list.clone()).unwrap()["error"]["code"],
            -32002
        );
        let initialized = initialize(&mut server);
        assert_eq!(initialized["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            server.handle(list.clone()).unwrap()["error"]["code"],
            -32002
        );
        assert!(server
            .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        let result = server.handle(list).unwrap();
        assert_eq!(result["id"], "list");
        assert_eq!(result["result"]["tools"].as_array().unwrap().len(), 6);
        assert_eq!(initialize(&mut server)["error"]["code"], -32600);
    }

    #[test]
    fn invalid_initialization_can_be_corrected() {
        let mut server = McpServer::new("http://127.0.0.1:1").unwrap();
        let bad = server
            .handle(request(json!(0), "initialize", json!({})))
            .unwrap();
        assert_eq!(bad["error"]["code"], -32602);
        assert!(initialize(&mut server).get("result").is_some());
    }

    #[test]
    fn notifications_produce_no_response_and_ping_preserves_ids() {
        let mut server = ready_server();
        for method in ["notifications/cancelled", "unknown", "tools/call"] {
            assert!(server
                .handle(json!({"jsonrpc": "2.0", "method": method}))
                .is_none());
        }
        let id = json!(u64::MAX);
        let response = server
            .handle(request(id.clone(), "ping", json!({})))
            .unwrap();
        assert_eq!(response, json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        assert_eq!(
            server
                .handle(request(json!(1), "unknown", json!({})))
                .unwrap()["error"]["code"],
            -32601
        );
    }

    #[test]
    fn rejects_invalid_envelopes() {
        let mut server = ready_server();
        for invalid in [
            json!([]),
            json!(null),
            json!({"jsonrpc":"1.0", "method":"ping", "id":1}),
            request(json!(null), "ping", json!({})),
            request(json!(1.5), "ping", json!({})),
            request(json!(true), "ping", json!({})),
            request(json!(1), "ping", json!([])),
        ] {
            assert_eq!(server.handle(invalid).unwrap()["error"]["code"], -32600);
        }
    }

    #[test]
    fn framing_recovers_from_parse_errors_and_exits_at_eof() {
        let mut server = ready_server();
        let input = b"not json\n{\"jsonrpc\":\"2.0\",\"method\":\"unknown\"}\n{\"jsonrpc\":\"2.0\",\"id\":\"ping\",\"method\":\"ping\"}\n";
        let mut output = Vec::new();
        server.run(Cursor::new(input), &mut output).unwrap();
        let responses: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[1]["id"], "ping");
    }

    #[test]
    fn bounds_input_before_allocating_an_arbitrarily_large_line() {
        let mut server = ready_server();
        let mut output = Vec::new();
        assert!(server
            .run(Cursor::new(vec![b' '; MAX_MESSAGE_BYTES + 20]), &mut output)
            .is_err());
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["error"]["code"], -32600);
    }

    #[test]
    fn validates_tool_arguments_before_contacting_server() {
        let mut server = ready_server();
        for (name, arguments) in [
            ("unknown", json!({})),
            ("rojo_project", json!({"extra": true})),
            ("rojo_read_instances", json!({"ids": []})),
            ("rojo_read_instances", json!({"ids": ["../rojo"]})),
            ("rojo_read_instances", json!({"ids": [1]})),
            ("studio_status", json!({})),
            ("studio_status", json!({"clientId": "bad"})),
            (
                "studio_set_selection",
                json!({"clientId": Uuid::new_v4().to_string()}),
            ),
        ] {
            let response = server
                .handle(request(
                    json!(4),
                    "tools/call",
                    json!({"name": name, "arguments": arguments}),
                ))
                .unwrap();
            assert_eq!(response["error"]["code"], -32602, "{name}: {response}");
        }
        let ids = json!({"ids": vec![Ref::new().to_string(); MAX_INSTANCE_IDS + 1]});
        assert!(validate_tool("rojo_read_instances", ids.as_object().unwrap()).is_err());
        let clear = json!({"clientId": Uuid::new_v4().to_string(), "ids": []});
        assert!(
            matches!(validate_tool("studio_set_selection", clear.as_object().unwrap()).unwrap(), ToolCall::StudioCommand { ids: Some(ids), .. } if ids.is_empty())
        );
    }

    #[test]
    fn accepts_only_local_http_origins() {
        for origin in [
            "http://127.0.0.1:34872",
            "http://[::1]:34872/",
            "http://[::ffff:127.0.0.1]:34872",
            "http://localhost:34872",
        ] {
            assert!(local_origin(origin).is_ok(), "{origin}");
        }
        assert_eq!(
            local_origin("http://localhost:34872").unwrap().host_str(),
            Some("127.0.0.1")
        );
        for origin in [
            "https://127.0.0.1",
            "http://example.com",
            "http://192.168.1.1",
            "http://0.0.0.0",
            "http://127.0.0.1/api",
            "http://127.0.0.1?x",
            "http://127.0.0.1#x",
            "http://name:pass@127.0.0.1",
            "http://127.0.0.1:0",
            "file:///tmp/rojo",
        ] {
            assert!(local_origin(origin).is_err(), "{origin}");
        }
    }

    fn http_server(
        responses: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut bytes = vec![0; length];
                input.read_exact(&mut bytes).unwrap();
                request.push_str(&String::from_utf8(bytes).unwrap());
                requests.push(request);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
            requests
        });
        (origin, handle)
    }

    #[test]
    fn studio_commands_use_current_session_and_wait_for_acknowledgement() {
        let session_id = SessionId::new();
        let client_id = Uuid::new_v4().to_string();
        let project = rmp_serde::to_vec_named(&json!({"sessionId": session_id})).unwrap();
        let result = json!({"sessionId": session_id, "result": {"selection": []}});
        let (origin, handle) = http_server(vec![
            ("200 OK", project),
            ("200 OK", serde_json::to_vec(&result).unwrap()),
        ]);
        let backend = Backend::new(&origin).unwrap();
        assert_eq!(
            backend
                .call(
                    ToolCall::StudioCommand {
                        client_id: client_id.clone(),
                        command: "setSelection",
                        ids: Some(vec![])
                    },
                    None
                )
                .unwrap(),
            result
        );
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /api/rojo "));
        assert!(requests[1].starts_with("POST /api/studio/command "));
        let body: Value =
            serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"sessionId": session_id, "clientId": client_id, "command": "setSelection", "ids": []})
        );
    }

    #[test]
    fn rejects_studio_response_from_another_serve_session() {
        let project = rmp_serde::to_vec_named(&json!({"sessionId": SessionId::new()})).unwrap();
        let result =
            serde_json::to_vec(&json!({"sessionId": SessionId::new(), "result": {}})).unwrap();
        let (origin, handle) = http_server(vec![("200 OK", project), ("200 OK", result)]);
        let error = Backend::new(&origin)
            .unwrap()
            .call(
                ToolCall::StudioCommand {
                    client_id: Uuid::new_v4().to_string(),
                    command: "getStatus",
                    ids: None,
                },
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("restarted"));
        handle.join().unwrap();
    }

    #[test]
    fn backend_http_errors_are_recoverable_tool_errors() {
        let (origin, handle) = http_server(vec![(
            "503 Service Unavailable",
            br#"{"error":"Studio disconnected"}"#.to_vec(),
        )]);
        let mut server = ready_server();
        server.backend = Backend::new(&origin).unwrap();
        let response = server
            .handle(request(
                json!(1),
                "tools/call",
                json!({"name": "studio_list"}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Studio disconnected"));
        handle.join().unwrap();
    }

    #[test]
    fn does_not_follow_http_redirects() {
        let (origin, handle) = http_server(vec![(
            "302 Found\r\nLocation: http://192.0.2.1/",
            Vec::new(),
        )]);
        let error = Backend::new(&origin).unwrap().project().unwrap_err();
        assert!(error.to_string().contains("302"));
        handle.join().unwrap();
    }

    #[test]
    fn invalid_backend_data_is_a_recoverable_tool_error() {
        let (origin, handle) = http_server(vec![("200 OK", b"not JSON".to_vec())]);
        let mut server = ready_server();
        server.backend = Backend::new(&origin).unwrap();
        let response = server
            .handle(request(
                json!(2),
                "tools/call",
                json!({"name": "studio_list"}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid Studio client JSON"));
        handle.join().unwrap();
    }

    #[test]
    fn rejects_large_backend_responses_without_reading_the_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut input = io::BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            )
            .unwrap();
        });
        let error = backend.project().unwrap_err();
        assert!(error.to_string().contains("exceeds 16 MiB"));
        handle.join().unwrap();
    }

    struct ChannelInput {
        chunks: crossbeam_channel::Receiver<Vec<u8>>,
        current: Cursor<Vec<u8>>,
        eof: Sender<()>,
    }

    impl Read for ChannelInput {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.current.position() as usize == self.current.get_ref().len() {
                match self.chunks.recv() {
                    Ok(bytes) => self.current = Cursor::new(bytes),
                    Err(_) => {
                        let _ = self.eof.try_send(());
                        return Ok(0);
                    }
                }
            }
            self.current.read(output)
        }
    }

    fn input_line(value: Value) -> Vec<u8> {
        format!("{value}\n").into_bytes()
    }

    fn cancellation_during_studio_request(cancel_queued_selection: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (blocked_sender, blocked) = bounded(1);
        let (release, released) = bounded(1);
        let finished = Arc::new(AtomicBool::new(false));
        let backend_finished = Arc::clone(&finished);
        let backend = thread::spawn(move || {
            let session_id = SessionId::new();
            let mut requests = Vec::new();
            while !backend_finished.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                request.push_str(&String::from_utf8(body).unwrap());
                let response = if request.starts_with("GET /api/rojo ") {
                    rmp_serde::to_vec_named(&json!({"sessionId": session_id})).unwrap()
                } else {
                    serde_json::to_vec(&json!({"sessionId": session_id, "result": {}})).unwrap()
                };
                requests.push(request);
                let block_at = if cancel_queued_selection { 2 } else { 1 };
                if requests.len() == block_at {
                    blocked_sender.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
            }
            requests
        });
        let (send_input, receive_input) = bounded(4);
        let (eof_sender, eof) = bounded(1);
        let input = io::BufReader::new(ChannelInput {
            chunks: receive_input,
            current: Cursor::new(Vec::new()),
            eof: eof_sender,
        });
        let mut server = ready_server();
        server.backend = Backend::new(&origin).unwrap();
        let transport_finished = Arc::clone(&finished);
        let transport = thread::spawn(move || {
            let mut output = Vec::new();
            let result = server.run(input, &mut output);
            transport_finished.store(true, Ordering::Relaxed);
            result.unwrap();
            output
        });
        let client_id = Uuid::new_v4().to_string();
        let selection = request(
            json!(2),
            "tools/call",
            json!({"name": "studio_set_selection", "arguments": {"clientId": client_id, "ids": []}}),
        );
        send_input
            .send(input_line(if cancel_queued_selection {
                request(
                    json!(1),
                    "tools/call",
                    json!({"name": "studio_status", "arguments": {"clientId": client_id}}),
                )
            } else {
                selection.clone()
            }))
            .unwrap();
        blocked.recv_timeout(Duration::from_secs(3)).unwrap();
        if cancel_queued_selection {
            send_input.send(input_line(selection)).unwrap();
        }
        send_input.send(input_line(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}}))).unwrap();
        drop(send_input);
        // The blocked backend must not prevent the reader from consuming the
        // cancellation and EOF. Release it only once cancellation was processed.
        let reached_eof = eof.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        let output = transport.join().unwrap();
        let requests = backend.join().unwrap();
        assert!(reached_eof.is_ok());
        assert_eq!(requests.len(), if cancel_queued_selection { 2 } else { 1 });
        assert!(requests
            .iter()
            .all(|request| !request.contains("setSelection")));
        if cancel_queued_selection {
            let response: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(response["id"], 1);
        } else {
            assert!(output.is_empty());
        }
    }

    #[test]
    fn cancelled_queued_selection_never_reaches_backend() {
        cancellation_during_studio_request(true);
    }

    #[test]
    fn cancellation_during_session_lookup_prevents_studio_command() {
        cancellation_during_studio_request(false);
    }

    #[test]
    fn busy_queue_still_reads_cancellation_without_retaining_unknown_ids() {
        let pending = Arc::new(Mutex::new(PendingRequests::default()));
        let (sender, receiver) = bounded(MAX_QUEUED_MESSAGES);
        let mut input = Vec::new();
        for id in 0..=MAX_PENDING_REQUESTS {
            input.extend(input_line(request(json!(id), "ping", json!({}))));
        }
        for id in [0, 999] {
            input.extend(input_line(json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":id}})));
        }
        read_messages(Cursor::new(input), sender, Arc::clone(&pending));
        let pending = pending.lock().unwrap();
        assert_eq!(pending.requests.len(), MAX_PENDING_REQUESTS);
        assert!(pending.requests["0"].cancelled.load(Ordering::Relaxed));
        assert!(!pending.requests.contains_key("999"));
        let incoming: Vec<_> = receiver.try_iter().collect();
        assert_eq!(incoming.len(), MAX_PENDING_REQUESTS + 1);
        assert!(
            matches!(&incoming[MAX_PENDING_REQUESTS], Incoming::Response(value) if value["error"]["code"] == -32000)
        );
    }

    #[test]
    fn duplicate_outstanding_id_preserves_the_first_request() {
        let pending = Arc::new(Mutex::new(PendingRequests::default()));
        let (sender, receiver) = bounded(MAX_QUEUED_MESSAGES);
        let line = input_line(request(json!(7), "ping", json!({})));
        read_messages(
            Cursor::new([line.clone(), line].concat()),
            sender,
            Arc::clone(&pending),
        );
        assert_eq!(pending.lock().unwrap().requests.len(), 1);
        assert!(matches!(
            receiver.recv().unwrap(),
            Incoming::Message {
                request: Some(_),
                ..
            }
        ));
        assert!(
            matches!(receiver.recv().unwrap(), Incoming::Response(value) if value["error"]["code"] == -32600 && value["id"].is_null())
        );
    }

    #[test]
    fn output_error_returns_without_waiting_for_more_input() {
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (send_input, receive_input) = bounded(1);
        let (eof, _) = bounded(1);
        let input = io::BufReader::new(ChannelInput {
            chunks: receive_input,
            current: Cursor::new(Vec::new()),
            eof,
        });
        let (done, completed) = bounded(1);
        let transport = thread::spawn(move || {
            let result = ready_server().run(input, BrokenOutput);
            done.send(result.is_err()).unwrap();
        });
        send_input
            .send(input_line(request(json!(1), "ping", json!({}))))
            .unwrap();
        let result = completed.recv_timeout(Duration::from_secs(2));
        drop(send_input);
        transport.join().unwrap();
        assert!(result.unwrap());
    }
}
