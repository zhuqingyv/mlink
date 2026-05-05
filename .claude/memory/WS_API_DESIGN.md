# mlink WebSocket 接口设计（技术视角）

**作者**：ws-tech（技术架构师）
**日期**：2026-04-20
**范围**：首期只做 WebSocket，不做 HTTP REST，不改 CLI。
**基线**：基于 `HTTP_ARCH_EVAL.md` 的架构盘点；核心发现（Node 内部已全 `Arc<RwLock/Mutex>`、`broadcast::Receiver<NodeEvent>` 天然适配长连接）完全适用于 WS。

---

## 0. 一张图看完

```
┌─────────────────────── Machine A ──────────────────────┐
│                                                         │
│  Agent / KB / FileSync / …  (上层应用，不关心是谁)     │
│       │                                                 │
│       │  ws://127.0.0.1:<port>/v1/ws                    │
│       ▼                                                 │
│  ┌──────────────── mlink daemon ──────────────────┐    │
│  │  axum WS server                                 │    │
│  │     ├─ ClientSession #1  (订阅 roomA, roomB)   │    │
│  │     ├─ ClientSession #2  (订阅 roomC)          │    │
│  │     └─ ClientSession #N  …                     │    │
│  │                                                 │    │
│  │  Arc<DaemonState>                               │    │
│  │     ├─ Arc<Mutex<Node>>          ← mlink-core  │    │
│  │     ├─ Arc<Mutex<RoomManager>>                  │    │
│  │     ├─ Arc<RpcRegistry>, Arc<PendingRequests>  │    │
│  │     ├─ Arc<Mutex<PubSubManager>>               │    │
│  │     ├─ SessionManager  (搬家 CLI 主循环)       │    │
│  │     └─ broadcast::Sender<NodeEvent>            │    │
│  └─────────────────────┬───────────────────────────┘    │
│                        │                                │
│                    BLE / TCP                            │
└────────────────────────┼────────────────────────────────┘
                         │
                    ┌────▼────┐
                    │ Machine B (同结构) │
                    └─────────┘
```

关键心智：
- **daemon 只承载一个 Node**（BLE peripheral / mDNS / btleplug adapter 都是单例资源，不能多开）
- **WS 是机器内部 IPC**，不是机器之间的通路；机器之间还是 BLE/TCP
- **"多 room 订阅" 在 daemon 层实现**，不是 core 层；客户端一个 WS 连接可以同时 join 多个 room，daemon 负责为每个 room 维护 Node 状态集合

---

## 1. WS 消息协议（JSON）

### 1.1 统一信封

所有消息都走这个信封，便于版本控制和路由：

```jsonc
// 客户端 → mlink（Request / Command / Subscribe）
{
  "v": 1,                    // protocol version（目前固定 1，不匹配直接关连接）
  "id": "c-42",              // 客户端生成的 request id（Response/Error 会原样回传）
  "type": "room.join",       // 消息类型（见下表）
  "payload": { ... }         // 类型特定
}

// mlink → 客户端（Response / Event / Error）
{
  "v": 1,
  "id": "c-42",              // 回应哪个请求；Event 无 id（纯推送）
  "type": "room.joined",     // 或 "error" / "event.peer_connected" 等
  "payload": { ... }
}
```

- 客户端 id 是**客户端的事情**，daemon 不解释，只回声——让客户端自己关联请求和响应。
- `v` 用来做未来平滑升级；收到未知 v 立刻 `close(code=4400, reason="unsupported version")`。
- 不引入 RPC 调用 id 冲突：mlink 内的 `RpcRequest.request_id`（u16）与这里的 `id`（字符串）是两层，互不干扰。

### 1.2 客户端 → mlink 消息类型

