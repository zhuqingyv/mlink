# mlink HTTP 接口改造 —— 技术评估报告

**评估员**：tech-eval
**日期**：2026-04-20
**目标**：评估 mlink 从"纯 CLI/库"改造为"内核 + HTTP API 接口层"的可行性、工作量、风险

---

## 结论（TL;DR）

**可以改，改动量中等，风险可控。**

- 内核 API 已经足够干净，**80% 的 endpoint 可以直接薄包一层**，核心改动集中在"把 CLI main.rs 的 1600 行状态机（engaged_wire_ids / attempts / connected_peers / wire_to_app / readers / peer_names / retry 循环）抽出来变成 Daemon Session"。
- `Node` 的 `&mut self` 不是真并发阻塞点，内部 state 已经全 `Arc<Mutex/RwLock>`，把外层 `Node` 再套 `Arc<Mutex<Node>>` 即可，**不需要重构 core**。
- 长连接推送（事件流）直接复用 `node.subscribe()` 的 `broadcast::Receiver<NodeEvent>`，SSE/WS 都天然适配。
- 推荐 crate 结构：`mlink-core`（不变） + `mlink-daemon`（新增，Node 生命周期 + HTTP server） + `mlink-cli`（改为 HTTP client，保留 `--local` 模式直调 core 以兼容离线诊断）。

---

## 一、现有架构能否支撑？

### 1.1 Node 的公开 API 盘点

读完 `crates/mlink-core/src/core/node.rs` + `src/lib.rs`，对外暴露的方法按可并发性分三类：

**A. `&self` 只读/共享可变（天然并发安全）**
- `state()`, `app_uuid()`, `config()`, `room_hash()`
- `peers()`, `get_peer()`, `peer_state()`, `has_peer()`, `connection_count()`
- `subscribe()` → 返回 `broadcast::Receiver<NodeEvent>` —— **SSE/WS 的完美来源**
- `send_raw()`, `recv_raw()` —— 内部走 `self.connections.lock().await`，并发安全
- `send_heartbeat()`, `check_heartbeat()`
- `set_peer_aes_key()`, `trust_store()`, `role_for()`
- `spawn_peer_reader()` —— 后台拉流发事件，HTTP 层直接复用
- `mark_lost()`, `mark_reconnecting()`

**B. `&mut self` 但仅改 `self.state: NodeState` 这个 `Copy` 枚举**
- `start()`, `stop()`, `set_room_hash()`, `connect_peer()`, `accept_incoming()`, `attach_connection()`, `disconnect_peer()`

`&mut` 的根本原因只是写 `self.state`（Idle/Discovering/Connected…）这一个整数。内部的 `peer_manager`/`peer_states`/`connections`/`trust_store` **已经是 `Arc<RwLock/Mutex>`**，完全可以在多请求间共享。

**结论**：把外层 Node 套一层 `Arc<Mutex<Node>>` 即可解决 `&mut`；如果在意锁粒度，后续可以把 `state` 也换成 `Arc<AtomicU8>` 或者 `RwLock<NodeState>`，彻底去掉 `&mut`。**不是阻塞项，是可选优化**。

### 1.2 API 模块盘点

- `api::message` —— `send_message` / `broadcast_message` / `recv_message`：全 `&Node`，直接 HTTP POST `/messages`
- `api::rpc` —— `rpc_request` / `send_response` / `RpcRegistry` / `PendingRequests`：`&Node` + 两个可共享 manager，可以做 `/rpc/call` + `/rpc/register` + `/rpc/dispatch` endpoint
- `api::pubsub` —— `PubSubManager` + `publish`：`&Node` + `&PubSubManager`，HTTP `/pubsub/{subscribe,unsubscribe,publish}`
- `api::stream` —— `create_stream()` 需要 `Arc<Node>`，返回 `StreamWriter` —— **已经是 Arc 设计，HTTP 改造最顺**

### 1.3 结论：**核心 API 已经可以直接包 HTTP，不需要大改 core。**

---

## 二、改造工作量

### 2.1 HTTP 框架选型

**推荐 axum。**

