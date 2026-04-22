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

所有帧都是 JSON 对象，统一信封：`{ "v": 1, "id"?: string, "type": string, "payload": object }`。`v` 必须等于 `1`（其它版本以 `bad_version` 拒收）。请求可带客户端生成的字符串 `id`，daemon 会在对应的 `ack` / `error` 原样回传，便于在多路复用连接上关联请求/响应。单帧上限 **1 MB**（超出以 `payload_too_large` 拒收）。只接受 UTF-8 文本帧，不接受二进制帧。`room` 必须是**恰好 6 位 ASCII 数字**（`/^\d{6}$/`），其它字符串会被 `bad_room` 拒绝。

### 客户端 → daemon（5 种）

| type     | payload                                           | 用途                                                                                                                                   |
|----------|---------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `hello`  | `{ client_name?: string }`                        | 握手。目前仅供调试日志使用，daemon 接受任意客户端。作为新连接的第一帧发送，便于未来做鉴权扩展。                                    |
| `join`   | `{ room: string }`                                | **订阅**某房间的消息流。房间未加入时 daemon 会把它加入持久化集合，并把积压的消息推给你。幂等。                                      |
| `leave`  | `{ room: string }`                                | 退订。只有**最后一个**订阅者退订后，daemon 才会真正退房并断开该房间的对端连接。                                                     |
| `send`   | `{ room: string, payload: any, to?: string }`     | 将 `payload`（任意 JSON 值）广播到房间所有 peer；带 `to`（peer 的 `app_uuid`）即为点对点。`send` 前必须先 `join`。                 |
| `ping`   | `{}`                                              | 应用层心跳，回 `pong`。                                                                                                                |

### daemon → 客户端（6 种）

| type          | payload                                                                           | 触发时机                                                                                             |
|---------------|-----------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `ready`       | `{ app_uuid: string, version: string }`                                           | **连上即发**，先于任何 `hello`。`app_uuid` 是本 daemon 的稳定 UUIDv4；`version` 是 daemon semver 字符串（例如 `"0.1.0"`）。 |
| `ack`         | `{ id: string, type: string }`                                                    | 确认带 `id` 的请求已受理。`type` 回显请求类型（`"hello"`、`"join"`、`"leave"`、`"send"`）。          |
| `error`       | `{ id?: string, code: string, message: string }`                                  | 请求失败或运行时错误。能关联到具体请求时带 `id`。错误码见下表。                                      |
| `room_state`  | `{ room: string, peers: Array<{app_uuid: string, name: string}>, joined: boolean }` | 房间成员变化（首次订阅、peer 进出、退订）。`joined: false` 表示客户端已不再订阅。                    |
| `message`     | `{ room: string, from: string, payload: any, ts: number }`                        | 订阅房间内的 peer 发来消息。`from` 是发送者的 `app_uuid`；`ts` 是 unix 时间戳，单位**毫秒**。队列补推和实时消息共用这个类型。 |
| `pong`        | `{}`                                                                              | `ping` 的响应。                                                                                      |

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

## Node.js / TypeScript SDK

npm 包 `mlink`（`packages/client`）是上文 WebSocket 协议的薄封装，负责帧编解码、请求/ack 关联、心跳和指数退避自动重连；具体传输仍由 daemon 完成。支持 Node ≥ 18，以及浏览器（必须显式传 `port`）。

### 安装

```bash
# 从源码安装（尚未发布到 npm）
cd packages/client && npm run setup    # 装依赖 + 构建 dist/

# 然后在任意消费项目中
npm link mlink
```

### 完整可运行示例

```typescript
import { MlinkClient, Message, RoomState, MlinkError } from 'mlink'

async function main() {
  // 端口解析：
  //   - new MlinkClient({ port: 51823 })  → 连接 ws://127.0.0.1:51823/ws
  //   - new MlinkClient()                 → 仅 Node；通过
  //                                         net.createServer().listen(0)
  //                                         探测一个随机空闲端口。
  //                                         如需知道 daemon 当前端口，
  //                                         自行读 ~/.mlink/daemon.json。
  const client = new MlinkClient({
    port: 51823,
    clientName: 'my-app',      // 可选；仅 daemon 日志记录，无鉴权语义
    pingIntervalMs: 30_000,    // 默认 30s；设 0 关闭心跳
    requestTimeoutMs: 10_000,  // 默认 10s；join/leave/send 未在此内收到 ack 则 reject
    autoReconnect: true,       // 默认 true；1/2/4/8/16/32s 指数退避
    maxReconnectDelayMs: 30_000,
  })

  // 监听器必须在 connect() 之前挂好，否则会错过首个 `ready`。
  client.on('ready', (info) => {
    // info: { appUuid: string, version: string }
    console.log('daemon uuid:', info.appUuid, 'version:', info.version)
  })
  client.on('room_state', (state: RoomState) => {
    // state: { room: string, peers: Array<{app_uuid, name}>, joined: boolean }
    console.log(`room ${state.room} peers:`, state.peers.map(p => p.app_uuid))
  })
  client.on('message', (msg: Message) => {
    // msg: { room: string, from: string（发送者 app_uuid）,
    //        payload: unknown（发送方传入的任意 JSON 值）,
    //        ts: number（unix 毫秒时间戳） }
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

  // 打开 socket 并等待 daemon 的 `ready` 帧。
  await client.connect()

  // 先订阅，再发送。二者都会在 `error` 帧或 requestTimeoutMs 超时后 reject。
  await client.join('567892')                              // 6 位 ASCII 数字
  await client.send('567892', { text: 'hello everyone' })  // 广播
  await client.send('567892', { text: 'hi you' }, '<peer-app-uuid>')  // 点对点

  // 同步读取当前成员状态，无需重发请求：
  console.log('rooms:', client.rooms)           // string[]
  console.log('peers:', client.peers('567892')) // Array<{app_uuid, name}>
  console.log('me:', client.appUuid)            // daemon app_uuid（`ready` 后才有值）

  // 关闭。
  await client.leave('567892')
  client.disconnect()   // 停止重连、关闭 socket；幂等
}

main().catch(console.error)
```