| type                    | payload                                          | 行为                                                         |
| ----------------------- | ------------------------------------------------ | ------------------------------------------------------------ |
| `hello`                 | `{ client_name: "agent-x", client_version: "…" }` | 握手，daemon 回 `ready`（含 app_uuid、支持的能力位）。必须是第一条。 |
| `room.join`             | `{ code: "482193", encrypt?: true, name?: "…" }` | daemon 确保 Node 已 start、加入该 room 的 hash 集合。回 `room.joined`。 |
| `room.leave`            | `{ code: "482193" }`                             | 从 hash 集合移除；若为最后一个 room，daemon 维持 Node 运行但不再响应广播。 |
| `room.list`             | `{}`                                             | 回当前该 client 订阅的 room 列表。                           |
| `peers.list`            | `{ room?: "482193" }`                            | 返回指定 room（或所有）的 peer 列表。                        |
| `peers.scan`            | `{ timeout_ms?: 5000 }`                          | 触发一次主动扫描，同步返回发现的 peer 列表（不改变订阅）。   |
| `peers.disconnect`      | `{ peer_id: "…" }`                               | 主动断开某 peer。                                            |
| `message.send`          | `{ room: "…", peer_id?: "…", data: "<b64>", encoding: "base64" }` | `peer_id` 为空即广播到该 room。                              |
| `rpc.call`              | `{ peer_id: "…", method: "…", data: "<b64>", timeout_ms: 5000 }` | 发起 RPC，daemon 等 response 后回 `rpc.result`。             |
| `rpc.register`          | `{ method: "…" }`                                | 标记：本客户端愿意接收 `method` 的远端请求。daemon 把入站 Request 推送给该 client，等客户端回 `rpc.reply`。 |
| `rpc.reply`             | `{ request_id: "…", status: "ok"\|"error", data: "<b64>" }` | 客户端对 `event.rpc.request` 的响应。                        |
| `pubsub.subscribe`      | `{ topic: "…", peer_id: "…" }`                   | 订阅远端 peer 的话题。daemon 记录并推送后续 `event.pubsub.message`。 |
| `pubsub.unsubscribe`    | `{ topic: "…", peer_id: "…" }`                   | 同上。                                                       |
| `pubsub.publish`        | `{ topic: "…", data: "<b64>" }`                  | 对所有订阅者广播。                                           |
| `stream.create`         | `{ peer_id: "…", total_bytes: 1024 }`            | 回 `stream.created { stream_id }`。                          |
| `stream.chunk`          | `{ stream_id: 3, data: "<b64>" }`                | 送一个 chunk。                                               |
| `stream.finish`         | `{ stream_id: 3 }`                               | 结束流。                                                     |
| `trust.list`            | `{}`                                             | 返回信任条目。                                               |
| `trust.remove`          | `{ app_uuid: "…" }`                              | 移除信任。                                                   |
| `health`                | `{}`                                             | 返回 `{ ble_ok, tcp_ok, mdns_ok, node_state }`。             |
| `ping`                  | `{}`                                             | 应用层心跳，daemon 回 `pong`。                               |

### 1.3 mlink → 客户端消息类型

**Response（带 id，回应具体请求）**
- `ready` — hello 的回应，payload `{ app_uuid, caps: ["rpc","pubsub","stream","trust"] }`
- `room.joined` / `room.left` / `room.list_result`
- `peers.list_result` / `peers.scan_result` / `peers.disconnect_result`
- `message.sent` — 发送成功的确认（投递到 wire，不保证对端处理）
- `rpc.result` — `{ status: "ok"\|"error", data: "<b64>" }`
- `rpc.registered` / `rpc.reply_ack`
- `pubsub.subscribed` / `pubsub.unsubscribed` / `pubsub.published`
- `stream.created` / `stream.chunk_ack` / `stream.finished`
- `trust.list_result` / `trust.removed`
- `health.result`
- `pong`
- `error` — `{ code: "peer_gone"\|"room_mismatch"\|"unknown_method"\|"timeout"\|"bad_request"\|…, message: "…" }`

**Event（不带 id，纯推送；只推给订阅了相关 room 的 client）**
- `event.peer_connected` — `{ room: "…", peer_id, peer_name }`
- `event.peer_disconnected` — `{ room: "…", peer_id, reason?: "…" }`
- `event.peer_lost` / `event.peer_reconnecting` — 同上，多一个 attempt 字段
- `event.message_received` — `{ room: "…", peer_id, data: "<b64>" }`
- `event.rpc.request` — `{ peer_id, request_id, method, data: "<b64>" }`（daemon 把远端 Request 转推给注册了该 method 的 client；客户端必须回 `rpc.reply`）
- `event.pubsub.message` — `{ peer_id, topic, data: "<b64>" }`
- `event.stream.chunk` / `event.stream.finished` — 入站流