| 维度 | axum | actix-web | warp |
|---|---|---|---|
| 生态契合 | ⭐⭐⭐⭐⭐ tokio 原生，同 mlink-core | ⭐⭐⭐ 自有 actor 运行时，要桥接 | ⭐⭐⭐⭐ tokio 原生 |
| SSE/WS | ⭐⭐⭐⭐⭐ 内置 | ⭐⭐⭐⭐ 内置 | ⭐⭐⭐ 略啰嗦 |
| 学习成本 | ⭐⭐⭐⭐ 函数式 extractor，简单 | ⭐⭐⭐ actor 概念 | ⭐⭐ filter 组合器学习曲线陡 |
| 2026 主流度 | ⭐⭐⭐⭐⭐ Rust HTTP 事实标准 | ⭐⭐⭐⭐ | ⭐⭐ 逐渐冷却 |

axum 0.7+ / tower / hyper 跟 tokio 运行时完全一致，不引入额外运行时，最自然。

### 2.2 需要实现的 endpoint（粗略清单）

```
# 生命周期
POST   /v1/node/start                {room_code?, encrypt?, name?}
POST   /v1/node/stop
GET    /v1/node/status               → {state, app_uuid, room_hash, peer_count}

# 房间
POST   /v1/rooms                     → {code}           # 新建
POST   /v1/rooms/{code}/join
POST   /v1/rooms/{code}/leave

# Peer
GET    /v1/peers                     → [peer...]
GET    /v1/peers/{id}
POST   /v1/peers/{id}/disconnect
POST   /v1/peers/scan                → [discovered...]  # 一次性扫描

# 消息
POST   /v1/messages                  {peer_id?, data_base64} # peer_id 空 = broadcast
GET    /v1/messages/stream           → SSE: MessageReceived 事件流

# RPC
POST   /v1/rpc/call                  {peer_id, method, data}
POST   /v1/rpc/handlers/{method}     {webhook_url}     # 注册本地 handler（可选，高级）

# PubSub
POST   /v1/pubsub/topics/{topic}/subscribe  {peer_id}
POST   /v1/pubsub/topics/{topic}/publish    {data}

# 流传输
POST   /v1/streams                   {peer_id, total_bytes} → {stream_id}
POST   /v1/streams/{id}/chunks       (body = chunk bytes)
POST   /v1/streams/{id}/finish

# 事件（唯一长连接入口）
GET    /v1/events                    → SSE 或 WebSocket: 所有 NodeEvent

# 信任
GET    /v1/trust
DELETE /v1/trust/{app_uuid}

# 诊断
GET    /v1/health                    → {ble_ok, tcp_ok, mdns_ok}
```

### 2.3 长连接推送

**首选 SSE**，原因：
- `Node::subscribe()` 返回 `broadcast::Receiver<NodeEvent>`，每个 HTTP 连接 subscribe 一次，转发成 `event: ... / data: ...` 即可
- 客户端只订阅，不上行，不需要双工 —— SSE 足够
- 浏览器/Electron 原生支持，无额外握手

**需要双工时**（比如 chat 的输入流）再加 WebSocket endpoint，但 MVP 用 SSE + POST 组合就够了。

### 2.4 Node 生命周期（daemon 模式）

关键决策：**一个进程一个 Node，不支持多租户**。理由：
- BLE peripheral 在 macOS 一个进程只能有一个 `CBPeripheralManager`（见 `project_peripheral_bringup_timing.md`）
- Scanner + Transport 持有独占资源（btleplug adapter / mDNS daemon 句柄）
- mlink 定位是"本机设备连接层"，单实例就是正确模型

daemon 内部结构：
```rust
struct DaemonState {
    node: Arc<Mutex<Node>>,              // 主对象
    transports: TransportSet,            // BLE/TCP 连接池
    pubsub: Arc<Mutex<PubSubManager>>,
    rpc_registry: Arc<RpcRegistry>,
    pending_rpc: Arc<PendingRequests>,
    sessions: SessionManager,            // CLI main.rs 里 1000 行循环的搬家
    events_tx: broadcast::Sender<NodeEvent>,  // 复用 node.subscribe()
}
```

其中 `SessionManager` 封装 CLI `cmd_serve` / `cmd_chat` / `cmd_join` 里那一大堆（engaged_wire_ids、connected_peers、wire_to_app、attempts、readers、peer_names、backoff 重试）。**这是本次改造的主要工作量**，核心逻辑已有，只是从 main 函数的 `select!` 循环搬到一个 actor-like 的后台任务里。