### API 参考

**构造函数** — `new MlinkClient(options?: ClientOptions)`

| 选项                   | 类型      | 默认值    | 说明                                                                    |
|------------------------|-----------|-----------|-------------------------------------------------------------------------|
| `port`                 | `number`  | *探测*    | daemon 端口。Node 下省略则探测随机空闲端口；浏览器必须显式传。          |
| `clientName`           | `string`  | *未设*    | `hello` 帧的 `client_name`，daemon 仅记录日志，无鉴权作用。             |
| `pingIntervalMs`       | `number`  | `30000`   | 心跳间隔；`0` 关闭心跳定时器。                                           |
| `requestTimeoutMs`     | `number`  | `10000`   | `join` / `leave` / `send` 等待 `ack` 的超时时长。                       |
| `autoReconnect`        | `boolean` | `true`    | 意外断开后按 `1/2/4/8/16/32s` 指数退避重连。                             |
| `maxReconnectDelayMs`  | `number`  | `30000`   | 重连退避的上限。                                                        |

**方法**

| 签名                                                         | 返回值             | 说明                                                                                   |
|--------------------------------------------------------------|--------------------|---------------------------------------------------------------------------------------|
| `connect()`                                                  | `Promise<void>`    | daemon 发送 `ready` 后 resolve。已在连接中时返回同一个 in-flight Promise。              |
| `disconnect()`                                               | `void`             | 关 socket、取消重连。所有 in-flight 请求 Promise 会以 `connection closed` reject。     |
| `join(room: string)`                                         | `Promise<void>`    | `ack` 后 resolve；`error` 帧或超时 reject。`room` 须匹配 `/^\d{6}$/`。                 |
| `leave(room: string)`                                        | `Promise<void>`    | `ack` 后 resolve。对从未加入的房间调用也是安全的（daemon 直接 ack）。                  |
| `send(room: string, payload: unknown, to?: string)`          | `Promise<void>`    | `payload` 必须 JSON 可序列化。`to` = peer `app_uuid` 为点对点；省略即广播。必须先 `join`。 |
| `peers(room: string)`                                        | `Peer[]`           | 返回 `room` 最近一次 `room_state` 里的 peer 列表。未订阅或空时返回 `[]`。              |
| `isConnected()`                                              | `boolean`          | 底层 WS 处于 `OPEN` 时为 `true`。                                                      |

**只读属性**

| 属性               | 类型       | 说明                                                                   |
|--------------------|------------|------------------------------------------------------------------------|
| `port`             | `number`   | 已解析的 daemon 端口。未 `connect()` 前若为自动探测则为 `0`。           |
| `appUuid`          | `string`   | 来自 `ready` 帧的 daemon UUID。首次 `ready` 前是空字符串。              |
| `rooms`            | `string[]` | 当前已订阅房间的快照。                                                  |

**事件**（标准 `EventEmitter`，用 `.on` / `.once` / `.off`）

| 事件           | 监听器签名                                   | 触发时机                                                      |
|----------------|----------------------------------------------|---------------------------------------------------------------|
| `connected`    | `() => void`                                 | 底层 WS 已连上（`ready` 之前）。                              |
| `ready`        | `(info: {appUuid, version}) => void`         | 收到 daemon 的 `ready` 帧。                                   |
| `room_state`   | `(state: RoomState) => void`                 | 任何 `room_state` 帧（订阅/peer 变化/退订）。                 |
| `message`      | `(msg: Message) => void`                     | peer 消息（含队列补推和实时消息）。                           |
| `error`        | `(err: {code, message}) => void`             | 解析/socket/协议错误。能关联到具体请求的错误会通过 reject 对应 Promise 返回，而非该事件。 |
| `disconnected` | `() => void`                                 | WS 因任何原因关闭。                                           |
| `reconnecting` | `(attempt: number, delayMs: number) => void` | 准备触发下一次自动重连。                                      |

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
