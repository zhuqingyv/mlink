---
name: mlink-core node.rs split design
description: Split /crates/mlink-core/src/core/node.rs (1485 lines) into small focused modules under core/node/
author: node-designer
status: draft
---

# mlink-core/src/core/node.rs 拆分设计

## 当前结构分析

### 文件概览
- 总行数：**1485**
- 生产代码：约 **755 行**（L1–L802）
- 测试代码：约 **680 行**（L804–L1485，`#[cfg(test)] mod tests`）

### Node 结构体（L93–L103）

9 个字段，跨四个职责域：

| 字段 | 类型 | 职责域 |
|---|---|---|
| `config` | `NodeConfig` | 配置 |
| `app_uuid` | `String` | 身份 |
| `peer_manager` | `Arc<PeerManager>` | Peer 元数据 |
| `peer_states` | `Arc<RwLock<HashMap<String, PeerState>>>` | Peer 运行时状态 |
| `connections` | `Arc<Mutex<ConnectionManager>>` | 连接生命周期 |
| `trust_store` | `Arc<RwLock<TrustStore>>` | 安全 |
| `state` | `StdMutex<NodeState>` | 全局状态机 |
| `events_tx` | `broadcast::Sender<NodeEvent>` | 事件分发 |
| `room_hashes` | `StdMutex<HashSet<[u8;8]>>` | 房间成员关系 |

### 所有类型 / 常量

| 名称 | 行号 | 说明 |
|---|---|---|
| `HEARTBEAT_INTERVAL`, `HEARTBEAT_TIMEOUT` | L20–L21 | 心跳常量 |
| `NodeState` | L23–L33 | 全局/Peer 状态枚举 |
| `NodeConfig` | L35–L50 | 配置（含 Default） |
| `PeerState` | L52–L76 | Peer 运行时状态（seq、重连策略、aes_key 等） |
| `NodeEvent` | L78–L90 | 广播事件 |
| `Node` | L93–L103 | 主结构体 |
| `recv_once` (free fn) | L757–L802 | 后台 reader 专用：用 Arc 句柄读一帧 |

### impl Node 方法清单

| 方法 | 行号 | 职责 |
|---|---|---|
| `new` | L106–L126 | 构造 |
| `add_room_hash` | L131–L133 | **房间** |
| `remove_room_hash` | L136–L138 | **房间** |
| `room_hashes` | L140–L142 | **房间** |
| `wire_room_hash` | L147–L154 | **房间** |
| `state` | L156–L158 | 状态读取 |
| `app_uuid` | L160–L162 | 身份读取 |
| `config` | L164–L166 | 配置读取 |
| `subscribe` | L168–L170 | **事件** |
| `start` | L172–L182 | 生命周期 |
| `stop` | L184–L191 | 生命周期 |
| `peers` / `get_peer` | L193–L199 | Peer 查询 |
| `peer_state` | L201–L204 | Peer 状态读 |
| `has_peer` | L209–L211 | 连接查询 |
| `connect_peer` | L213–L357 | **握手（出站）** |
| `accept_incoming` | L363–L481 | **握手（入站）** |
| `attach_connection` | L489–L502 | 测试钩子 |
| `disconnect_peer` | L504–L523 | 连接生命周期 |
| `send_raw` | L525–L588 | **帧 I/O 发送** |
| `spawn_peer_reader` | L606–L654 | **后台 reader + 清理** |
| `recv_raw` | L656–L692 | **帧 I/O 接收** |
| `send_heartbeat` | L694–L696 | 心跳 |
| `check_heartbeat` | L698–L707 | 心跳 |
| `set_peer_aes_key` | L709–L715 | 安全 |
| `set_peer_state` (priv) | L717–L723 | Peer 状态写 |
| `role_for` | L725–L727 | 角色 |
| `mark_lost` / `mark_reconnecting` | L729–L741 | 事件 + 状态 |
| `connection_count` | L743–L746 | 连接查询 |
| `trust_store` | L748–L750 | 安全访问器 |

### 功能边界识别

读完后功能域清晰聚成 6 组：