### 1.4 二进制 payload 策略

**默认 base64（`encoding: "base64"`）**，原因：
- JSON 原生不支持二进制；base64 编码开销固定 33%，对常见消息（< 64KB）可接受
- 代码简单，所有客户端（JS/Python/Rust/…）零特殊处理
- 未来加 `encoding: "binary"` + 二进制帧（tokio-tungstenite `Message::Binary` 配一个 type tag 头）作为可选优化，不在 MVP

**大载荷（> 256KB）强制走 stream**，不走 `message.send`。daemon 收到 `message.send` 且 base64 解码后超过阈值直接回 `error { code: "payload_too_large" }`，引导用户改用 stream。阈值可配置。

**为什么不用 MessagePack？** 二次序列化（JSON 壳 + MP body）更乱，收益仅节省 JSON 字段名那几个字节，不值。

### 1.5 错误码（固定枚举，便于客户端处理）

```
bad_request          — JSON 解析/字段校验失败
unsupported_version  — v != 1
not_ready            — 还没发 hello
not_in_room          — 发消息到没加入的 room
peer_gone            — 目标 peer 已掉线
room_mismatch        — Node 底层拒绝（对方 room_hash 不匹配）
unknown_method       — RPC 未注册
timeout              — RPC / 操作超时
payload_too_large    — 超过 message.send 阈值
duplicate_register   — 同一 method 在另一个 client 已注册
internal             — 其他（message 字段含细节）
```

---

## 2. daemon 架构

### 2.1 WS 库选型

**结论：`axum` + `axum::extract::ws`（底层就是 `tokio-tungstenite`）。**

| 维度         | axum::ws                               | tokio-tungstenite 直用  |
| ------------ | -------------------------------------- | ----------------------- |
| 路由         | 有，未来加 `/health` 等 HTTP 零成本    | 要自己 hyper 起手       |
| 扩展         | tower 中间件（日志、metrics、压缩）天然 | 全手写                  |
| 与 core 一致 | tokio 原生，跟 mlink-core 运行时一致   | 同上                    |
| 握手控制     | axum extractor 足够                    | 更底层，但不需要这种控制 |
| 代码量       | 更少                                   | 更多                    |

跟 `HTTP_ARCH_EVAL.md` 里 axum 的判断一致，差别只在首期只暴露 WS endpoint。后续加 HTTP 零成本。

### 2.2 端口选择策略

**动态端口 + 端口文件**：

1. daemon 启动：`TcpListener::bind("127.0.0.1:0")` 拿到 OS 分配的端口
2. 写 `~/.mlink/daemon.json`，内容 `{ "port": 51823, "pid": 12345, "started_at": "2026-04-20T…" }`
3. 客户端读这个文件拿端口
4. 退出时 drop guard 删文件

**加固**：
- `flock` 文件锁防止两个 daemon 同时起
- 启动时读到旧文件，`kill -0 <pid>` 验活；如果 pid 还活着直接拒绝启动（打印"daemon 已在运行 PID=12345 port=51823"）；死了就覆盖
- 端口可通过 `MLINK_DAEMON_PORT` 环境变量强制指定，方便调试和测试

**为什么不固定端口？** 多用户机器 / 测试并发 / 其他进程可能占用 —— 动态最安全。

### 2.3 Node 生命周期

**采纳"lazy，第一个 room.join 时创建"的策略**，不是"daemon 启动即创建"。原因：
- daemon 可能只做诊断/健康检查，不一定要打开蓝牙（避免弹权限框）
- BLE 资源占用只在真的要通信时才有必要
- 简化 daemon 启动路径，减少首屏失败面

