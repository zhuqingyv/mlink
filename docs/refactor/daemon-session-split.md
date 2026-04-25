# mlink-daemon/src/session.rs 拆分设计

## 当前结构分析

- **文件总行数**：602 行（`crates/mlink-daemon/src/session.rs`）
- **职责**：单个 WS 连接从建立到断开的全部生命周期。负责接收帧、校验版本、分派到各 `handle_*`、把 NodeEvent 转成 `room_state`、从 fan-out mpsc 把消息写回 socket，以及连接结束时反注册。
- **共享状态**：通过 `DaemonState`（`node` / `node_events` / `sessions` / `rooms` / `queue`） 与 fan-out worker（定义在 `lib.rs`）协作；sub 集合保存在 `SessionHandle = Arc<Mutex<SessionEntry>>`。

### 常量
| 名称 | 行号 | 职责 |
|---|---|---|
| `MAX_FRAME_BYTES` | 36 | WS 单帧上限（1 MB），超过即回 `payload_too_large` |
| `OUTBOUND_CAPACITY` | 41 | 每会话出站 mpsc 容量（1024） |

### 函数与结构
| 名称 | 行号 | 可见性 | 职责 |
|---|---|---|---|
| `run` | 43–158 | pub | 会话顶层事件循环：`ready` → 注册 → `select!`（socket / mpsc / broadcast） → 反注册 |
| `handle_text` | 162–219 | fn | 解析 Envelope、校验版本、按 `ty` 分派 |
| `handle_hello` | 221–249 | fn | `HelloPayload` → 回 `ready` + 可选 `ack` |
| `handle_join` | 251–313 | fn | `RoomPayload` 校验 → `add_room_hash` + `rooms.add` → 原子订阅+drain → `ack` + `room_state` + backlog flush |
| `handle_leave` | 315–371 | fn | 移除 sub；若无其他会话引用则 `remove_room_hash` + `rooms.remove` + `queue.remove_room`；回 `ack` + `room_state` |
| `handle_send` | 373–477 | fn | `SendPayload` 校验 → 订阅检查 → 目标解析（单 peer / 广播） → 逐 peer `node.send_raw` → `ack` 或聚合错误 |
| `handle_ping` | 479–481 | fn | 回 `pong` |
| `forward_peer_event` | 494–518 | fn | PeerConnected / PeerDisconnected → 向每个订阅房间推送新 `room_state`；其他事件忽略 |
| `send_backlog_entry` | 520–532 | fn | 把 `MessageEntry` 编码成 `message` 帧并写出 |
| `send_room_state` | 534–553 | fn | 查 `node.peers()` → 构造 `RoomStatePayload` → 写出 |
| `send_ack_if_id` | 555–559 | fn | 仅当请求带 id 时回 `ack` |
| `send_bad_payload` | 561–575 | fn | 统一 `bad_payload` 错误帧 |
| `send` | 577–579 | fn | 把 `String` 包成 `Message::Text` 写入 socket |
| `valid_room_code` | 581–583 | fn | 6 位 ASCII 数字校验 |
| `tests` | 585–602 | mod | `valid_room_code_*` 两条 |

### 功能边界识别
1. **生命周期编排**：`run` — 绑定 socket、初始化订阅态、跑 `select!`、退出反注册。
2. **入帧分派**：`handle_text` + 版本 / JSON 校验。
3. **命令处理器**：`handle_hello` / `handle_join` / `handle_leave` / `handle_send` / `handle_ping` — 每个都是独立的业务用例。
4. **事件桥接**：`forward_peer_event`（把 NodeEvent 变成 WS `room_state`）。
5. **出帧辅助**：`send` / `send_ack_if_id` / `send_bad_payload` / `send_backlog_entry` / `send_room_state`。
6. **验证**：`valid_room_code`（静态小工具）。

## 拆分方案

目标：在不改变外部 API（`lib.rs` 仅调用 `session::run`）的前提下，把 `session.rs` 变成一个 `session/` 目录模块，内部按职责分层。每个文件 ≤ 200 行。

```
crates/mlink-daemon/src/
  session/
    mod.rs           # 入口，re-export run；MAX_FRAME_BYTES/OUTBOUND_CAPACITY 常量
    lifecycle.rs     # run() + 注册/反注册 + select! 主循环
    dispatch.rs      # handle_text + 版本检查 + 类型分派
    handlers.rs      # handle_hello / handle_join / handle_leave / handle_ping
    handlers_send.rs # handle_send
    events.rs        # forward_peer_event
    outbound.rs      # send / send_ack_if_id / send_bad_payload / send_backlog_entry / send_room_state
    validate.rs      # valid_room_code + 6 位校验的单元测试
```

