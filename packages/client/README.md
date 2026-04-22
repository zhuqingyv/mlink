# mlink-client

WebSocket client SDK for the mlink daemon. Works in Node.js (>=18) and modern browsers.

## Install

```bash
npm install mlink-client
```

## Quick start

```js
const { MlinkClient } = require("mlink-client");

// Pick a random free port automatically (Node only):
const client = new MlinkClient();
await client.connect();
console.log("port:", client.port);

await client.join("123456");

client.on("message", (msg) => {
  console.log(msg.from, msg.payload);
});

await client.send("123456", { text: "hello" });
```

Or specify a port explicitly:

```js
const client = new MlinkClient({ port: 7878 });
await client.connect();
```

## API

### `new MlinkClient(options?)`

| option                 | default     | notes                                                                                                 |
| ---------------------- | ----------- | ----------------------------------------------------------------------------------------------------- |
| `port`                 | *auto*      | Daemon port. When omitted, SDK probes a free port via `net.createServer().listen(0)` on connect (Node only). |
| `clientName`           | `undefined` | Sent in the `hello` frame; daemon logs it.                                                            |
| `pingIntervalMs`       | `30000`     | `0` disables heartbeat.                                                                               |
| `requestTimeoutMs`     | `10000`     | Reject timer for `join` / `leave` / `send`.                                                           |
| `autoReconnect`        | `true`      | Exponential backoff 1/2/4/8…s capped at the next knob.                                                |
| `maxReconnectDelayMs`  | `30000`     | Upper cap on backoff.                                                                                 |

Connection URL is always `ws://127.0.0.1:${port}/ws`.

### Methods

- `connect(): Promise<void>` — resolves once the daemon sends `ready`.
- `disconnect(): void` — closes the socket and stops reconnection.
- `join(room): Promise<void>` — 6-digit room code; resolves on `ack`.
- `leave(room): Promise<void>`
- `send(room, payload, to?): Promise<void>` — `to` addresses a single peer by `app_uuid`; omit to broadcast to every connected peer.
- `peers(room): Peer[]` — cached peer list for a room (refreshed via `room_state`).

### Properties

- `appUuid: string` — this daemon's app_uuid (available after `ready`).
- `port: number` — daemon port in use. Equals the `port` option when supplied; otherwise `0` until the first `connect()` probes a free port, then stable.
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

In browsers you must pass `port` explicitly — automatic port probing uses Node's `net` module and is not available:

```js
const client = new MlinkClient({ port: 7878 });
```

The native `WebSocket` global is used automatically; the `ws` dependency is Node-only.
