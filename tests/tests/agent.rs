use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::{ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use hyper_tungstenite::tungstenite::{client, Message, WebSocket};
use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

use crate::rojo_test::{
    io_util::{KillOnDrop, ROJO_PATH},
    serve_util::deserialize_msgpack,
};

const TIMEOUT: Duration = Duration::from_secs(5);

struct AgentSession {
    _process: KillOnDrop,
    dir: TempDir,
    client: Client,
    port: u16,
    info: Value,
}

impl AgentSession {
    fn new(controls: bool) -> Self {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/Test.lua"), "return 1").unwrap();
        fs::write(
            dir.path().join("default.project.json"),
            r#"{"name":"AgentTest","tree":{"$path":"src"}}"#,
        )
        .unwrap();

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = Command::new(ROJO_PATH);
        command
            .arg("serve")
            .arg(dir.path())
            .args(["--port", &port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if controls {
            command.arg("--enable-studio-controls");
        }
        let mut process = KillOnDrop(command.spawn().unwrap());
        let client = Client::builder().timeout(TIMEOUT).build().unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let info = loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "Rojo exited before becoming ready"
            );
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/api/rojo"))
                .send()
            {
                let bytes = response.error_for_status().unwrap().bytes().unwrap();
                break deserialize_msgpack(&bytes).unwrap();
            }
            assert!(Instant::now() < deadline, "Rojo did not become ready");
            thread::sleep(Duration::from_millis(20));
        };

        Self {
            _process: process,
            dir,
            client,
            port,
            info,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn studio_clients(&self) -> Value {
        self.client
            .get(self.url("/api/studio/clients"))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap()
    }

    fn connect_studio(&self) -> (WebSocket<TcpStream>, Value) {
        self.connect_studio_at("/api/socket/0")
    }

    fn connect_plugin(&self) -> (WebSocket<TcpStream>, Value) {
        self.connect_studio_at("/api/studio/socket")
    }

    fn connect_studio_at(&self, path: &str) -> (WebSocket<TcpStream>, Value) {
        let tcp = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.port).parse().unwrap(),
            TIMEOUT,
        )
        .unwrap();
        tcp.set_read_timeout(Some(TIMEOUT)).unwrap();
        tcp.set_write_timeout(Some(TIMEOUT)).unwrap();
        let (mut socket, _) = client(format!("ws://127.0.0.1:{}{path}", self.port), tcp).unwrap();
        let existing = self.studio_clients()["clients"].as_array().unwrap().clone();
        socket
            .send(Message::Text(
                json!({
                    "sessionId": self.info["sessionId"],
                    "packetType": "studioHello",
                    "body": {"placeId": 123, "gameId": 456, "placeName": "Test"}
                })
                .to_string(),
            ))
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let clients = self.studio_clients();
            assert_eq!(clients["sessionId"], self.info["sessionId"]);
            for client in clients["clients"].as_array().unwrap() {
                if !existing
                    .iter()
                    .any(|old| old["clientId"] == client["clientId"])
                {
                    assert_eq!(client["placeId"], 123);
                    assert_eq!(client["gameId"], 456);
                    assert_eq!(client["placeName"], "Test");
                    return (socket, client["clientId"].clone());
                }
            }
            assert!(Instant::now() < deadline, "Studio registration timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn command(&self, client_id: &Value, command: &str) -> Value {
        json!({
            "sessionId": self.info["sessionId"],
            "clientId": client_id,
            "command": command
        })
    }

    fn send_command(&self, body: Value) -> Receiver<Response> {
        let client = self.client.clone();
        let url = self.url("/api/studio/command");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let response = client.post(url).json(&body).send().unwrap();
            let _ = sender.send(response);
        });
        receiver
    }

    fn reply(&self, socket: &mut WebSocket<TcpStream>, request: &Value, result: Value) {
        socket
            .send(Message::Text(
                json!({
                    "sessionId": self.info["sessionId"],
                    "packetType": "studioResult",
                    "body": {"requestId": request["body"]["requestId"], "result": result}
                })
                .to_string(),
            ))
            .unwrap();
    }
}