### session/mod.rs（≈ 25 行）
- **职责**：对外暴露 `run`；声明子模块；收纳顶层常量；承载模块级文档注释。
- **包含**：
  - 原源码 1–12 行的 `//!` 模块级注释（"Sessions no longer own the room…"）整体迁入 `session/mod.rs` 文件头，作为整个 `session/` 目录模块的文档
  - 常量 `MAX_FRAME_BYTES`（原 36）、`OUTBOUND_CAPACITY`（原 41）
  - `mod lifecycle; mod dispatch; mod handlers; mod handlers_send; mod events; mod outbound; mod validate;`
  - `pub use lifecycle::run;`
- **对外 API**：`pub async fn run(socket, state)`（通过 re-export 保持 `session::run` 调用点不变）

### session/lifecycle.rs（≈ 100 行）
- **职责**：WebSocket 会话的生命周期编排，除命令分派之外的全部“粘合”。
- **包含**（原 43–158）：
  - `pub async fn run(mut socket: WebSocket, state: DaemonState)`
  - `ready` 预先发送逻辑
  - `SessionHandle` 创建 + `state.sessions.push`
  - `tokio::select!` 主循环（socket / mpsc / broadcast 三路）
  - 超长帧拦截、二进制帧拒绝、Close/Ping/Pong 处理
  - 反注册（`sessions.retain`）
- **依赖内部**：`dispatch::handle_text`、`events::forward_peer_event`、`outbound::send`
- **依赖外部**：`DaemonState`、`SessionHandle`、`SessionEntry`、`encode_frame`、`ErrorPayload`、`codes::*`、`DAEMON_VERSION`、`NodeEvent`、`broadcast::RecvError`
- **对外 API**：`pub async fn run(...)`（被 `mod.rs` re-export）

### session/dispatch.rs（≈ 60 行）
- **职责**：把一帧文本解析成 `Envelope`，校验协议版本，按 `ty` 路由到 `handlers`。
- **包含**（原 162–219）：
  - `pub(super) async fn handle_text(socket, state, handle, text) -> bool`
- **依赖内部**：`handlers::{handle_hello, handle_join, handle_leave, handle_send, handle_ping}`、`outbound::send`
- **依赖外部**：`Envelope`、`ErrorPayload`、`codes::*`、`WS_PROTOCOL_VERSION`、`encode_frame`
- **对外 API**：`pub(super) async fn handle_text`

### session/handlers.rs（≈ 165 行）
- **职责**：4 个命令处理器（除 send 外）。每个都是“解析 payload → 校验 → 改状态 → 写若干帧”。
- **包含**（原 221–371 + 479–481）：
  - `pub(super) async fn handle_hello`
  - `pub(super) async fn handle_join`
  - `pub(super) async fn handle_leave`
  - `pub(super) async fn handle_ping`
- **依赖内部**：`outbound::{send, send_ack_if_id, send_bad_payload, send_backlog_entry, send_room_state}`、`validate::valid_room_code`
- **依赖外部**：`DaemonState`、`SessionHandle`、`HelloPayload` / `RoomPayload`、`room_hash`、`codes::*`、`ErrorPayload`、`encode_frame`、`DAEMON_VERSION`
- **对外 API**：4 个 `pub(super) async fn` handler

### session/handlers_send.rs（≈ 115 行）
- **职责**：`handle_send` 单独成文件 — 它是唯一超过 100 行的命令处理器，单独放置可让 `handlers.rs` 保持在 200 行以内。
- **包含**（原 373–477）：
  - `pub(super) async fn handle_send`
- **依赖内部**：`outbound::{send, send_ack_if_id, send_bad_payload}`
- **依赖外部**：`DaemonState`、`SessionHandle`、`SendPayload`、`MessageType`、`codes::*`、`ErrorPayload`、`encode_frame`
- **对外 API**：`pub(super) async fn handle_send`

### session/events.rs（≈ 35 行）
- **职责**：把 `NodeEvent` 翻译成会话侧动作。当前只处理 PeerConnected / PeerDisconnected；留作未来扩展（例如：如果以后决定直接在会话里处理某类事件，改这里即可）。
- **包含**（原 483–518（含 doc comment））：
  - `pub(super) async fn forward_peer_event(socket, state, handle, ev) -> bool`
