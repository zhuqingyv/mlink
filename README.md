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

All frames are JSON objects with a common envelope: `{ "v": 1, "id"?: string, "type": string, "payload": object }`. `v` must equal `1` (frames with a different version are rejected with `bad_version`). Requests may include a client-generated string `id`; the daemon echoes it back on the matching `ack` or `error` so callers can correlate request/response on a multiplexed socket. Max frame size: **1 MB** (oversize frames rejected with `payload_too_large`). Frames are UTF-8 text only — binary frames are not accepted. `room` codes are always **exactly 6 ASCII digits** (`/^\d{6}$/`); any other string yields `bad_room`.

### Client to daemon (5 types)

| type     | payload                             | purpose                                                                                                                                                    |
|----------|-------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `hello`  | `{ client_name?: string }`          | Handshake. Currently informational (logged for debugging); the daemon accepts any client. Send it as the first frame for forward compatibility.            |
| `join`   | `{ room: string }`                  | **Subscribe** to a room's message stream. The daemon adds the room to its persistent set if not already joined, then flushes any queued backlog. Idempotent. |
| `leave`  | `{ room: string }`                  | Unsubscribe. The daemon retires the room (and drops its peer fabric for that room) only after the **last** subscriber leaves.                              |
| `send`   | `{ room: string, payload: any, to?: string }` | Broadcast `payload` (any JSON value) to every peer in `room`, or unicast when `to` is a peer's `app_uuid`. You must be subscribed to `room` first. |
| `ping`   | `{}`                                | Application-level keepalive. Replies with `pong`.                                                                                                          |

### Daemon to client (6 types)

| type          | payload                                                                 | when                                                                                                 |
|---------------|-------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `ready`       | `{ app_uuid: string, version: string }`                                 | Sent **on connect**, before any `hello`. `app_uuid` is this daemon's stable UUIDv4; `version` is the daemon semver string (e.g. `"0.1.0"`). |
| `ack`         | `{ id: string, type: string }`                                          | Confirms a request that carried an `id`. `type` echoes the request type (`"hello"`, `"join"`, `"leave"`, `"send"`). |
| `error`       | `{ id?: string, code: string, message: string }`                        | Request failed or runtime error. `id` is present when the failure can be tied to a specific request. See error codes below. |
| `room_state`  | `{ room: string, peers: Array<{app_uuid: string, name: string}>, joined: boolean }` | Room membership changed (initial subscribe, peer connect / disconnect, leave). `joined: false` means the client is no longer subscribed. |
| `message`     | `{ room: string, from: string, payload: any, ts: number }`              | A peer in a subscribed room sent bytes. `from` is the sender's `app_uuid`; `ts` is unix time in **milliseconds**. Backlog and live messages share this type. |
| `pong`        | `{}`                                                                    | Response to `ping`.                                                                                  |

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

The `mlink` npm package (`packages/client`) is a thin wrapper over the WebSocket protocol above. It handles framing, request/ack correlation, heartbeat, and auto-reconnect with exponential backoff; the daemon still does all the transport work. Works in Node ≥ 18 and in browsers (when an explicit `port` is passed).

### Install

```bash
# From source (no published npm release yet)
cd packages/client && npm run setup    # installs deps + builds dist/

# Then in any consumer project
npm link mlink
```

### Complete working example

```typescript
import { MlinkClient, Message, RoomState, MlinkError } from 'mlink'

async function main() {
  // Port resolution:
  //   - new MlinkClient({ port: 51823 })  → connect to ws://127.0.0.1:51823/ws
  //   - new MlinkClient()                 → Node only; probes a random free port
  //                                         via net.createServer().listen(0).
  //                                         Read ~/.mlink/daemon.json yourself
  //                                         if you want the running daemon's port.
  const client = new MlinkClient({
    port: 51823,
    clientName: 'my-app',      // optional; logged by daemon, no auth semantics
    pingIntervalMs: 30_000,    // default 30s; set 0 to disable heartbeat
    requestTimeoutMs: 10_000,  // default 10s; join/leave/send reject if no ack
    autoReconnect: true,       // default true; 1/2/4/8/16/32s backoff, capped
    maxReconnectDelayMs: 30_000,
  })

  // Wire listeners BEFORE connect() so you don't miss the first `ready` frame.
  client.on('ready', (info) => {
    // info: { appUuid: string, version: string }
    console.log('daemon uuid:', info.appUuid, 'version:', info.version)
  })
  client.on('room_state', (state: RoomState) => {
    // state: { room: string, peers: Array<{app_uuid, name}>, joined: boolean }
    console.log(`room ${state.room} peers:`, state.peers.map(p => p.app_uuid))
  })
  client.on('message', (msg: Message) => {
    // msg: { room: string, from: string (sender app_uuid),
    //        payload: unknown (whatever JSON the sender passed),
    //        ts: number (unix epoch ms) }
    console.log(`[${msg.room}] ${msg.from}:`, msg.payload)
  })
  client.on('error', (err: MlinkError) => {
    // err.code ∈ {bad_json, bad_version, bad_type, bad_payload, bad_room,
    //             not_joined, send_failed, payload_too_large, socket_error}
    console.error(err.code, err.message)
  })
  client.on('disconnected', () => console.log('socket closed'))
  client.on('reconnecting', (attempt, delayMs) =>
    console.log(`reconnect #${attempt} in ${delayMs}ms`))

  // Open the socket and wait for the daemon's `ready` frame.
  await client.connect()

  // Subscribe, then send. Both reject on `error` frame or after requestTimeoutMs.
  await client.join('567892')                              // 6 ASCII digits
  await client.send('567892', { text: 'hello everyone' })  // broadcast
  await client.send('567892', { text: 'hi you' }, '<peer-app-uuid>')  // unicast

  // Read current membership synchronously without re-fetching:
  console.log('rooms:', client.rooms)           // string[]
  console.log('peers:', client.peers('567892')) // Array<{app_uuid, name}>
  console.log('me:', client.appUuid)            // daemon app_uuid (set after `ready`)

  // Tear down.
  await client.leave('567892')
  client.disconnect()   // stops reconnect, closes socket; safe to call twice
}