fn read_packet(socket: &mut WebSocket<TcpStream>, packet_type: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {packet_type}"
        );
        let packet: Value = match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            Message::Binary(bytes) => deserialize_msgpack(&bytes).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            message => panic!("Unexpected WebSocket message: {message:?}"),
        };
        if packet["packetType"] == packet_type {
            return packet;
        }
    }
}

#[test]
fn studio_commands_wait_for_acknowledgment_and_preserve_sync() {
    let session = AgentSession::new(true);
    assert_eq!(session.info["studioControls"], true);
    let (mut socket, client_id) = session.connect_studio();
    let root_id = session.info["rootInstanceId"].clone();

    for (command, result) in [
        ("getStatus", json!({"isRunning": false, "placeId": 123})),
        ("getSelection", json!({"ids": [root_id.clone()]})),
        ("setSelection", json!({"ids": [root_id.clone()]})),
    ] {
        let mut body = session.command(&client_id, command);
        if command == "setSelection" {
            body["ids"] = json!([root_id]);
        }
        let response = session.send_command(body);
        let request = read_packet(&mut socket, "studioCommand");
        assert_eq!(request["sessionId"], session.info["sessionId"]);
        assert_eq!(request["body"]["command"], command);
        assert!(request["body"]["requestId"].is_string());
        if command == "setSelection" {
            assert_eq!(request["body"]["ids"], json!([root_id]));
        }
        assert!(matches!(
            response.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        session.reply(&mut socket, &request, result.clone());
        let response: Value = response
            .recv_timeout(TIMEOUT)
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(
            response,
            json!({"sessionId": session.info["sessionId"], "result": result})
        );
    }

    fs::write(session.dir.path().join("src/Test.lua"), "return 2").unwrap();
    let update = read_packet(&mut socket, "messages");
    assert_eq!(update["sessionId"], session.info["sessionId"]);
    assert!(update["body"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["updated"]
                .as_array()
                .unwrap()
                .iter()
                .any(|instance| instance["changedProperties"]["Source"]["String"] == "return 2")
        }));
}

#[test]
fn disconnect_fails_pending_studio_commands_and_removes_client() {
    let session = AgentSession::new(true);
    let (mut socket, client_id) = session.connect_studio();
    let response = session.send_command(session.command(&client_id, "getStatus"));
    read_packet(&mut socket, "studioCommand");
    socket.close(None).unwrap();
    drop(socket);
    let response = response.recv_timeout(TIMEOUT).unwrap();
    assert!(response.status().is_server_error() || response.status().is_client_error());
    assert!(response.json::<Value>().unwrap()["error"].is_string());
    assert_eq!(session.studio_clients()["clients"], json!([]));
}

#[test]
fn independent_plugin_socket_controls_unsynced_clients_and_never_streams_sync() {
    let session = AgentSession::new(true);
    assert_eq!(session.info["studioPluginControls"], true);
    let (mut socket, client_id) = session.connect_plugin();

    fs::write(session.dir.path().join("src/Test.lua"), "return 42").unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let read: Value = deserialize_msgpack(
            &session
                .client
                .get(session.url(&format!(
                    "/api/read/{}",
                    session.info["rootInstanceId"].as_str().unwrap()
                )))
                .send()
                .unwrap()
                .bytes()
                .unwrap(),
        )
        .unwrap();
        if read["instances"]
            .as_object()
            .unwrap()
            .values()
            .any(|instance| instance["Properties"]["Source"]["String"] == "return 42")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Filesystem changes were not observed"
        );
        thread::sleep(Duration::from_millis(20));
    }
    for (command, arguments, result) in [
        (
            "getPluginState",
            json!({}),
            json!({"connection":{"status":"disconnected"}, "windowEnabled":true}),
        ),
        (
            "getPluginChanges",
            json!({"offset":5,"limit":10}),
            json!({"offset":5,"total":0,"changes":[]}),
        ),
        (
            "pluginAction",
            json!({"action":{"type":"setWindow","enabled":false}}),
            json!({"windowEnabled":false}),
        ),
        (
            "pluginAction",
            json!({"action":{"type":"disconnect"}}),
            json!({"connection":{"status":"disconnected"}}),
        ),
    ] {
        let mut body = session.command(&client_id, command);
        body.as_object_mut()
            .unwrap()
            .extend(arguments.as_object().unwrap().clone());
        let response = session.send_command(body.clone());
        let Message::Text(packet) = socket.read().unwrap() else {
            panic!("The plugin control socket received sync data");
        };
        let packet: Value = serde_json::from_str(&packet).unwrap();
        assert_eq!(packet["packetType"], "studioCommand");
        assert_eq!(packet["body"]["command"], command);
        for (key, value) in arguments.as_object().unwrap() {
            assert_eq!(&packet["body"][key], value);
        }
        assert!(matches!(
            response.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        session.reply(&mut socket, &packet, result.clone());
        let acknowledged: Value = response
            .recv_timeout(TIMEOUT)
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(acknowledged["result"], result);
    }
    // Plugin disconnect actions leave the independent control connection alive.
    assert_eq!(
        session.studio_clients()["clients"][0]["clientId"],
        client_id
    );
    let pending = session.send_command(session.command(&client_id, "getPluginState"));
    read_packet(&mut socket, "studioCommand");
    socket.close(None).unwrap();
    drop(socket);
    assert_eq!(pending.recv_timeout(TIMEOUT).unwrap().status(), 503);
    let (_new_socket, new_id) = session.connect_plugin();
    assert_ne!(new_id, client_id);
}

#[test]
fn independent_plugin_socket_requires_opt_in_and_closes_invalid_sessions() {
    let disabled = AgentSession::new(false);
    assert_ne!(disabled.info["studioPluginControls"], true);
    assert_eq!(
        disabled
            .client
            .get(disabled.url("/api/studio/socket"))
            .send()
            .unwrap()
            .status(),
        403
    );
    let session = AgentSession::new(true);
    assert_eq!(
        session
            .client
            .get(session.url("/api/studio/socket"))
            .send()
            .unwrap()
            .status(),
        400
    );
    let (mut socket, _) = session.connect_plugin();
    socket.send(Message::Text(json!({"sessionId":uuid::Uuid::new_v4(), "packetType":"studioHello", "body":{"placeId":123,"gameId":456,"placeName":"Wrong"}}).to_string())).unwrap();
    assert!(matches!(socket.read(), Ok(Message::Close(_))));
    let deadline = Instant::now() + TIMEOUT;
    while !session.studio_clients()["clients"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(
            Instant::now() < deadline,
            "Invalid control socket remained registered"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn studio_commands_reject_invalid_sessions_ids_and_disabled_controls() {
    let session = AgentSession::new(true);
    let (_socket, client_id) = session.connect_studio();
    let mut wrong_session = session.command(&client_id, "getStatus");
    wrong_session["sessionId"] = json!(uuid::Uuid::new_v4());
    let mut malformed_ids = session.command(&client_id, "setSelection");
    malformed_ids["ids"] = json!(["not-a-ref"]);
    let mut too_many_ids = session.command(&client_id, "setSelection");
    too_many_ids["ids"] = json!(vec![session.info["rootInstanceId"].clone(); 129]);
    for body in [wrong_session, malformed_ids, too_many_ids] {
        let response = session
            .client
            .post(session.url("/api/studio/command"))
            .json(&body)
            .send()
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(response.json::<Value>().unwrap()["error"].is_string());
    }

    let disabled = AgentSession::new(false);
    assert_ne!(disabled.info["studioControls"], true);
    assert!(disabled
        .client
        .get(disabled.url("/api/studio/clients"))
        .send()
        .unwrap()
        .status()
        .is_client_error());
    assert!(disabled
        .client
        .post(disabled.url("/api/studio/command"))
        .json(&disabled.command(&client_id, "getStatus"))
        .send()
        .unwrap()
        .status()
        .is_client_error());
}

struct McpClient {
    _process: KillOnDrop,
    stdin: ChildStdin,
    responses: Receiver<String>,
}

impl McpClient {
    fn new(session: &AgentSession) -> Self {
        let mut process = KillOnDrop(
            Command::new(ROJO_PATH)
                .args(["mcp", "--server", &session.url("")])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdin = process.0.stdin.take().unwrap();
        let stdout = process.0.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            _process: process,
            stdin,
            responses,
        }
    }

    fn send(&mut self, request: Value) {
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn receive(&self, id: u64) -> Value {
        let response: Value =
            serde_json::from_str(&self.responses.recv_timeout(TIMEOUT).unwrap()).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert!(
            response.get("error").is_none(),
            "MCP request failed: {response}"
        );
        response["result"].clone()
    }
}

#[test]
fn mcp_stdio_discovers_tools_and_routes_studio_commands() {
    let session = AgentSession::new(true);
    let (mut socket, client_id) = session.connect_plugin();
    let mut mcp = McpClient::new(&session);
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "Rojo integration test", "version": "1"}
        }
    }));
    let initialized = mcp.receive(1);
    assert_eq!(initialized["protocolVersion"], "2024-11-05");
    assert!(initialized["capabilities"]["tools"].is_object());
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools = mcp.receive(2);
    for name in [
        "rojo_project",
        "rojo_read_instances",
        "studio_list",
        "studio_status",
        "studio_get_selection",
        "studio_set_selection",
        "studio_plugin_state",
        "studio_plugin_changes",
        "studio_plugin_diff",
        "studio_plugin_action",
    ] {
        assert!(
            tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name),
            "Missing MCP tool {name}"
        );
    }

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "rojo_project", "arguments": {}}
    }));
    let project = mcp.receive(3);
    let project: Value =
        serde_json::from_str(project["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(project["sessionId"], session.info["sessionId"]);

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "rojo_read_instances", "arguments": {"ids": [session.info["rootInstanceId"]]}}
    }));
    let instances = mcp.receive(4);
    let instances: Value =
        serde_json::from_str(instances["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(instances["sessionId"], session.info["sessionId"]);
    assert!(instances["instances"]
        .as_object()
        .unwrap()
        .values()
        .any(|instance| instance["Name"] == "Test" && instance["ClassName"] == "ModuleScript"));

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "studio_list", "arguments": {}}
    }));
    let clients = mcp.receive(5);
    let clients: Value =
        serde_json::from_str(clients["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(clients["clients"][0]["clientId"], client_id);

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "studio_status", "arguments": {"clientId": client_id}}
    }));
    let request = read_packet(&mut socket, "studioCommand");
    assert_eq!(request["body"]["command"], "getStatus");
    session.reply(
        &mut socket,
        &request,
        json!({"isRunning": false, "placeId": 123}),
    );
    let status = mcp.receive(6);
    assert_ne!(status["isError"], true);
    let status: Value =
        serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["result"]["isRunning"], false);
    assert_eq!(status["result"]["placeId"], 123);

    for (id, tool, arguments, expected) in [
        (
            7,
            "studio_plugin_state",
            json!({}),
            json!({"command":"getPluginState"}),
        ),
        (
            8,
            "studio_plugin_changes",
            json!({"offset":3}),
            json!({"command":"getPluginChanges", "offset":3, "limit":50}),
        ),
        (
            9,
            "studio_plugin_action",
            json!({"action":{"type":"respondConfirmation", "confirmationId":"confirmation-2", "decision":"Accept"}}),
            json!({"command":"pluginAction", "action":{"type":"respondConfirmation", "confirmationId":"confirmation-2", "decision":"Accept"}}),
        ),
        (
            10,
            "studio_plugin_diff",
            json!({"id":"change-id", "property":"Source", "side":"old", "revision":1, "offset":16384}),
            json!({"command":"getPluginDiff", "id":"change-id", "property":"Source", "side":"old", "revision":1, "offset":16384, "limit":8192}),
        ),
    ] {
        let mut arguments = arguments;
        arguments["clientId"] = client_id.clone();
        mcp.send(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":tool, "arguments":arguments}}));
        let request = read_packet(&mut socket, "studioCommand");
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&request["body"][key], value);
        }
        session.reply(&mut socket, &request, json!({"acknowledged":true}));
        let result = mcp.receive(id);
        assert_ne!(result["isError"], true);
        let result: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(result["result"]["acknowledged"], true);
    }
}
