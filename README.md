English | [中文](README_CN.md)

# mlink

> Local-first device communication runtime.

## What is mlink?

mlink is a runtime that moves bytes between your devices without touching the public internet. You start a long-lived daemon on each device; upper-layer applications — agents, knowledge bases, sync tools — connect to that daemon over a WebSocket and talk to peers across BLE or LAN. mlink is a dumb pipe: it delivers bytes, nothing more.

## Architecture

```
 ┌──────────┐   WebSocket    ┌──────────────┐    BLE / TCP    ┌──────────────┐   WebSocket    ┌──────────┐
 │  Your    │ ◄────────────► │ mlink daemon │ ◄─────────────► │ mlink daemon │ ◄────────────► │  Peer    │
 │  App     │   (subscribe)  │  (device A)  │   (peer link)   │  (device B)  │   (subscribe)  │  App     │
 └──────────┘                └──────────────┘                 └──────────────┘                └──────────┘
                              persistent bg svc               persistent bg svc
                              owns rooms + radios             owns rooms + radios
```

The daemon is a persistent background service. It owns the radios, the peer links, and the joined-room list (persisted at `~/.mlink/rooms.json`). WebSocket clients are **subscribers** — they come and go without affecting peer connectivity. Your app never touches sockets, BLE, or mDNS; it just speaks JSON to `ws://127.0.0.1:<port>/ws`.

Single-instance lock: on startup the daemon writes `~/.mlink/daemon.json` (`{port, pid}`) and refuses to boot if another live process already holds it. Clients read this file to discover the port.

## Core Concepts

- **Room** — a 6-digit numeric code. Any device joining the same code is in the same room (full mesh). Rooms live on the **daemon**, persisted across restarts, independent of any WS client.
- **WS client** — a *subscriber* to one or more rooms. Connecting, disconnecting, or crashing the client does **not** drop the room or the peer links.
- **Message queue** — per-room bounded FIFO (500 messages). Messages that arrive while no client is subscribed are queued; on subscribe/reconnect the backlog is flushed, oldest first, then live messages resume.
- **Transport** — BLE (no network required) or TCP + mDNS (same LAN). Auto-selected; no config.
- **Dumb pipe** — mlink does not parse, interpret, or long-term-persist your payload. The 500-slot queue lives in memory only.

## WebSocket Protocol (v1)

All messages are JSON objects with a common envelope: `{ "v": 1, "type": "...", "payload": {...} }`. Requests may include a client-generated `id` that is echoed back on the matching `ack` or `error`. Max frame size: **1 MB** (oversize frames are rejected with `payload_too_large`).

### Client to daemon (5 types)

| type     | payload                             | purpose                                                                                                                                                    |
|----------|-------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `hello`  | `{ client_name? }`                  | Handshake. Currently informational (logged for debugging); the daemon accepts any client. Send it as the first frame for forward compatibility.            |
| `join`   | `{ room }`                          | **Subscribe** to a room's message stream. The daemon adds the room to its persistent set if not already joined, then flushes any queued backlog. Idempotent. |
| `leave`  | `{ room }`                          | Unsubscribe. The daemon retires the room (and drops its peer fabric for that room) only after the **last** subscriber leaves.                              |
| `send`   | `{ room, payload, to? }`            | Broadcast to the room, or unicast when `to` is a peer's `app_uuid`. You must be subscribed to `room` first.                                                 |
| `ping`   | `{}`                                | Application-level keepalive. Replies with `pong`.                                                                                                          |

### Daemon to client (6 types)

| type          | payload                                        | when                                                                                                 |
|---------------|------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `ready`       | `{ app_uuid, version }`                        | Sent **on connect**, before any `hello`. Tells you who this daemon is and which protocol it speaks. |
| `ack`         | `{ id, type }`                                 | Confirms a request that carried an `id`. `type` echoes the request type (`"hello"`, `"join"`, …).   |
| `error`       | `{ id?, code, message }`                       | Request failed or runtime error. See error codes below.                                              |
| `room_state`  | `{ room, peers: [{app_uuid, name}], joined }`  | Room membership changed (initial subscribe, peer connect / disconnect, leave).                       |
| `message`     | `{ room, from, payload, ts }`                  | A peer in a subscribed room sent bytes. Backlog and live messages share this type.                  |
| `pong`        | `{}`                                           | Response to `ping`.                                                                                  |

