[English](README.md) | 中文

# mlink

> 面向本地设备的通信运行时（local-first device communication runtime）。

## mlink 是什么？

mlink 是一个运行时，负责在你的设备之间搬运字节，全程不经过公网。上层应用——agent、知识库、同步工具——通过 WebSocket 连到本机的 mlink daemon，由 daemon 走 BLE 或局域网把消息送到对端。mlink 只做一件事：送字节，是条哑管道。

## 架构

```
 ┌──────────┐   WebSocket    ┌──────────────┐    BLE / TCP    ┌──────────────┐   WebSocket    ┌──────────┐
 │ 你的应用 │ ◄────────────► │ mlink daemon │ ◄─────────────► │ mlink daemon │ ◄────────────► │ 对端应用 │
 │          │  ws://127.0.0  │  (设备 A)    │                 │   (设备 B)   │                │          │
 └──────────┘   .0.1:<port>  └──────────────┘                 └──────────────┘                └──────────┘
```

你的应用不碰蓝牙、socket、mDNS。它只通过本机 WebSocket 发 JSON，下面的事全由 mlink 处理。

## 核心概念

- **Room（房间）** —— 6 位数字码。输入同一个码的设备就在同一个房间。房间内所有设备两两互通（full mesh）。
- **Transport（传输通道）** —— BLE（无网络即可）或 TCP + mDNS（同局域网），自动选择，无需配置。
- **WebSocket API** —— 每台设备一个本地 daemon。任何支持 WebSocket 的语言都能接入；单条连接可同时订阅多个房间。
- **哑管道** —— mlink 不解析、不持久化、不解释你的 payload。业务语义归上层。

## WebSocket 协议（v1）

所有消息都是 JSON 对象，统一信封：`{ "v": 1, "type": "...", "payload": {...} }`。请求可带客户端生成的 `id`，对应响应会原样回传。

### 客户端 → daemon（5 种）

| type     | payload                                 | 用途                                                 |
|----------|-----------------------------------------|------------------------------------------------------|
| `hello`  | `{ client_name, client_version }`       | 握手，新连接的第一条消息。                          |
| `join`   | `{ room }`                              | 订阅房间，可多次调用订阅多个房间。                  |
| `leave`  | `{ room }`                              | 退订房间。                                           |
| `send`   | `{ room, payload, to? }`                | 房间广播；带 `to`（peer 的 app_uuid）即为点对点。  |
| `ping`   | `{}`                                    | 应用层心跳。                                         |

### daemon → 客户端（6 种）

| type          | payload                                     | 触发时机                                        |
|---------------|---------------------------------------------|-------------------------------------------------|
| `ready`       | `{ app_uuid, version, caps[] }`             | `hello` 的响应，告诉你这个 daemon 是谁。       |
| `ack`         | `{ id, op }`                                | 确认带 `id` 的请求已受理。                     |
| `error`       | `{ id?, code, message }`                    | 请求失败或运行时错误。                          |
| `room_state`  | `{ room, peers[], joined }`                 | 房间成员变化（join / leave / peer 进出）。    |
| `message`     | `{ room, from, payload, ts }`               | 订阅房间内的 peer 发来消息。                   |
| `pong`        | `{}`                                        | `ping` 的响应。                                 |

## 快速上手

```bash
# 每台设备起一个 daemon
mlink daemon
```

然后在任意语言里用 WebSocket 客户端接入：

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

其它加入 `567892` 的设备会在各自的 daemon 上收到这条消息。

## 传输通道支持

| Transport   | 适用场景               | 状态   |
|-------------|------------------------|--------|
| BLE         | 无网络，近距离         | 稳定   |
| TCP + mDNS  | 同一局域网             | 稳定   |
| QUIC        | 跨网络                 | 规划中 |

## 安装

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.sh | sh

# 从源码安装
cargo install --path crates/mlink-cli
```

## mlink 不做的事

- 不向公网转发。
- 不做业务逻辑，payload 含义归你自己定义。
- 除了"本机进程"这层信任边界，不做账号、身份、鉴权。
- 不做消息持久化或离线队列。

## License

MIT
