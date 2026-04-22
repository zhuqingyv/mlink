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

## Core Concepts

- **Room** — a 6-digit numeric code. Any device joining the same code is in the same room (full mesh). Rooms live on the **daemon**, persisted across restarts, independent of any WS client.
- **WS client** — a *subscriber* to one or more rooms. Connecting, disconnecting, or crashing the client does **not** drop the room or the peer links.
- **Message queue** — per-room bounded FIFO (500 messages). Messages that arrive while no client is subscribed are queued; on subscribe/reconnect the backlog is flushed, oldest first.
- **Transport** — BLE (no network required) or TCP + mDNS (same LAN). Auto-selected; no config.
- **Dumb pipe** — mlink does not parse, persist (beyond the 500-slot queue), or interpret your payload.

## WebSocket Protocol (v1)

All messages are JSON objects with a common envelope: `{ "v": 1, "type": "...", "payload": {...} }`. Requests may include a client-generated `id` that is echoed back on the matching response.

### Client to daemon (5 types)

| type     | payload                                   | purpose                                                      |
|----------|-------------------------------------------|--------------------------------------------------------------|
| `hello`  | `{ client_name, client_version }`         | Handshake. First message on a new connection.                |
| `join`   | `{ room }`                                | **Subscribe** to a room's message stream. The daemon adds the room to its persistent set if not already joined, then flushes any queued backlog. Call multiple times to subscribe to many rooms. |
| `leave`  | `{ room }`                                | Unsubscribe. The daemon retires the room only after the last subscriber leaves. |
| `send`   | `{ room, payload, to? }`                  | Broadcast to the room, or unicast when `to` is a peer uuid. |
| `ping`   | `{}`                                      | Application-level keepalive.                                 |

### Daemon to client (6 types)

| type          | payload                                        | when                                                    |
|---------------|------------------------------------------------|---------------------------------------------------------|
| `ready`       | `{ app_uuid, version, caps[] }`                | Response to `hello`. Tells you who this daemon is.      |
| `ack`         | `{ id, op }`                                   | Acknowledges a request that carried an `id`.            |
| `error`       | `{ id?, code, message }`                       | Request failed or runtime error.                        |
| `room_state`  | `{ room, peers[], joined }`                    | Room membership changed (join / leave / peer in / out). |
| `message`     | `{ room, from, payload, ts }`                  | A peer in a subscribed room sent bytes. Backlog and live messages share this type. |
| `pong`        | `{}`                                           | Response to `ping`.                                     |

### Client lifecycle vs. peer connectivity

Peer links are owned by the daemon, not the WS client. If your app disconnects, the daemon keeps the BLE/TCP links up and buffers messages for every joined room (up to 500 per room, oldest evicted). When a client reconnects and re-subscribes with `join`, the buffered messages are flushed in order before new ones arrive.

## Quick Start

```bash
# One-liner: start the daemon and open the in-browser debug page.
mlink dev

# Or run the headless daemon (no browser):
mlink daemon
```

Both commands are single-instance — they refuse to start if another daemon is already live on this machine. The bound port and pid land in `~/.mlink/daemon.json`.

Then from any language with a WebSocket client:

```js
const ws = new WebSocket("ws://127.0.0.1:<port>/ws") // port from ~/.mlink/daemon.json
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
- No long-term message persistence. The per-room backlog is bounded at 500 and lives in memory only.

## License

MIT