- **依赖内部**：`outbound::send_room_state`
- **依赖外部**：`NodeEvent`、`SessionHandle`、`DaemonState`
- **对外 API**：`pub(super) async fn forward_peer_event`

### session/outbound.rs（≈ 75 行）
- **职责**：所有“把东西写进 socket”的出口。统一错误格式、ack 策略、backlog/room_state 帧构造。
- **包含**（原 520–579）：
  - `pub(super) async fn send(socket, text) -> bool`
  - `pub(super) async fn send_ack_if_id(socket, id, ty) -> bool`
  - `pub(super) async fn send_bad_payload(socket, id, err) -> bool`
  - `pub(super) async fn send_backlog_entry(socket, entry) -> bool`
  - `pub(super) async fn send_room_state(socket, state, code, joined) -> bool`
- **依赖内部**：无（叶子）
- **依赖外部**：`WebSocket`、`Message`、`encode_frame`、`AckPayload` / `ErrorPayload` / `MessagePayload` / `RoomStatePayload` / `PeerInfo`、`codes::*`、`MessageEntry`、`DaemonState`
- **对外 API**：5 个 `pub(super) async fn`

### session/validate.rs（≈ 30 行）
- **职责**：room code 的结构校验。独立成文件因为它有两条专属单测，以及将来可能扩展更多校验规则。
- **包含**（原 581–602）：
  - `pub(super) fn valid_room_code(code: &str) -> bool`
  - `#[cfg(test)] mod tests { valid_room_code_accepts_six_digits, valid_room_code_rejects_length_and_non_digit }`
- **依赖**：无
- **对外 API**：`pub(super) fn valid_room_code`

### 行数校核
| 文件 | 预估行数 |
|---|---|
| mod.rs | ~25 |
| lifecycle.rs | ~100 |
| dispatch.rs | ~60 |
| handlers.rs | ~165 |
| handlers_send.rs | ~115 |
| events.rs | ~35 |
| outbound.rs | ~75 |
| validate.rs | ~30 |
| **合计** | ~605（< 602 基础上增加少量 `use` 导入重复，整体在可接受范围内） |

均 ≤ 200 行，且 `handlers.rs`（最大）在 200 行内有缓冲。

## 模块间依赖

### 与 DaemonState 的交互（拆后不变）
- `state.node`
  - `app_uuid()` → `lifecycle` / `handlers::handle_hello`
  - `add_room_hash` / `remove_room_hash` → `handlers::handle_join` / `handle_leave`
  - `peers()` → `outbound::send_room_state` / `handlers::handle_send`
  - `send_raw` → `handlers::handle_send`
- `state.node_events.subscribe()` → `lifecycle`
- `state.sessions` → `lifecycle`（注册 / 反注册） + `handlers::handle_leave`（判断是否仍被订阅）
- `state.rooms` → `handlers::handle_join` / `handle_leave`
- `state.queue` → `handlers::handle_join`（drain） / `handle_leave`（remove_room）

所有共享都通过方法参数 `&DaemonState` 传入，不新建全局。

### 与其他 daemon 模块的关系
- `protocol::*`：`Envelope`、各 Payload、`codes`、`encode_frame` 在 dispatch / handlers / outbound 多处引用（无变化）
- `queue::MessageEntry`：仅在 `outbound::send_backlog_entry` 与 `handlers::handle_join` 流过
- `rooms::RoomStore`：仅由 `handlers::handle_join` / `handle_leave` 通过 `state.rooms` 操作
- `lib.rs`：只依赖 `session::run`；通过 `mod.rs` re-export 保证调用点 `session::run(socket, state)` 不变（行 217）
- `mlink-core::core::node` / `::room` / `::protocol::types`：仅 lifecycle / handlers 使用
- fan-out worker（在 `lib.rs::build_state`）仍然读取 `SessionEntry::subs` + 写 `tx`；拆分不触及它

### 可见性约束
- 除 `run` 以外的函数全部使用 `pub(super)` 限定在 `session/` 内；不泄漏实现细节到 crate 其他模块。
- 常量 `MAX_FRAME_BYTES` / `OUTBOUND_CAPACITY` 停留在 `session/mod.rs`，只有 `lifecycle.rs` 需要它们（`use super::{MAX_FRAME_BYTES, OUTBOUND_CAPACITY}`）。