1. **类型定义**：`NodeState / NodeConfig / PeerState / NodeEvent` + 心跳常量
2. **构造 + 生命周期**：`new / start / stop`
3. **房间成员关系**：`add_room_hash / remove_room_hash / room_hashes / wire_room_hash`
4. **握手**（重头）：`connect_peer / accept_incoming` — 两个方法各百行，含大量房间校验 + 幂等去重逻辑，模式高度相似，可提取一个私有 helper `register_after_handshake`
5. **帧 I/O**：`send_raw / recv_raw / spawn_peer_reader` + 自由函数 `recv_once`
6. **Peer/心跳/安全的零碎访问器**：`peers / get_peer / peer_state / set_peer_state / has_peer / disconnect_peer / send_heartbeat / check_heartbeat / set_peer_aes_key / role_for / mark_lost / mark_reconnecting / connection_count / subscribe / trust_store / state / app_uuid / config / attach_connection`

---

## 拆分方案

把单文件拆成一个**模块目录** `core/node/`，对外保持 `crate::core::node::{Node, NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_*}` 全部不变（`core/mod.rs` 已 `pub use` 这些，拆分后不动 mod.rs）。

所有子文件使用 `impl Node { ... }` 的分块实现，共享同一个 `Node` 结构体（Rust 允许跨文件多 `impl` 块）。

### 目标文件结构

```
crates/mlink-core/src/core/node/
├── mod.rs              # Node 结构体 + pub use + 构造 + 生命周期
├── types.rs            # NodeState / NodeConfig / PeerState / NodeEvent / 常量
├── rooms.rs            # impl Node { room_* / wire_room_hash }
├── connect.rs          # impl Node { connect_peer / accept_incoming / register_after_handshake(私有) }
├── io.rs               # impl Node { send_raw / recv_raw / spawn_peer_reader } + fn recv_once
├── lifecycle.rs        # impl Node { disconnect_peer / attach_connection / mark_lost / mark_reconnecting / state accessors }
└── tests.rs            # 全部 #[cfg(test)] 测试（保持单文件即可，已按场景分散，不值得再切）
```

> 不创建 `misc.rs`、`accessor.rs` 这种语义稀薄的文件。所有访问器都塞进 `lifecycle.rs` 或 `mod.rs`，按"它在什么生命周期节点被调用"聚合。

### 各文件详细设计

---

#### 1. `node/mod.rs` — Node 主入口

**职责**：定义 `Node` 结构体本身、构造、启停、模块声明与 `pub use`。

**迁移自原文件**：
- L1–L18 所有 `use`（但按子模块实际需要再分配）
- L93–L103 `struct Node`
- L105–L126 `impl Node { new }`
- L172–L191 `start / stop`
- L156–L166 `state / app_uuid / config` 三个最基础的访问器
- L168–L170 `subscribe`
- L725–L727 `role_for`（放这里因为它是 Node 身份的直接延伸，不依赖任何子系统）
- L743–L750 `connection_count / trust_store`（简短访问器，避免为 2 行独立分文件）

**导出**：
```rust
mod types;
mod rooms;
mod connect;
mod io;
mod lifecycle;
#[cfg(test)]
mod tests;

pub use types::{NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT};

pub struct Node { /* ... */ }

impl Node {
    pub async fn new(config: NodeConfig) -> Result<Self> { ... }
    pub fn state(&self) -> NodeState { ... }
    pub fn app_uuid(&self) -> &str { ... }
    pub fn config(&self) -> &NodeConfig { ... }
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> { ... }
    pub async fn start(&self) -> Result<()> { ... }
    pub async fn stop(&self) -> Result<()> { ... }
    pub fn role_for(&self, peer_app_uuid: &str) -> Role { ... }
    pub async fn connection_count(&self) -> usize { ... }
    pub fn trust_store(&self) -> Arc<RwLock<TrustStore>> { ... }
}
```

**预估行数**：~130 行（含字段文档与结构体定义）

---

#### 2. `node/types.rs` — 纯类型

**职责**：完全静态的类型声明，无 `impl Node`。

**迁移自原文件**：
- L20–L21 心跳常量
- L23–L33 `NodeState`
- L35–L50 `NodeConfig` + `Default`
- L52–L76 `PeerState` + `new` + `next_seq`
- L78–L90 `NodeEvent`

**导出**：全部 pub（`new` 和 `next_seq` 是 `pub(super)`，仅 `io.rs / connect.rs` 使用）。

**预估行数**：~80 行

---

#### 3. `node/rooms.rs` — 房间成员关系

**职责**：房间哈希集合的增删查 + 握手时挑一个上线广播的包装。

