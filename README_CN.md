[English](README.md) | 中文

# mlink

> 面向本地设备的通信运行时（local-first device communication runtime）。

## mlink 是什么？

mlink 是一个运行时，负责在你的设备之间搬运字节，全程不经过公网。每台设备起一个常驻 daemon；上层应用——agent、知识库、同步工具——通过 WebSocket 连到 daemon，由 daemon 走 BLE 或局域网把消息送到对端。mlink 只做一件事：送字节，是条哑管道。

## 架构

```
 ┌──────────┐   WebSocket    ┌──────────────┐    BLE / TCP    ┌──────────────┐   WebSocket    ┌──────────┐
 │ 你的应用 │ ◄────────────► │ mlink daemon │ ◄─────────────► │ mlink daemon │ ◄────────────► │ 对端应用 │
 │          │    (订阅者)    │   (设备 A)   │    (设备互联)   │   (设备 B)   │    (订阅者)    │          │
 └──────────┘                └──────────────┘                 └──────────────┘                └──────────┘
                              常驻后台服务                     常驻后台服务
                              持有房间 + 传输通道              持有房间 + 传输通道
```

daemon 是常驻后台服务，它持有传输通道、对端连接，以及已加入的房间列表（持久化在 `~/.mlink/rooms.json`）。WebSocket 客户端只是**订阅者**——客户端来去、崩溃、断线都不影响 daemon 之间已经建立的对端连接。你的应用不碰 socket、BLE 或 mDNS，只要向 `ws://127.0.0.1:<port>/ws` 发 JSON。

单实例锁：daemon 启动时写 `~/.mlink/daemon.json`（`{port, pid}`），若已有活进程持有就拒绝启动。客户端从该文件读端口。

## 核心概念

- **Room（房间）** —— 6 位数字码。输入同一个码的设备就在同一个房间（full mesh）。房间归 **daemon** 所有，跨进程重启持久化，与任何 WS 客户端解耦。
- **WS 客户端** —— 某些房间的*订阅者*。连上、断开、崩溃都不会让 daemon 退房或断开对端连接。
- **消息队列** —— 每房间一条 FIFO，上限 500 条。没有订阅者在线时到达的消息进队列；客户端重连并 `join` 时，先按时间顺序把积压推完再推实时消息。
- **Transport（传输通道）** —— BLE（无网络即可）或 TCP + mDNS（同局域网），自动选择，无需配置。
- **哑管道** —— mlink 不解析、不解释、不做长期持久化。500 条队列只在内存里。

## WebSocket 协议（v1）

所有消息都是 JSON 对象，统一信封：`{ "v": 1, "type": "...", "payload": {...} }`。请求可带客户端生成的 `id`，对应的 `ack` / `error` 会原样回传。单帧上限 **1 MB**（超出以 `payload_too_large` 拒收）。

### 客户端 → daemon（5 种）

| type     | payload                         | 用途                                                                                                                                   |
|----------|---------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `hello`  | `{ client_name? }`              | 握手。目前仅供调试日志使用，daemon 接受任意客户端。作为新连接的第一帧发送，便于未来做鉴权扩展。                                    |
| `join`   | `{ room }`                      | **订阅**某房间的消息流。房间未加入时 daemon 会把它加入持久化集合，并把积压的消息推给你。幂等。                                      |
| `leave`  | `{ room }`                      | 退订。只有**最后一个**订阅者退订后，daemon 才会真正退房并断开该房间的对端连接。                                                     |
| `send`   | `{ room, payload, to? }`        | 房间广播；带 `to`（peer 的 `app_uuid`）即为点对点。`send` 前必须先 `join`。                                                          |
| `ping`   | `{}`                            | 应用层心跳，回 `pong`。                                                                                                                |

### daemon → 客户端（6 种）

| type          | payload                                        | 触发时机                                                                                             |
|---------------|------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `ready`       | `{ app_uuid, version }`                        | **连上即发**，先于任何 `hello`。告诉你这是哪台 daemon、协议版本。                                    |
| `ack`         | `{ id, type }`                                 | 确认带 `id` 的请求已受理。`type` 回显请求类型（`"hello"`、`"join"` 等）。                            |
| `error`       | `{ id?, code, message }`                       | 请求失败或运行时错误。错误码见下表。                                                                 |
| `room_state`  | `{ room, peers: [{app_uuid, name}], joined }`  | 房间成员变化（首次订阅、peer 进出、退订）。                                                          |
| `message`     | `{ room, from, payload, ts }`                  | 订阅房间内的 peer 发来消息。队列补推和实时消息共用这个类型。                                         |
| `pong`        | `{}`                                           | `ping` 的响应。                                                                                      |

### 错误码

`bad_json` · `bad_version` · `bad_type` · `bad_payload` · `bad_room` · `not_joined` · `send_failed` · `payload_too_large`

### 客户端生命周期 vs. 对端连接

对端连接归 daemon 管，不归 WS 客户端管。应用断开时 daemon 继续维持 BLE/TCP 链路，并把发给任一已加入房间的消息写入队列（每房间 500 条，超出淘汰最旧）。客户端重连并 `join` 时，先按顺序推完积压再推实时。只有最后一个订阅者**显式** `leave`，daemon 才会退房。

## 快速上手

```bash
# 一行启动：daemon + 自动打开网页调试页面
mlink dev

# 或只跑无头 daemon（不开浏览器）：
mlink daemon
```

两个命令都是单实例——本机已有 daemon 在跑时会拒绝启动。绑定端口和 pid 写在 `~/.mlink/daemon.json`：

```json
{ "port": 51823, "pid": 12345 }
```

从该文件读取端口后接入：

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

// 房间广播：
ws.send(JSON.stringify({ v: 1, type: "send", payload: { room: "567892", payload: { text: "hi" } } }))

// 点对点：
ws.send(JSON.stringify({ v: 1, type: "send", payload: { room: "567892", to: "<peer-uuid>", payload: { text: "hi you" } } }))
```

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
- 不做长期消息持久化。每房间 500 条的积压队列只在内存里。

## License

MIT