具体规则：
1. daemon 启动：只起 WS server，Node 为 `None`
2. 第一个客户端 `room.join` 触发 `Node::new` + `Node::start`
3. 之后的 `room.join` 共享同一个 Node
4. **所有 client 都 leave 了所有 room** → daemon 保持 Node 运行 60 秒空闲后 stop（避免短连接抖动反复开关 BLE）
5. daemon shutdown：`Node::stop()` → 关所有连接 → 写"正常退出"标志

**关键决策：共享 Node 但 room 订阅按 client 隔离**
- Node 的 `room_hash: Option<[u8;8]>` 需要扩展为 `room_hashes: HashSet<[u8;8]>`（或 `Vec<[u8;8]>`）— 详见 §3.1
- 每个 WS client session 持有它订阅的 `HashSet<code>`
- 入站 peer 事件按 room 过滤分发：只有订阅该 room 的 session 才收到该 peer 的事件

### 2.4 多客户端并发

**一个 daemon 多个 WS client 同时连**，典型场景：一个 agent 连，一个 GUI 监控，一个 CLI `status`。

并发设计：
- 每个 WS 连接一个 `ClientSession` actor（`tokio::spawn`）
- ClientSession 持有 `Arc<DaemonState>`
- 入站 NodeEvent 通过 `broadcast::Receiver` 分发；session 自己过滤（`if self.rooms.contains(room_of_event)`）
- 出站命令：直接调 `DaemonState` 的方法（都已 `Arc<Mutex<…>>` 包装，内部并发）

**RPC handler 注册冲突**：
- 同一个 method 只能被一个 session 持有 handler
- 第二个 session 来注册同名 method → 回 `error: duplicate_register`
- 注册 session 断开 → 自动 unregister；若此时有 pending 的 `event.rpc.request` 正在等 client 回复，daemon 对远端发 `status=error, data="handler gone"`

**连接级心跳**：
- WS 协议层 ping/pong（tokio-tungstenite 自带）
- 15 秒无数据流动 daemon 主动发 ping，60 秒无 pong 断开

### 2.5 DaemonState 结构草图

```rust
pub struct DaemonState {
    node: Arc<Mutex<Option<Node>>>,          // lazy init
    rooms: Arc<Mutex<RoomManager>>,          // 多 room 账本
    rpc_registry: Arc<RpcRegistry>,
    pending_rpc: Arc<PendingRequests>,
    pubsub: Arc<Mutex<PubSubManager>>,
    sessions: Arc<SessionManager>,           // 入站 peer reader + 重连循环（搬家 CLI）
    rpc_handler_owner: Arc<RwLock<HashMap<String, SessionId>>>,  // method -> 哪个 session 拥有
    events_tx: broadcast::Sender<RoutedEvent>,  // 带 room 标签的事件
    client_sessions: Arc<RwLock<HashMap<SessionId, ClientHandle>>>,
}

#[derive(Clone)]
struct ClientHandle {
    session_id: SessionId,
    // 单向：daemon -> client 出站队列
    outbound_tx: mpsc::Sender<ServerMsg>,
    rooms: Arc<RwLock<HashSet<String>>>,  // 订阅的 room code
}

#[derive(Clone)]
enum RoutedEvent {
    PeerConnected { room: String, peer_id: String, peer_name: String },
    PeerDisconnected { room: String, peer_id: String },
    MessageReceived { room: String, peer_id: String, payload: Vec<u8> },
    // …
}
```

`RoutedEvent` vs 直接用 `NodeEvent`：后者不含 room 信息（Node 层不管 room 路由），daemon 层必须自己打标签。这是把 core 发生的事情"按 room 维度 fan-out"的必要封装。

### 2.6 Session 化的 CLI 主循环（搬家）

跟 `HTTP_ARCH_EVAL.md` §2.4 一致，这是最大工作量：

CLI `main.rs` 里的 `engaged_wire_ids` / `attempts` / `connected_peers` / `wire_to_app` / `readers` / `peer_names` / 重试循环，全部搬到 `SessionManager`，改成"对所有活跃 room_hash 扫描 + 对新发现的 peer 发起连接 + 每个连接起 `spawn_peer_reader`"。

