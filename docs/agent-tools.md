# Agent access to Rojo and Roblox Studio

`rojo mcp` is a stdio MCP server for an existing local Rojo serve session. It exposes the filesystem-backed project and, with an updated connected plugin, live Studio status and selection controls. It supports MCP protocol revision `2024-11-05` and negotiates that revision during initialization.

## Start a development session

Build this checkout, including its plugin dependencies:

```sh
git submodule update --init --recursive
cargo build --locked
```

On the Windows or macOS machine running Studio, install the plugin embedded in this build:

```sh
./target/debug/rojo plugin install
```

This replaces the existing Rojo-managed local plugin. Disable other Rojo plugin copies so the intended build handles the connection. Linux can build the plugin but cannot run Roblox Studio.

Serve your project's `.project.json` file with Studio controls enabled:

```sh
./target/debug/rojo serve /path/to/default.project.json --enable-studio-controls
```

Connect Studio using the Rojo plugin as usual and complete the initial sync. The plugin registers for agent controls only after synchronization succeeds. Existing plugins can still sync, but will not appear in the Studio client list.

Configure your MCP client with the absolute path to the newly built binary:

```json
{
  "mcpServers": {
    "rojo": {
      "command": "/absolute/path/to/ajo/target/debug/rojo",
      "args": ["mcp", "--server", "http://127.0.0.1:34872"]
    }
  }
}
```

On Windows use the path to `target/debug/rojo.exe`. The MCP process connects to the running server; it does not start one. Standard output contains only newline-delimited JSON-RPC messages. Diagnostics go to standard error.

The MCP adapter accepts a loopback HTTP origin, bypasses HTTP proxies, and does not follow redirects. `--enable-studio-controls` also requires a loopback server bind. Agents on another machine need a private connection that presents the server as a local endpoint; this implementation does not provision tunnels or expose an unauthenticated public control server.

## Available tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `rojo_project` | `{}` | Project name, server/session information, and root instance ID. |
| `rojo_read_instances` | `{"ids":["<Rojo instance ID>"]}` | Served instances and their descendants. Missing IDs are omitted. |
| `studio_list` | `{}` | Registered Studio clients with `clientId`, place ID, game ID, and place name at connection time. |
| `studio_status` | `{"clientId":"<client UUID>"}` | Current place/game information and Studio edit/run state. |
| `studio_get_selection` | `{"clientId":"<client UUID>"}` | Live selected instances with name, class, path, and Rojo ID when mapped. |
| `studio_set_selection` | `{"clientId":"<client UUID>","ids":["<Rojo instance ID>"]}` | Changes selection, then returns the selection read back from Studio. An empty array clears it. |

Start with `rojo_project` and `rojo_read_instances` to discover project instance IDs. Use `studio_list` to select a specific open Studio before invoking its tools. Client identities change on reconnect; stale identities fail instead of selecting another Studio. Read requests and selections are limited to 128 IDs. A Studio selection larger than 128 instances returns an error rather than a truncated result.

Filesystem instance reads describe Rojo's served project, not arbitrary live Studio changes. Studio selection may include objects outside that project; those entries have no Rojo ID. Setting selection requires every ID to resolve to a live mapped object, and validates the whole request before changing anything.

Studio commands wait for an acknowledgement from the targeted plugin. A disconnected client fails pending requests. An unresponsive client times out after ten seconds. A timeout means execution is uncertain: inspect the current selection before retrying a mutation. Queued requests that expire before being sent are discarded; commands are never automatically replayed after reconnect. A plugin that is applying sync changes rejects control commands until that batch finishes, so an acknowledged selection cannot be overwritten by an in-progress instance replacement.

MCP cancellation discards a queued call before dispatch and suppresses a cancelled call's response. Cancellation cannot undo a Studio command that has already been sent. Inspect live state before retrying such a mutation.

Project-read tools work without opting in to Studio controls. The Studio endpoints are disabled unless `--enable-studio-controls` is present. This flag allows local clients to inspect Studio status and selection and change selection; it does not enable arbitrary Luau execution, play/stop commands, or general instance/property editing.

## Protocol extension

Normal Rojo synchronization remains protocol version 5 and retains its MessagePack packets. A server started with controls enabled advertises `studioControls: true` in `/api/rojo`. Only a capable plugin sends a JSON `studioHello` packet over the existing sync WebSocket. The server gives that connection a unique client identity and directs JSON `studioCommand` packets to it. JSON `studioResult` replies carry the request ID and server session ID; replies cannot resolve requests belonging to another connection.

The local HTTP bridge has two endpoints:

- `GET /api/studio/clients` returns `{sessionId, clients}`.
- `POST /api/studio/command` accepts JSON `{sessionId, clientId, command, ids?}`. Commands are `getStatus`, `getSelection`, or `setSelection` (which requires `ids`). Success returns `{sessionId, result}` after the plugin replies; failures return an HTTP error and `{error}`.

The bridge bounds connected clients and pending requests. Existing Host/Origin checks still apply. Studio endpoints also require a loopback peer.

## Development checks

```sh
INSTA_UPDATE=no cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace
stylua --check plugin/src scripts/test-studio-controls.luau
lune run scripts/test-studio-controls.luau
./target/debug/rojo build plugin.project.json -o target/AgentRojo.rbxm
```

The Lune runner executes the Studio-control specs with mocked services. Rust end-to-end tests exercise the MCP process, real HTTP/WebSocket transport, command acknowledgements, disconnect handling, and concurrent filesystem sync. These checks do not replace testing the plugin in actual Studio. On a supported desktop, run `bash scripts/unit-test-plugin.sh` and verify that agent selection commands update the intended open place.

Continuous branch serving, GitHub PR management, Studio play/debug commands, and persistence of arbitrary Studio edits are separate capabilities. This change provides an initial MCP connection and an acknowledged Studio-command path for that work.