### Error codes

`bad_json` · `bad_version` · `bad_type` · `bad_payload` · `bad_room` · `not_joined` · `send_failed` · `payload_too_large`

### Client lifecycle vs. peer connectivity

Peer links are owned by the daemon, not the WS client. If your app disconnects, the daemon keeps BLE/TCP links up and buffers messages for every joined room (up to 500 per room, oldest evicted). When a client reconnects and re-subscribes with `join`, the buffered messages are flushed in order before new ones arrive. Only an explicit `leave` from the last subscriber retires the room.

## Quick Start

```bash
# One-liner: start the daemon and open the in-browser debug page.
mlink dev

# Or run the headless daemon (no browser):
mlink daemon
```

Both are single-instance — they refuse to start if another daemon is already live on this machine. The bound port and pid land in `~/.mlink/daemon.json`:

```json
{ "port": 51823, "pid": 12345 }
```

Read the port from that file, then connect:

```js
const ws = new WebSocket("ws://127.0.0.1:51823/ws")

ws.onmessage = (e) => {
  const msg = JSON.parse(e.data)
  if (msg.type === "ready")      console.log("daemon uuid:", msg.payload.app_uuid)
  if (msg.type === "room_state") console.log("peers:", msg.payload.peers)
  if (msg.type === "message")    console.log(msg.payload.from, msg.payload.payload)
}

ws.onopen = () => {
  ws.send(JSON.stringify({ v: 1, type: "hello", payload: { client_name: "my-app" } }))
  ws.send(JSON.stringify({ v: 1, id: "j1", type: "join", payload: { room: "567892" } }))
}

// Broadcast to the room:
ws.send(JSON.stringify({ v: 1, type: "send", payload: { room: "567892", payload: { text: "hi" } } }))

// Or unicast to one peer by its app_uuid:
ws.send(JSON.stringify({ v: 1, type: "send", payload: { room: "567892", to: "<peer-uuid>", payload: { text: "hi you" } } }))
```

## Node.js / TypeScript SDK

```bash
# Local install (from source)
cd packages/client && npm run setup

# Then in any project
npm link mlink
```

```typescript
import { MlinkClient } from 'mlink'

const client = new MlinkClient()          // auto-detect daemon port
// or: new MlinkClient({ port: 8080 })    // explicit port

await client.connect()
console.log('daemon port:', client.port)

await client.join('567892')
client.on('message', (msg) => {
  console.log(msg.from, msg.payload)
})
await client.send('567892', { text: 'hello' })

// Events
client.on('room_state', (state) => console.log(state.peers))
client.on('error', (err) => console.error(err.code, err.message))
client.on('disconnected', () => console.log('lost connection'))

// Cleanup
await client.leave('567892')
client.disconnect()
```

### API

| Method | Description |
|--------|-------------|
| `new MlinkClient({ port? })` | Create client. Omit port to auto-detect from daemon.json |
| `connect()` | Connect to daemon WebSocket |
| `disconnect()` | Close connection |
| `join(room)` | Subscribe to room |
| `leave(room)` | Unsubscribe from room |
| `send(room, payload, to?)` | Send message (broadcast or unicast) |
| `client.port` | Daemon port |
| `client.appUuid` | Daemon's app UUID |
| `client.rooms` | Subscribed rooms |
| `client.peers(room)` | Peers in room |

## Transport Support

| Transport   | Use case                   | Status   |
|-------------|----------------------------|----------|
| BLE         | No network, nearby devices | Stable   |
| TCP + mDNS  | Same LAN                   | Stable   |
| QUIC        | Across networks            | Planned  |

## Install

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.sh | sh

# From source
cargo install --path crates/mlink-cli
```

## What mlink does NOT do

- No relay through the public internet.
- No business logic — payload semantics belong to your app.
- No accounts, identity, or authentication beyond the local-machine trust boundary.
- No long-term message persistence. The per-room backlog is bounded at 500 and lives in memory only.

## License

MIT