关键适配：
- 扫描时把 `Node::room_hash` 读取改成读 `RoomManager::active_hashes()`，把扫描过滤 predicate 从"等于单一值"改成"在集合里"
- peer 连上后，daemon 要知道这个 peer 属于哪个 room —— 握手里 `Handshake.room_hash` 已经带了，用它反查 `room_code`，再把 peer 归入 `RoomManager.add_peer(code, peer_id)`
- reader 读到消息 → 查这个 peer 所在 room → 发 `RoutedEvent`

---

## 3. core 适配

### 3.1 必须改 core 的点：多 room_hash

**当前**：
```rust
pub struct Node { … room_hash: Option<[u8; 8]> … }
pub fn set_room_hash(&mut self, hash: Option<[u8; 8]>)
```

`connect_peer` / `accept_incoming` 里用 `if let Some(local) = self.room_hash` 做单值比对。

**改动**：
```rust
pub struct Node { … room_hashes: HashSet<[u8; 8]> … }
pub fn set_room_hashes(&mut self, hashes: HashSet<[u8; 8]>)
pub fn add_room_hash(&mut self, hash: [u8; 8])
pub fn remove_room_hash(&mut self, hash: [u8; 8])
pub fn room_hashes(&self) -> &HashSet<[u8; 8]>
```

连接时比对逻辑改：
- 如果 `room_hashes` 为空 → 接受任何（跟当前"None"行为一致）
- 否则 peer 的 `room_hash` 必须 ∈ `room_hashes`；否则 `RoomMismatch`

**向后兼容**：保留 `set_room_hash(Option<[u8;8]>)` 作为 thin wrapper（`None` → 清空，`Some(x)` → 单元素集合）。CLI 不需要改。

**Handshake 报文不变**：对端只带自己的一个 room_hash（它加入了哪个 room 就带哪个），本地用集合匹配就行。

**scanner 过滤**：scanner 当前也需要 room_hash 做 BLE 广告过滤；改成 "广告命中任一活跃 hash 就过" 即可（见 `project_ble_overflow_scanfilter.md`）。

**工作量**：core 改动约 30 行 + 测试，不大。

### 3.2 其他 core 改动（小）

- `NodeEvent` 增加 `room_hash` 字段（可选）：让 daemon 不用再反查。但这会污染 core 层语义，我倾向**不改 NodeEvent**，daemon 自己维护 `peer_id → room_hash` 映射（`accept_incoming` / `connect_peer` 拿到 handshake 时记一下即可）。

- `api::message::broadcast_message` 当前广播给所有 peers，daemon 场景需要"广播到指定 room 的 peers"—— 这个不改 core，daemon 自己遍历该 room 的 peer 列表调 `send_message` 就行。

### 3.3 不需要改的点

- `api::rpc` / `api::pubsub` / `api::stream` —— 都是 `&Node`，daemon 直接包装
- `Node::subscribe()` 的 broadcast channel —— 直接转发
- `ConnectionManager` / `PeerManager` / `TrustStore` —— 并发安全，daemon 直接用

---

## 4. 技术决策里不显然的几点（别让产品问住）

1. **为什么首期只做 WS，不做 REST？**
   - REST 的诊断/只读 endpoint 有价值（比如 `mlink status`），但 WS 已经能覆盖它们（`peers.list` / `health`），且省掉"两套协议"的维护面
   - 等下面任一条成立再加 REST：(a) 有浏览器/curl 调试强需求；(b) 要给 Electron 主进程外的东西调；(c) 要加 webhook
2. **WS 连接断了怎么办？**
   - 订阅状态（rooms / rpc handlers / pubsub subs）都**绑定到 WS 连接**，连接一断全部回收
   - 客户端必须**重连后重新 hello + join 所有 room + 注册所有 handler**
   - 语义简单，实现干净，符合"上层应用自己管重连"的原则
3. **daemon 不做持久化队列**
   - `message.send` 是"尝试送达"，client 掉线期间来的入站消息会丢
   - 这跟现在 CLI 的行为一致；要可靠交付是上层应用自己的事（用 RPC + ack 或 stream）
   - 如果以后要队列，加到 daemon 不是 core（core 保持无状态）
