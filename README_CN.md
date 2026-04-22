[English](README.md) | 中文

# mlink

> 面向设备的本地优先通信基础设施。

mlink 是同一个 room 内设备之间的"哑管道"。任意两台设备——无论走 Bluetooth、LAN 还是 IPC——都能绕开公网，可靠地交换字节。

## 愿景

mlink 面向开发者，提供一个传输原语。上层应用（比如 [mteam](https://github.com/zhuqingyv/mteam)）通过本地 HTTP API 跟 mlink 对话，完全不用关心底下跑的是 BLE 还是 TCP。mlink 只做一件事：把字节端到端地送到。

## 业务模型

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

- **Room** —— 6 位数字码。输入同一个码的设备加入同一个 room。
- **Node** —— 每台设备一个，用稳定的 `app_uuid` 标识。
- **Mesh** —— 同一个 room 内所有 node 两两互通。
- **Transport** —— BLE、TCP+mDNS、IPC，根据环境自动选择。

## 架构分层

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

## 数据流

一条消息从设备 1 的 App A 发往设备 2 的 App B：

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

## 快速上手

两台设备，命令对称。

```bash
# Mac A —— 创建 room
mlink 567892

# Mac B —— 加入 room
mlink join 567892
```

聊天模式：

```bash
# Mac A
mlink chat 567892

# Mac B
mlink join --chat 567892
```

TCP 模式（同一局域网）：

```bash
mlink --transport tcp chat 567892
```

## 传输通道支持

| Transport  | 适用场景                 | 状态     |
|------------|--------------------------|----------|
| BLE        | 无网络、近距离           | 稳定     |
| TCP + mDNS | 同一局域网               | 稳定     |
| IPC        | 同一主机、跨进程         | 稳定     |
| QUIC       | 跨网络                   | 规划中   |

## 安装

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.sh | sh

# 从源码安装
cargo install --path crates/mlink-cli
```

## HTTP API（规划中）

启动后，mlink 在 `localhost` 上暴露一套本地 REST API：

- `POST /room/create` —— 创建一个 room
- `POST /room/join` —— 加入已有的 room
- `POST /send` —— 向 room 发送一条消息
- `GET  /peers` —— 列出已连接的 peer
- `GET  /events` —— SSE 流，推送接收到的消息

## 特性

- **零配置** —— peer 之间自动发现。
- **多通道并行** —— BLE 和 TCP 同时跑，哪条先通用哪条。
- **哑管道** —— 只管送达，不掺业务语义。
- **Room 隔离** —— 不同 room 之间流量互不可见。
- **压缩** —— zstd 透明处理。
- **加密** —— AES-GCM（加固中）。

## 集成方式

把 mlink 集成进应用的步骤：

1. 启动 mlink 本地服务（独立二进制或以 sidecar 形式）。
2. 通过 `POST /room/join` 加入一个 room。
3. 用 `POST /send` 发消息，用 `GET /events`（SSE）收消息。

上层应用完全不用碰 BLE、mDNS 或 socket 代码。

## 不做的事

- 不向公网转发。
- 不处理业务逻辑。
- 不做账号和身份管理。
- 不做消息持久化。

## License

MIT
