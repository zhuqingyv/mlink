# mlink

> Local-first communication infrastructure for devices.

mlink is a dumb pipe between devices on the same room. Any two devices — over Bluetooth, LAN, or IPC — can reliably exchange bytes without the public internet.

## Vision

mlink is a developer-facing transport primitive. Upper-layer applications (like [mteam](https://github.com/zhuqingyv/mteam)) talk to mlink through a local HTTP API and never have to care whether the bytes ride BLE or TCP. mlink only cares about delivering bytes, end to end, reliably.

## Business Model

```
            Room "567892"  (6-digit code, shared across devices)
    ┌─────────────────────────────────────────────────────────┐
    │                                                         │
    │   ┌────────┐          ┌────────┐          ┌────────┐    │
    │   │ Node A │ ◄──────► │ Node B │ ◄──────► │ Node C │    │
    │   │ uuid=a │          │ uuid=b │          │ uuid=c │    │
    │   └────────┘          └────────┘          └────────┘    │
    │        ▲                   ▲                   ▲        │
    │        └───────────────────┴───────────────────┘        │
    │                   full-mesh inside room                 │
    │                                                         │
    └─────────────────────────────────────────────────────────┘
              Transport auto-selected: BLE / TCP / IPC
```

- **Room** — a 6-digit numeric code. Any device entering the same code joins the same room.
- **Node** — one per device, identified by a stable `app_uuid`.
- **Mesh** — inside a room every node talks to every other node.
- **Transport** — BLE, TCP+mDNS, or IPC, chosen automatically based on environment.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  Application Layer                                           │
│  HTTP API on localhost  (mteam, other apps)      [planned]   │
├──────────────────────────────────────────────────────────────┤
│  API Layer                                                   │
│  message  │  rpc  │  stream  │  pubsub                       │
├──────────────────────────────────────────────────────────────┤
│  Core Layer                                                  │
│  Node  │  Room  │  Scanner  │  Connection  │  Peer           │
├──────────────────────────────────────────────────────────────┤
│  Protocol Layer                                              │
│  Frame codec  │  zstd  │  AES-GCM  │  MessagePack            │
├──────────────────────────────────────────────────────────────┤
│  Transport Layer                                             │
│  BLE          │  TCP + mDNS     │  IPC                       │
└──────────────────────────────────────────────────────────────┘
```

## Data Flow

A message from App A on device 1 to App B on device 2:

```
  App A                                                 App B
    │                                                     ▲
    │ POST /send {room, payload}                          │ SSE /events
    ▼                                                     │
┌─────────┐                                         ┌─────────┐
│ mlink 1 │                                         │ mlink 2 │
└─────────┘                                         └─────────┘
    │                                                     ▲
    │  MessagePack ─► zstd ─► AES ─► Frame                │
    ▼                                                     │
  Transport (BLE / TCP / IPC) ──────────────────────►  Transport
                                                          │
                                      Frame ─► AES ─► zstd ─► MessagePack
```

## Quick Start

Two devices, symmetric commands.

```bash
# Mac A — create room
mlink 567892

# Mac B — join room
mlink join 567892
```

Chat mode:

```bash
# Mac A
mlink chat 567892

# Mac B
mlink join --chat 567892
```

TCP mode (same LAN):

```bash
mlink --transport tcp chat 567892
```

## Transport Support

| Transport   | Use case                            | Status      |
|-------------|-------------------------------------|-------------|
| BLE         | No network, close range             | Stable      |
| TCP + mDNS  | Same LAN                            | Stable      |
| IPC         | Same host, cross-process            | Stable      |
| QUIC        | Across networks                     | Planned     |

## Install

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.sh | sh

# From source
cargo install --path crates/mlink-cli
```

## HTTP API (planned)

Once launched, mlink exposes a local REST API on `localhost`:

- `POST /room/create` — create a room
- `POST /room/join` — join an existing room
- `POST /send` — send a message to the room
- `GET  /peers` — list connected peers
- `GET  /events` — server-sent events stream of inbound messages

## Features

- **Zero config** — peers discover each other automatically.
- **Multi-transport** — BLE and TCP in parallel, best channel wins.
- **Dumb pipe** — delivery only, no business semantics.
- **Room isolation** — rooms never see each other's traffic.
- **Compression** — zstd applied transparently.
- **Encryption** — AES-GCM (hardening in progress).

## Integration

To integrate mlink into an application:

1. Launch the mlink local service (binary or sidecar).
2. Join a room via `POST /room/join`.
3. Send messages via `POST /send`; receive via `GET /events` (SSE).

The upper-layer app never touches BLE, mDNS, or socket code.

## Non-goals

- No forwarding to the public internet.
- No business logic.
- No account or identity management.
- No message persistence.

## License

MIT