4. **认证/授权**
   - MVP 绑 127.0.0.1，信任边界是"本机进程"
   - 加 token 时，在 `hello` payload 里带 `token` 字段，daemon 读 `~/.mlink/auth_token`（mode 0600）比对 —— 这是下一步，不在首期
5. **JSON schema 要不要 formalize？**
   - 首期用 Rust 结构体 + serde_json 自包含约束，够了
   - 如果要给 TS/Python SDK 生成，再产 JSON Schema 或 protobuf；现在不引入

---

## 5. 工作量估算

| 模块 | 人日 | 备注 |
|---|---|---|
| axum WS 骨架 + 消息信封 + 错误映射 | 1 | |
| daemon.json 端口文件 + flock + 启动/停止 | 0.5 | |
| DaemonState + `Arc<Mutex<Option<Node>>>` + lazy 生命周期 | 1 | |
| core 多 room_hash 改造 + 测试 | 0.5 | |
| SessionManager（搬家 CLI 主循环 + 多 room 扫描） | **3** | 最大块；跟 HTTP 评估里的估计一致 |
| room.* / peers.* endpoint | 1 | |
| message.* endpoint + 广播限流 | 0.5 | |
| rpc.* endpoint（含 handler 跨 session 注册 + 生命周期） | 1.5 | |
| pubsub.* + stream.* endpoint | 1.5 | |
| trust.* / health / ping | 0.5 | |
| 事件路由（RoutedEvent fan-out + room 过滤） | 1 | |
| WS 级心跳 + 优雅关闭 | 0.5 | |
| 集成测试（两个 daemon 互连 + 模拟 client 跑完整场景） | 2 | |
| **合计** | **14** | 1 人 3 周，2 人并行 1.5 周 |

比 HTTP 评估（15 人日）少 1 人日，因为 SSE/长连接的两套机制合成一套；多 room 改造和 RoutedEvent 打标签是新增。

---

## 6. 风险和缓解

| 风险 | 级别 | 缓解 |
|---|---|---|
| core `Node::room_hash` 改成集合可能漏改路径 | 中 | 保留 `set_room_hash` wrapper + 现有测试用例全跑；新增多 room 集成测试 |
| 多 client 同一 room 都发 `message.send` → 广播风暴 | 中 | daemon 层做限流（默认 100 msg/s/client）；超过回 `error` |
| RPC handler owner 的 session 崩溃 → 远端挂起 | 中 | handler session 断开时把此时 pending 的入站 Request 立即回 `status=error`；未来 client 可以用 `rpc.register` 的 `takeover: true` 标志抢 |
| WS 连接堆积（忘记关） | 低 | 连接数上限（默认 64），超了直接拒；加 metrics |
| JSON 大消息造成 HoL blocking | 中 | `message.send > 256KB` 一律回 `payload_too_large`；stream 拆 chunk |
| lazy Node 启动和第一次 `room.join` 之间的竞态 | 低 | `Arc<Mutex<Option<Node>>>` + 启动加锁一次性完成；第二个并发 join 看到 `Some` 直接用 |
| daemon.json pid 复用（pid 回绕） | 低 | 同时存 pid + started_at；校验 launch time；不完美但够用 |
| 客户端断连后 pending RPC | 低 | 出站侧 daemon 仍等远端；远端回来时发现 session 没了 → drop，pending 里 timeout 清理 |

---

## 7. 下一步（给实现者看）

假定设计过审，推荐实现顺序（跟工作量表一致）：

**Week 1**：骨架 + core 改造
1. 新 crate `mlink-daemon`，axum 0.7 + tokio-tungstenite
2. `hello` / `ping` / `health` 三个 endpoint 能通
3. core `Node` 多 room_hash 改造，所有既有测试过
4. 端口文件 + 启动/停止

**Week 2**：核心 room/peer/message + SessionManager
1. `room.join` / `room.leave` / `room.list`
2. SessionManager 搬家 CLI 主循环，支持多 room 扫描
3. `peers.list` / `peers.scan`
4. `message.send` + `event.message_received`（用 broadcast + room 过滤）