### 2.5 工作量估算

| 模块 | 工时（人日） |
|---|---|
| axum 骨架 + 路由 + 错误映射 | 1 |
| Node 生命周期 / DaemonState 组装 | 1 |
| SSE 事件流（复用 node.subscribe） | 1 |
| Peer / Message / Scan endpoint | 2 |
| SessionManager（搬家 CLI 主循环）| **3-4**（最大块） |
| RPC / PubSub endpoint | 1.5 |
| Stream endpoint（chunk 上传 + finish）| 1.5 |
| Trust / Doctor / Health | 0.5 |
| CLI 改造成 HTTP client | 1.5 |
| 集成测试 + 手工 E2E | 2 |
| **合计** | **15 人日** |

一个人 3 周内可完结，两人并行 1.5 周。

---

## 三、架构风险

### 3.1 Node 的 `&mut self` 方法在多请求下安全吗？

**不直接安全，但加一层 `Arc<Mutex<Node>>` 就解决。**

- 内部 state（peer_manager/connections/peer_states/trust_store）已 `Arc<RwLock/Mutex>`，多请求同时 `send_raw` 不互斥到彼此
- `&mut self` 的方法（connect_peer / accept_incoming / disconnect_peer / start / stop / set_room_hash）在 daemon 内**本来就应该串行化**（一个房间同时只能一个状态变更），外层 `Mutex<Node>` 完全符合语义
- 热路径 `send_raw` / `recv_raw` 是 `&self`，不会被外层 Mutex 卡到

**优化方向**（非阻塞项）：
- 可以把 `Node::state: NodeState` 换成 `AtomicU8`（NodeState 是 `Copy` 枚举），彻底 `&mut` → `&self`
- 或者引入 `ConnectionCommand` actor 模式，HTTP 请求转成 command 消息，由单个 loop 串行处理 —— 更干净，但工作量大

### 3.2 Connection / PeerState 是否 thread-safe？

- `ConnectionManager` 被 `Arc<Mutex<>>` 包着 —— 安全
- `PeerState` 在 `Arc<RwLock<HashMap>>` 里 —— 安全
- `Box<dyn Connection>` 本身不要求 `Sync`，但 `get_mut` 在单个 Mutex guard 下读写 —— 安全
- **风险点**：`recv_raw` 和 `spawn_peer_reader::recv_once` 都会 `lock().await.read()` —— 如果有两个协程同时对同一个 peer 调 `recv_raw`，会按 Mutex 顺序等待，不会数据损坏，但可能丢事件。
  - **缓解**：禁止 HTTP 调 `recv_raw`，**只允许通过 SSE + spawn_peer_reader 消费入站消息**。这是强约束。

### 3.3 HTTP 请求如何关联到正确的 Room/Peer？

- 一个 daemon 一个 Node，一个 Node 当前持有一个 `room_hash`
- Peer 定位：`peer_id`（即 app_uuid，全局唯一）直接作为 URL path 参数
- Room 定位：由 daemon 内 RoomManager 管理（`mlink-core` 已有 `core::room::RoomManager`）
- **不需要 session token** —— 本地 HTTP（localhost），信任边界是"能打开 127.0.0.1 的进程"。如果未来要跨机访问，再加 token，但不在 MVP

### 3.4 其他风险

| 风险 | 程度 | 缓解 |
|---|---|---|
| CLI 短命进程 → daemon 模式后 `mlink send` 变成 HTTP 调用 | 中 | CLI 自动 `mlink daemon start` 如没起 / 提供 `--local` 直调 core 的 fallback |
| daemon 崩溃连带所有 session 丢失 | 中 | 跟现状一致，不是 regression；加 systemd/launchd 守护 |
| 端口冲突 127.0.0.1:XXXX | 低 | 动态端口 + 写 `~/.mlink/port` 文件，CLI 读取 |
| macOS BLE/蓝牙权限弹窗由 daemon 触发，用户可能拒绝 | 中 | daemon 首次启动必须前台运行一次拿权限 |
| 多 CLI 客户端并发发同一消息 | 低 | Node 内部锁已解决；语义上允许 |
| `create_stream` 的 `Arc<Node>` 要求 | 已知 | daemon 里 Node 本来就是 `Arc<Mutex<Node>>`，取 `.lock()` 里的 `&Node` 包 Arc 即可；比 CLI 里的 `unsafe_clone_node` 干净 |

