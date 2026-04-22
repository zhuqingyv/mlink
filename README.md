English | [中文](README_CN.md)

# mlink

> Local-first device communication runtime.

## What is mlink?

mlink is a runtime that moves bytes between your devices without touching the public internet. Upper-layer applications — agents, knowledge bases, sync tools — connect to the local mlink daemon over a WebSocket and talk to peers across BLE or LAN. mlink is a dumb pipe: it delivers bytes, nothing more.

## Architecture

```
 ┌──────────┐   WebSocket    ┌──────────────┐    BLE / TCP    ┌──────────────┐   WebSocket    ┌──────────┐
 │  Your    │ ◄────────────► │ mlink daemon │ ◄─────────────► │ mlink daemon │ ◄────────────► │  Peer    │
 │  App     │  ws://127.0.0  │  (device A)  │                 │  (device B)  │                │  App     │
 └──────────┘   .0.1:<port>  └──────────────┘                 └──────────────┘                └──────────┘
```

Your app never touches radios, sockets, or discovery code. It speaks JSON over a local WebSocket; mlink handles everything below.

## Core Concepts

- **Room** — a 6-digit numeric code. Any device joining the same code is in the same room. Inside a room every device talks to every other device (full mesh).
- **Transport** — BLE (no network required) or TCP + mDNS (same LAN). Auto-selected; no config.
- **WebSocket API** — one local daemon per device. Any language that speaks WebSocket can drive it. A single connection can subscribe to multiple rooms.
- **Dumb pipe** — mlink does not parse, persist, or interpret your payload. Semantics belong to your application.

## WebSocket Protocol (v1)

All messages are JSON objects with a common envelope: `{ "v": 1, "type": "...", "payload": {...} }`. Requests may include a client-generated `id` that is echoed back on the matching response.

### Client to daemon (5 types)

| type     | payload                                   | purpose                                                      |
|----------|-------------------------------------------|--------------------------------------------------------------|
| `hello`  | `{ client_name, client_version }`         | Handshake. Must be the first message on a new connection.   |
| `join`   | `{ room }`                                | Subscribe to a room. Call multiple times to join many rooms. |
| `leave`  | `{ room }`                                | Unsubscribe from a room.                                     |
| `send`   | `{ room, payload, to? }`                  | Broadcast to the room, or unicast when `to` is a peer uuid. |
| `ping`   | `{}`                                      | Application-level keepalive.                                 |

### Daemon to client (6 types)

| type          | payload                                        | when                                                    |
|---------------|------------------------------------------------|---------------------------------------------------------|
| `ready`       | `{ app_uuid, version, caps[] }`                | Response to `hello`. Tells you who this daemon is.      |
| `ack`         | `{ id, op }`                                   | Acknowledges a request that carried an `id`.            |
| `error`       | `{ id?, code, message }`                       | Request failed or runtime error.                        |
| `room_state`  | `{ room, peers[], joined }`                    | Room membership changed (join / leave / peer in / out). |
| `message`     | `{ room, from, payload, ts }`                  | A peer in a subscribed room sent you bytes.             |
| `pong`        | `{}`                                           | Response to `ping`.                                     |

## Quick Start

```bash
# Start the daemon on each device
mlink daemon
```

Then from any language with a WebSocket client:

```js
const ws = new WebSocket("ws://127.0.0.1:7421/ws")
ws.onopen = () => {
  ws.send(JSON.stringify({ v: 1, type: "hello", payload: { client_name: "my-app", client_version: "0.1" } }))
  ws.send(JSON.stringify({ v: 1, type: "join",  payload: { room: "567892" } }))
}
ws.onmessage = (e) => {
  const msg = JSON.parse(e.data)
  if (msg.type === "message") console.log(msg.from, msg.payload)
}
ws.send(JSON.stringify({ v: 1, type: "send", payload: { room: "567892", payload: { text: "hi" } } }))
```

Any device that joins `567892` on its local daemon will receive the message.

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
- No message persistence or offline queue.

## License

MIT