**Week 3**：RPC + PubSub + Stream + 测试
1. `rpc.register` + handler owner 表 + `event.rpc.request` + `rpc.reply`
2. `rpc.call` + `pending_rpc`
3. `pubsub.*`
4. `stream.*`
5. 双 daemon E2E 集成测试；多 client 并发测试

**不在范围**：CLI 改造、REST endpoint、token 鉴权、跨机 WS 代理、持久化队列 —— 都是 post-MVP。

---

## 附录 A：消息示例（三个完整来回）

**A1. join room 并收消息**
```jsonc
// C -> S
{"v":1,"id":"1","type":"hello","payload":{"client_name":"agent-x","client_version":"0.1"}}
// S -> C
{"v":1,"id":"1","type":"ready","payload":{"app_uuid":"a1b2…","caps":["rpc","pubsub","stream","trust"]}}

// C -> S
{"v":1,"id":"2","type":"room.join","payload":{"code":"482193","encrypt":true}}
// S -> C
{"v":1,"id":"2","type":"room.joined","payload":{"code":"482193","hash":"abcdef0123456789"}}

// S -> C (event, 无 id)
{"v":1,"type":"event.peer_connected","payload":{"room":"482193","peer_id":"u1…","peer_name":"bob"}}

// S -> C (event)
{"v":1,"type":"event.message_received","payload":{"room":"482193","peer_id":"u1…","data":"aGVsbG8="}}
```

**A2. 发消息**
```jsonc
// C -> S
{"v":1,"id":"3","type":"message.send","payload":{"room":"482193","peer_id":"u1…","data":"aGVsbG8=","encoding":"base64"}}
// S -> C
{"v":1,"id":"3","type":"message.sent","payload":{}}
```

**A3. RPC 双向**
```jsonc
// client-B 注册 handler
{"v":1,"id":"10","type":"rpc.register","payload":{"method":"echo"}}
{"v":1,"id":"10","type":"rpc.registered","payload":{}}

// client-A 远端发起调用（机器 A 上）
{"v":1,"id":"20","type":"rpc.call","payload":{"peer_id":"u1B…","method":"echo","data":"aGk=","timeout_ms":5000}}

// client-B 收到请求（机器 B 上，event）
{"v":1,"type":"event.rpc.request","payload":{"peer_id":"u1A…","request_id":"daemon-generated-7","method":"echo","data":"aGk="}}

// client-B 回复
{"v":1,"id":"11","type":"rpc.reply","payload":{"request_id":"daemon-generated-7","status":"ok","data":"aGk="}}
{"v":1,"id":"11","type":"rpc.reply_ack","payload":{}}

// client-A 拿到结果
{"v":1,"id":"20","type":"rpc.result","payload":{"status":"ok","data":"aGk="}}
```

---

## 附录 B：关键代码骨架（节选）

```rust
// ClientSession 入站循环
async fn session_loop(
    mut ws: WebSocket,
    state: Arc<DaemonState>,
    session_id: SessionId,
) {
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ServerMsg>(256);
    let handle = ClientHandle { session_id, outbound_tx, rooms: Default::default() };
    state.register_session(handle.clone()).await;
    let mut events = state.events_tx.subscribe();

    // 等 hello
    let hello = match ws.next().await { … };
    // send ready
    …

    loop {
        tokio::select! {
            // 入站 client 消息
            Some(Ok(msg)) = ws.next() => { dispatch_client_msg(&state, &handle, msg).await; }
            // 出站队列（来自 dispatch 里 await 的 response）
            Some(out) = outbound_rx.recv() => { ws.send(out.to_ws()).await.ok(); }
            // 全局 NodeEvent
            Ok(ev) = events.recv() => {
                if handle.rooms.read().await.contains(&ev.room_code()) {
                    ws.send(ev.to_event_msg().to_ws()).await.ok();
                }
            }
            else => break,
        }
    }
    state.unregister_session(session_id).await;
}
```

— 完 —