## 迁移步骤

按"从叶子到根"执行，每步后 `cargo build -p mlink-daemon` + `cargo test -p mlink-daemon` 必须全绿。

1. **建目录**：把 `session.rs` 改为 `session/mod.rs`，先原样迁移内容并跑通编译和测试（确认路径切换本身无副作用）。
2. **抽 validate.rs**：迁 `valid_room_code` + 它的两条单测 → `session/validate.rs`；`mod.rs` 里 `mod validate; use validate::valid_room_code;`。测试须仍在 `validate.rs` 内通过。
3. **抽 outbound.rs**：迁 `send` / `send_ack_if_id` / `send_bad_payload` / `send_backlog_entry` / `send_room_state`，`mod.rs` 用 `use outbound::*;`。此步影响面最大（被多个函数引用），单独一次迁移更易 code review。
4. **抽 events.rs**：迁 `forward_peer_event`，只被 `run` 引用。
5. **抽 handlers.rs**：迁 5 个 `handle_*`。它们依赖 `outbound` / `validate`，已在前面步骤就位。
6. **抽 dispatch.rs**：迁 `handle_text`。依赖 `handlers`，故放在 handlers 之后。
7. **抽 lifecycle.rs**：把 `run` 搬出去；`mod.rs` 留下 `pub use lifecycle::run;` + 两个常量 + 子模块声明。
8. **终检**：`cargo build --workspace` + `cargo test --workspace`，确认包括 daemon 集成测试在内无回归。

每一步都是"平移 + `use` 调整"，不改行为，不删测试，便于二分回滚。

## 风险点

1. **WebSocket 生命周期管理**
   - `run` 内 `select!` 的三路 cancel-safety 仍依赖 `StreamExt::next` / `mpsc::Receiver::recv` / `broadcast::Receiver::recv` 的既有保证。拆分不改调用点、不改分支顺序，风险低。仍需人工复核 `select!` 分支顺序在 `lifecycle.rs` 与原 `session.rs` 完全一致。
   - 反注册代码（`sessions.retain`）必须仍在 `run` 的 drop 路径上，不能被错误地挪进子函数导致提前反注册。

2. **状态一致性**
   - `handle_join` 的"原子订阅 + drain"要求 `queue` 锁和 `subs` 插入在同一临界区（原 289–296 行）。迁到 `handlers.rs` 时必须整体平移，不能把 `subs.insert` 拆到别处。**关键不变量**：持 queue 锁期间，fan-out worker 阻塞 → 此时 subs 无该 room 则 worker 一定没推过 → drain 出的就是全集。
   - `handle_leave` 的"仍被其他会话引用?"判断（原 351–355）同时读 `state.sessions` 和各 session 的 `subs`，若拆分引入中间函数，须确保锁释放顺序不变（先 subs 再 sessions）。

3. **可见性退化风险**
   - 现所有 helper 都是 private `async fn`。拆分成子模块后若不小心把 `pub(super)` 写成 `pub`，会让 `protocol` / `queue` 等无关模块看到，破坏封装。迁移 checklist 里要逐个函数确认为 `pub(super)`。

4. **常量位置**
   - `MAX_FRAME_BYTES` 和 `OUTBOUND_CAPACITY` 目前在 `session.rs` 文件头。若只迁到 `lifecycle.rs` 而其他子模块又引用它们会重复；设计将其收束到 `mod.rs` 作单一真相源，lifecycle 用 `super::MAX_FRAME_BYTES` 访问。

5. **测试定位**
   - `valid_room_code` 的两个单测必须随函数迁到 `validate.rs`，保持 `cargo test -p mlink-daemon` 可发现。`run` 本身没有直接单测（集成测试在 `crates/mlink-daemon/tests/` 或更外层），因此 `lifecycle.rs` 迁移后集成测试是唯一验证手段——迁移前务必确认当前集成测试覆盖 join/leave/send/ping/hello/oversize/bad_version/bad_json 八条路径，避免"代码搬了但没人看到坏"。若覆盖不足，优先补集成测试再拆分。

6. **协议版本耦合**
   - `dispatch.rs` 直接引用 `WS_PROTOCOL_VERSION`（定义在 `lib.rs`）。保持从 `crate::WS_PROTOCOL_VERSION` 导入即可，不要在 `session/` 内重新定义。