**迁移自原文件**：
- L131–L154 四个房间方法

**为什么独立**：`connect_peer / accept_incoming` 里的"snapshot-then-drop-StdMutex-guard 再过 `.await`"模式已是项目级规范（memory: `project_tcp_mdns_transport.md` 同款），值得单独一个文件让这组方法和注释聚焦。将来如果要做"多房间自动轮换"、"房间过期"等，改动面被严格约束在这里。

**预估行数**：~40 行

---

#### 4. `node/connect.rs` — 握手入口

**职责**：出站 `connect_peer` + 入站 `accept_incoming` + 提取共享的私有 helper。

**迁移自原文件**：
- L213–L357 `connect_peer`
- L363–L481 `accept_incoming`

**新增 helper（私有 `impl Node`）**：
```rust
/// Shared post-handshake bookkeeping:
///   1. room-membership check (returns Err(RoomMismatch) on failure)
///   2. duplicate-peer dedup (returns Ok(existing_id) if already connected)
///   3. register Peer + Connection + PeerState, flip Node state, emit PeerConnected
async fn register_after_handshake(
    &self,
    peer_hs: Handshake,
    conn: Box<dyn Connection>,
    name: String,
    transport_id: &str,
) -> Result<String> { ... }
```

两个 200 行的姊妹方法目前各自拷贝了"snapshot room → check → dedup → register"同一套逻辑（原 L282–L357 与 L409–L480），提取 helper 后每个方法只剩 ~50 行的"组装 local_hs + 执行 handshake + 错误路径 close"。

**预估行数**：~180 行（含 helper + 两个简化后的方法 + 大段必要注释）

> 注：这**会超过 200 行**么？看起来不会，因为原文件 connect_peer + accept_incoming 合计 ~260 行里 ~130 行是 eprintln 日志和房间校验（提取后一处），净逻辑 130 行。

**风险（见下）**：helper 里的 `conn: Box<dyn Connection>` 所有权转移必须小心。

---

#### 5. `node/io.rs` — 帧读写

**职责**：加密/压缩/编解码的上层包装。

**迁移自原文件**：
- L525–L588 `send_raw`
- L606–L654 `spawn_peer_reader`
- L656–L692 `recv_raw`
- L757–L802 自由函数 `recv_once`（保持 `pub(super) async fn`，仅 `spawn_peer_reader` 使用；也允许被测试访问）
- L694–L707 `send_heartbeat / check_heartbeat`（I/O 的语义对偶）

**为什么把心跳放这里**：`send_heartbeat` 是 `send_raw` 的一行封装；`check_heartbeat` 读 `PeerState.last_heartbeat`，都紧贴帧层，不值得单开文件。

**预估行数**：~170 行（含 `recv_once` 与注释）

---

#### 6. `node/lifecycle.rs` — Peer 生命周期与状态访问

**职责**：围绕"peer 进入/离开/刷新状态"的一组轻量方法。

**迁移自原文件**：
- L193–L204 `peers / get_peer / peer_state`
- L209–L211 `has_peer`
- L489–L523 `attach_connection / disconnect_peer`
- L709–L723 `set_peer_aes_key / set_peer_state`
- L729–L741 `mark_lost / mark_reconnecting`

**预估行数**：~120 行

---

#### 7. `node/tests.rs` — 测试

**职责**：原 `#[cfg(test)] mod tests`（L804–L1485）原封不动迁入，只把 `use super::*;` 改成 `use super::super::*;`（从 `node/tests.rs` 看 `Node` 在 `node/mod.rs`）。

**为什么不再切分**：
- 测试已按"场景"组织（idle/start/accept/send 不阻塞 reader/reader EOF 清理…），单独一个 ~680 行的 tests.rs 完全 OK —— **生产代码拆分不要求测试同步拆分**。
- 切分测试会增加维护成本（HOME_LOCK / HomeGuard / tmp_config 公共助手都要跟着复制或再拆一个 `tests/common.rs`），收益为零。

**预估行数**：~680 行（**超过 200 行，但属于测试代码**；如果 200 行硬指标也必须覆盖测试，可再拆成 `tests/handshake.rs / tests/io.rs / tests/rooms.rs`，但需先和 leader 对齐此目标是否包含测试）

---

### 行数汇总