main().catch(console.error)
```

### API reference

**Constructor** — `new MlinkClient(options?: ClientOptions)`

| option                 | type      | default   | meaning                                                                 |
|------------------------|-----------|-----------|-------------------------------------------------------------------------|
| `port`                 | `number`  | *probed*  | Daemon port. Omit in Node to probe a random free port; required in browser. |
| `clientName`           | `string`  | *unset*   | Sent in the `hello` frame, logged by daemon. No auth effect.            |
| `pingIntervalMs`       | `number`  | `30000`   | Heartbeat interval. `0` disables the ping timer.                        |
| `requestTimeoutMs`     | `number`  | `10000`   | How long `join` / `leave` / `send` wait for `ack` before rejecting.     |
| `autoReconnect`        | `boolean` | `true`    | Reconnect with `1/2/4/8/16/32s` exponential backoff on unexpected close. |
| `maxReconnectDelayMs`  | `number`  | `30000`   | Cap for the backoff delay.                                              |

**Methods**

| signature                                                    | returns            | notes                                                                                   |
|--------------------------------------------------------------|--------------------|-----------------------------------------------------------------------------------------|
| `connect()`                                                  | `Promise<void>`    | Resolves after the daemon sends `ready`. Calling while already connecting returns the in-flight promise. |
| `disconnect()`                                               | `void`             | Closes socket, cancels reconnect. In-flight request promises reject with `connection closed`. |
| `join(room: string)`                                         | `Promise<void>`    | Resolves on `ack`; rejects on `error` frame or timeout. `room` must match `/^\d{6}$/`.  |
| `leave(room: string)`                                        | `Promise<void>`    | Resolves on `ack`. Safe to call for a room you never joined (daemon just acks).         |
| `send(room: string, payload: unknown, to?: string)`          | `Promise<void>`    | `payload` is any JSON-serializable value. `to` = peer `app_uuid` for unicast; omit for broadcast. You must be `join`-ed. |
| `peers(room: string)`                                        | `Peer[]`           | Last known peer list for `room`. Empty if not joined or no peers.                       |
| `isConnected()`                                              | `boolean`          | `true` iff underlying WS is `OPEN`.                                                     |

**Readonly properties**

| property           | type       | meaning                                                                |
|--------------------|------------|------------------------------------------------------------------------|
| `port`             | `number`   | Resolved daemon port. `0` until first `connect()` if auto-probed.      |
| `appUuid`          | `string`   | Daemon UUID from the `ready` frame. Empty string until first `ready`.  |
| `rooms`            | `string[]` | Snapshot of subscribed rooms.                                          |

**Events** (all via standard `EventEmitter`, i.e. `.on` / `.once` / `.off`)

| event          | listener signature                           | fires                                                         |
|----------------|----------------------------------------------|---------------------------------------------------------------|
| `connected`    | `() => void`                                 | Underlying WS opened (before `ready`).                        |
| `ready`        | `(info: {appUuid, version}) => void`         | Daemon's `ready` frame received.                              |
| `room_state`   | `(state: RoomState) => void`                 | Any `room_state` frame (join/peer-change/leave).              |
| `message`      | `(msg: Message) => void`                     | Peer message (backlog + live).                                |
| `error`        | `(err: {code, message}) => void`             | Parse/socket/protocol error. Request-correlated errors reject the corresponding promise instead. |
| `disconnected` | `() => void`                                 | WS closed for any reason.                                     |
| `reconnecting` | `(attempt: number, delayMs: number) => void` | Auto-reconnect is about to retry.                             |

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
