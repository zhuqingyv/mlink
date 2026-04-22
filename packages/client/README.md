# @mlink/client

WebSocket client SDK for the mlink daemon. Works in Node.js (>=18) and modern browsers.

## Install

```bash
npm install @mlink/client
```

## Quick start

```js
const { MlinkClient } = require("@mlink/client");

const client = new MlinkClient(); // auto-discovers the daemon port
await client.connect();
await client.join("123456");

client.on("message", (msg) => {
  console.log(msg.from, msg.payload);
});

await client.send("123456", { text: "hello" });
```

## API

### `new MlinkClient(options?)`

| option                 | default                                     | notes                                                  |
| ---------------------- | ------------------------------------------- | ------------------------------------------------------ |
| `url`                  | `ws://127.0.0.1:<port>/ws` from `daemon.json` | Required in browsers.                                  |
| `daemonInfoPath`       | `~/.mlink/daemon.json`                      | Node only. Env `MLINK_DAEMON_FILE` also honoured.      |
| `clientName`           | `undefined`                                 | Sent in the `hello` frame; daemon logs it.             |
| `pingIntervalMs`       | `30000`                                     | `0` disables heartbeat.                                |
| `requestTimeoutMs`     | `10000`                                     | Reject timer for `join` / `leave` / `send`.            |
| `autoReconnect`        | `true`                                      | Exponential backoff 1/2/4/8…s capped at the next knob. |
| `maxReconnectDelayMs`  | `30000`                                     | Upper cap on backoff.                                  |

### Methods

- `connect(): Promise<void>` — resolves once the daemon sends `ready`.
- `disconnect(): void` — closes the socket and stops reconnection.
- `join(room): Promise<void>` — 6-digit room code; resolves on `ack`.
- `leave(room): Promise<void>`
- `send(room, payload, to?): Promise<void>` — `to` addresses a single peer by `app_uuid`; omit to broadcast to every connected peer.
- `peers(room): Peer[]` — cached peer list for a room (refreshed via `room_state`).

### Properties

- `appUuid: string` — this daemon's app_uuid (available after `ready`).
- `port: number` — daemon port the client is pointed at. Resolved from `daemon.json` or the `url` option at construction time.
- `rooms: string[]` — currently joined rooms.

### Events

| event          | payload                                                  |
| -------------- | -------------------------------------------------------- |
| `ready`        | `{ appUuid, version }`                                   |
| `message`      | `{ room, from, payload, ts }`                            |
| `room_state`   | `{ room, peers, joined }`                                |
| `error`        | `{ code, message }`                                      |
| `disconnected` | —                                                        |
| `reconnecting` | `(attempt, delayMs)`                                     |
| `connected`    | — fires when the socket is open, before `ready`          |

## Protocol

Implements mlink WS protocol v1. Envelope: `{"v":1,"id"?,"type":...,"payload":{...}}`. Client sends `hello` / `join` / `leave` / `send` / `ping`; daemon sends `ready` / `ack` / `error` / `room_state` / `message` / `pong`.

Requests carry an auto-incremented `id`; the matching `ack` resolves the returned promise and an `error` with the same id rejects it.

## Browser usage

In browsers you must pass `url` explicitly — file-system discovery is unavailable:

```js
const client = new MlinkClient({ url: "ws://127.0.0.1:7878/ws" });
```

The native `WebSocket` global is used automatically; the `ws` dependency is Node-only.