| 文件 | 生产代码行数 |
|---|---|
| `mod.rs` | ~130 |
| `types.rs` | ~80 |
| `rooms.rs` | ~40 |
| `connect.rs` | ~180 |
| `io.rs` | ~170 |
| `lifecycle.rs` | ~120 |
| **合计** | **~720** |
| `tests.rs` | ~680 |

全部生产代码文件 **≤ 200 行**（`connect.rs` 最接近，需要在实施阶段仔细控制日志注释长度）。

---

## 模块间依赖

### Node 字段的访问模式

拆分后所有子文件都是 `impl Node { ... }`，**直接通过 `&self` 访问所有字段** —— 不需要引入 `Arc` 或中间对象。Rust 原生支持跨文件多 impl 块，字段可见性不受文件划分影响。

唯一例外是 `recv_once`（自由函数，不是 `impl Node` 成员），它接收 `Arc<Mutex<ConnectionManager>>` 和 `Arc<RwLock<HashMap<String, PeerState>>>` —— 保持原签名，`spawn_peer_reader` 在 `io.rs` 内调用它。

### 哪些类型被多个子模块使用

| 类型 | 使用方 |
|---|---|
| `NodeState` | `mod.rs`（`state`）、`lifecycle.rs`（`set_peer_state`, `attach_connection`）、`connect.rs`（握手后 flip 状态）、`io.rs`（reader 清理） |
| `PeerState` | `io.rs`（`send_raw` 读 seq/aes_key，`recv_once` 读 aes_key）、`lifecycle.rs`（`attach_connection / set_peer_state / set_peer_aes_key`）、`connect.rs`（`register_after_handshake` 插入） |
| `NodeEvent` | `mod.rs`（`subscribe`）、`connect.rs`（`PeerConnected`）、`io.rs`（`MessageReceived / PeerDisconnected`）、`lifecycle.rs`（`mark_lost / mark_reconnecting / disconnect_peer`） |
| `HEARTBEAT_TIMEOUT` | `io.rs`（`check_heartbeat`） |
| `Role`（外部） | `mod.rs`（`role_for`）、`connect.rs`（`connect_peer` 中 `negotiate_role` 日志） |

**结论**：`types.rs` 是全局被依赖的底座；其他子文件互相 **零依赖** —— 全部通过 `&self.xxx` 间接访问共享字段。

---

## 迁移步骤

按"构建/测试每步都绿"的原则分 7 个 PR 或 commit 递进，每步可独立回滚。

**Step 0**：基线跑一次 `cargo test -p mlink-core`，确保拆分前全绿作为参照。

**Step 1**：新建目录 `crates/mlink-core/src/core/node/`，创建空的 `mod.rs`，把原 `node.rs` 内容原样粘入 `node/mod.rs`，删除旧 `node.rs`，`core/mod.rs` 中 `pub mod node;` 无需改动（目录形式的模块等价）。
- 验证：`cargo test -p mlink-core` 仍全绿。

**Step 2**：抽离 `types.rs`，在 `mod.rs` 里 `mod types; pub use types::*;`，把 `NodeState / NodeConfig / PeerState / NodeEvent / HEARTBEAT_*` 迁过去。
- 验证：`cargo build` + `cargo test`。

**Step 3**：抽离 `rooms.rs`，迁入 4 个 room_* 方法。
- 验证：`cargo test room`（聚焦房间相关测试）+ 全量 `cargo test`。

**Step 4**：抽离 `lifecycle.rs`，迁入 peer 生命周期访问器。
- 验证：同上。

**Step 5**：抽离 `io.rs`，迁入 `send_raw / recv_raw / spawn_peer_reader / recv_once / send_heartbeat / check_heartbeat`。
- 验证：聚焦跑 `send_raw_not_blocked_by_parked_reader`、`spawn_peer_reader_emits_message_received_event`、`reader_eof_cleans_up_connection_and_peer_manager` 三个关键并发回归测试。

**Step 6**：抽离 `connect.rs`，迁入 `connect_peer / accept_incoming`，**先保留原两份实现**，再**第二步才提取** `register_after_handshake` helper。
- 验证：`cargo test accept_incoming`、`cargo test connect_peer`、`cargo test dedup`。

**Step 7**：把 `#[cfg(test)] mod tests` 移出到 `node/tests.rs`，`mod.rs` 中 `#[cfg(test)] mod tests;`，改测试 import。
- 验证：`cargo test -p mlink-core` 全绿收尾。