---

## 四、对现有 CLI 的影响

**改造后 CLI 有两种模式**：

1. **默认（daemon 模式）**：CLI 调本地 HTTP API
   - `mlink serve` → `POST /v1/node/start` + 打印 SSE 事件流
   - `mlink send 482193 hello` → `POST /v1/messages`
   - `mlink status` → `GET /v1/node/status` —— 终于能显示真实 peer 了（目前 CLI 是无状态的）

2. **`--local` / 离线模式**：CLI 直调 core（保留现有实现）
   - 用于诊断、debug、CI 测试
   - `mlink --local doctor` 之类的一次性命令保留

**强烈建议保留 --local 模式**，原因：
- 依赖 daemon 运行才能诊断有点尴尬（daemon 起不来怎么办？）
- `doctor` 子命令就是要在 daemon 没起的情况下能跑
- CI 里的集成测试也需要单进程跑

---

## 五、推荐的 crate 架构

```
mlink/
├── crates/
│   ├── mlink-core/              # 不变（保持现状）
│   │   └── Node/Scanner/Connection/Transport/api
│   │
│   ├── mlink-daemon/            # 新增
│   │   ├── src/
│   │   │   ├── lib.rs           # DaemonState 主对象 + 启动入口
│   │   │   ├── http/
│   │   │   │   ├── routes/      # axum router + endpoint 实现
│   │   │   │   ├── sse.rs       # 事件流
│   │   │   │   └── error.rs     # MlinkError → HTTP status
│   │   │   ├── session.rs       # 搬家 CLI 主循环（engaged/attempts/readers…）
│   │   │   └── bin/
│   │   │       └── mlinkd.rs    # daemon 独立二进制
│   │   └── Cargo.toml
│   │       # 依赖: mlink-core, axum, tokio, tower, serde, tracing
│   │
│   └── mlink-cli/               # 改造：HTTP client + --local fallback
│       └── src/
│           ├── lib.rs           # clap 定义不变
│           ├── main.rs          # 变薄
│           ├── http_client.rs   # 新增
│           └── local.rs         # 把现在 main.rs 的实现挪过来，--local 时用
```

二进制产物：
- `mlink`（CLI，默认走 HTTP）
- `mlinkd`（daemon 独立二进制，可被 launchd/systemd 托管）

**Cargo.toml workspace 变动**：
```toml
[workspace]
members = ["crates/mlink-core", "crates/mlink-daemon", "crates/mlink-cli"]
```

---

## 六、推荐改造路线（增量）

1. **Week 1：骨架**
   - 新 crate `mlink-daemon`，axum 骨架，`/health` + `/status` endpoint
   - `DaemonState` + `Arc<Mutex<Node>>` 包装
   - SSE `/events` 直接转发 `node.subscribe()` —— 最早可以看到事件流
2. **Week 2：核心 endpoint**
   - peers / messages / scan / start/stop
   - 把 CLI `cmd_serve` 主循环搬进 `session.rs` 作为独立任务
3. **Week 3：RPC + PubSub + Stream + CLI 改造**
   - RPC/PubSub endpoint
   - CLI 加 HTTP client，保留 `--local` fallback
   - E2E 测试（两个 daemon 互联 + CLI 调 HTTP）

---

## 七、最后的判断

- **能不能改？** 能。内核已经为此做好了大部分铺垫（broadcast events / Arc 内部 state / api/* 模块清晰）。
- **改多大？** 中等：15 人日，新增 1 个 crate，CLI 变薄，core 不动。
- **怎么改？** axum daemon + SSE 事件 + `Arc<Mutex<Node>>` + CLI 双模式（HTTP / --local）。
- **最大风险？** CLI `cmd_serve` / `cmd_chat` 那 1000 行状态机搬家 —— 这是工时集中点，也是 bug 集中点。建议**先搬家，保持行为不变，再 HTTP 化**（两步走，避免同时动两轴）。
- **最大收获？** `mlink status` 第一次能显示真实 peer；多个 GUI/CLI 客户端可以同时连同一个 daemon；为 Electron/桌面应用开路。