**为什么这个顺序**：
- Step 2 先抽类型 —— 后续所有 impl 文件都要 `use super::types::*`，基础先打好。
- Step 6 把最大 / 最危险的 connect 模块留到末尾，此时别的模块已证明分文件方案可行。
- 测试文件最后迁，避免每一步都被 `use super::*;` 路径变化牵连。

---

## 风险点

### 1. `connect_peer` / `accept_incoming` 的 helper 提取

**风险**：两方法里日志量极大（`eprintln!("[mlink:conn] ..."` 遍布），且对 `!Send MutexGuard + .await` 的避雷显式依赖 snapshot 模式（见原 L279–L291、L405–L418 的注释）。提取 helper 时必须保留：
- StdMutex 的 drop 顺序（helper 内部不能持有 `room_hashes.lock()` 跨 `.await`）
- `Box<dyn Connection>` 只能 move 一次（helper 失败路径需自己 `conn.close().await`，成功路径转交所有权）
- 日志 site 信息（原本 `eprintln` 里写了 "connect_peer" / "accept_incoming" 以区分上下文，helper 需接收一个 `site: &'static str` 参数或保留在调用方打印）

**缓解**：Step 6 分两步，先纯搬迁不抽 helper，再单独一个 commit 抽 helper 并保持行为一致。每一步独立跑完整测试套件。

### 2. 并发相关拆分风险

**要保护的不变量**（代码里大量注释点出了这些）：
- `connections.lock().await` 的 Tokio Mutex 不能跨 `conn.read().await / conn.write().await` 持有（会复发 BLE chat 死锁 —— memory: `project_connection_ownership_pattern.md`）
- `room_hashes` 的 **std::Mutex** guard 不能跨任何 `.await` 活着（`tokio::spawn` 要求 `Send`，`StdMutexGuard` 不 `Send`）
- `PeerState` 的 `seq` 是写锁下 `next_seq()` 得到后立刻 drop，再做帧编码与 write —— 保证单调递增且不阻塞其他 peer

**缓解**：
- 每个子文件迁移后必须 **单独跑一次 `send_raw_not_blocked_by_parked_reader`** 确认锁粒度没退化
- PR 描述里逐点列出上述三个不变量作为 code review checklist

### 3. trait 实现拆分

本次拆分 **不涉及** 任何 trait impl（Node 本身不 impl 任何 trait，`Connection / Transport` 都是外部依赖），这是幸运处。唯一需要注意：`ConnectionManager` 在 `connection.rs`，我们不动它；`Node` 使用 `ConnectionManager` 的所有点在 `io.rs / connect.rs / lifecycle.rs / mod.rs` 分散，**不要误把 `ConnectionManager` 再塞进 node/ 目录**。

### 4. public API 兼容性

**断言**：拆分前后，`crate::core::{Node, NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT}` 的 **路径完全不变**。因为：
- `core/mod.rs` 已有 `pub use node::{Node, NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT};`
- 把 `node.rs` 变成 `node/mod.rs`，并在 `node/mod.rs` 里 `pub use types::*;` —— 外部引用零修改

验证办法：拆分前后搜一次 `rg "core::node::" --type rust` 与 `rg "use crate::core::node" --type rust`，对比无路径变化。

### 5. 测试文件的 HOME_LOCK

测试里 `static HOME_LOCK: Mutex<()>` 用于串行化 `HOME` env 变量的测试，**不要拆分测试文件**（拆分后 static 会按文件复制，串行化就失效了）。上面 Step 7 保持测试单文件正是出于这个考虑。

---

## 未决问题（需 leader 明确）

1. **200 行硬指标是否包含 `#[cfg(test)]`**？如果包含，`tests.rs` 要再拆；不包含则保持单文件。默认按"不包含"设计。
2. `connect.rs` 预估 180 行接近上限，若 helper 抽离后仍超，是否允许按"连接方向"再拆成 `connect/outbound.rs + connect/inbound.rs + connect/register.rs`？（我倾向不拆 —— 一个文件能让读者看到出/入两条路径的对称性，对维护更友好。）
3. 是否希望同时把 `mod.rs` 里的 `role_for / connection_count / trust_store` 这些 3–5 行的小访问器再抽一个 `accessors.rs`？我的建议是 **不抽** —— 语义稀薄、仅为凑行数的文件反而降低可读性。
