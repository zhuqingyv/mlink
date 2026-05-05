# Dual-Transport INTERFACE CONTRACTS (arch-a v1)

> **只读权威。** 所有 Wave 1 / Wave 2 模块只准 `use` 这里冻结的签名，不准改名 / 加字段 / 换返回类型。
> 任何接口变更必须先改本文 → 两架构师评审 → 同步 TASK-LIST。
> 修订注写在本文档顶部 CHANGELOG 区，**不要**在各模块 README 里开分支。

---

## 0. 术语与命名约束

| 词 | 意义 | 禁用同义词 |
| --- | --- | --- |
| `peer_id` | 远端 `app_uuid`（handshake 后的身份） | `peer_app_uuid` / `remote_id` |
| `link_id` | 单条物理连接的本地唯一 id（`"{transport_id}:{wire_id}:{nonce}"`） | `channel_id` / `conn_id` |
| `transport_id` | 由 `Transport::id()` 返回（`"tcp"` / `"ble"` / `"mock"`） | `transport_kind` / `tx` |
| `primary` / `standby` | 逻辑角色 —— 当前承载 write 的是 primary | `active` / `passive` |
| `seq` | `Frame.seq`（`u16`，已 wrapping_add） | 不许换名换宽 |
| `ack_watermark` | 对端 `Handshake.last_seq` + 累计 Ack 推进出的"已收到 ≤ X"窗口 | —— |

> BLE / TCP 是当前唯二物理传输；下文接口不硬编码这两个字面量，改用 `transport_id`。

---

## 1. `Link` —— 单条物理连接的状态包装（Wave 1）

**位置：** `crates/mlink-core/src/core/link/mod.rs`（新增，≤120 行）

```rust
use std::sync::Arc;
use std::time::Instant;

use crate::transport::Connection;

/// 健康状态 —— 状态机由 health-checker 推进，link 自身只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// 刚建链，尚未收到第一个心跳。
    Probing,
    /// 心跳正常，可承载 write。
    Healthy,
    /// 最近一次心跳超时，但连接尚未关闭 —— 已被降级，不再作 primary。
    Degraded,
    /// 物理连接已断（read EOF / close）。
    Dead,
}

#[derive(Debug, Clone)]
pub struct LinkMetrics {
    pub connected_at: Instant,
    pub last_rx_at: Option<Instant>,
    pub last_heartbeat_rx: Option<Instant>,
    pub rtt_ewma_ms: Option<f32>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub health: LinkHealth,
}

/// 一条物理连接 + 运行时统计。Link 不关心协议，只持有 Arc<dyn Connection>
/// 与元数据；seq / ack / resume 全部在 peer 层（Wave 2）推进。
pub struct Link {
    link_id: String,
    peer_id: String,
    transport_id: String,
    conn: Arc<dyn Connection>,
    metrics: parking_lot::RwLock<LinkMetrics>,
}

impl Link {
    pub fn new(
        link_id: String,
        peer_id: String,
        transport_id: String,
        conn: Arc<dyn Connection>,
    ) -> Self { /* ... */ }

    pub fn id(&self) -> &str;
    pub fn peer_id(&self) -> &str;
    pub fn transport_id(&self) -> &str;
    pub fn conn(&self) -> Arc<dyn Connection>; // clone the Arc
    pub fn metrics(&self) -> LinkMetrics;       // snapshot, cheap read-lock
    pub fn set_health(&self, h: LinkHealth);    // 仅 health-checker 调用
    pub fn record_rx(&self, bytes: usize);      // io 层每次 read() 命中后调用
    pub fn record_tx(&self, bytes: usize);      // send_raw 成功后调用
    pub fn record_heartbeat_rx(&self);
    pub fn record_rtt_sample(&self, rtt_ms: u32);
}
```

**规则**

- `Link` 只是 `Arc<dyn Connection>` + 元数据，**不**实现 `Connection`。
- `metrics` 用 `parking_lot::RwLock`（同步锁）足够 —— 读路径极热，写只在 io/health 两处。
- `link_id` 由 `LinkPool::next_id()` 生成，格式 `"{transport_id}:{wire_id}:{nonce_u32}"`，人类可读便于前端显示。
- Link **不拥有** Connection 的读任务；读任务由 peer 层 spawn 后把字节灌给协议层。

---

## 2. `LinkPool` —— 同一 peer 的多 Link 管理（Wave 1）

**位置：** `crates/mlink-core/src/core/link/pool.rs`（≤180 行）

```rust
use std::sync::Arc;
use std::collections::HashMap;

use crate::core::link::{Link, LinkHealth};

/// 一个 peer 对应一个 LinkPool。内部最多同时持有 N 条 Link（N=2 即 BLE+TCP）。
/// 选择策略：primary 由外部 selector 决定，pool 只保存映射。
pub struct LinkPool {
    peer_id: String,
    links: HashMap<String, Arc<Link>>,
    primary_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// primary 切换成功，返回新 primary 的 link_id。
    Promoted,
    /// 未发生切换（已经是目标 primary，或目标 link 不存在）。
    NoChange,
    /// 池内没有任何 Healthy link，promote 无能为力。
    NoHealthy,
}

impl LinkPool {
    pub fn new(peer_id: String) -> Self;

    /// 幂等：link_id 相同则替换（同 transport 重连）。返回是否首次加入。
    pub fn add(&mut self, link: Arc<Link>) -> bool;

    /// 移除 link；若被移除的是 primary，自动从剩余 Healthy 中挑一条
    /// （挑选策略：transport_id 字典序小者优先，保证两端一致）。
    pub fn remove(&mut self, link_id: &str) -> Option<Arc<Link>>;

    pub fn get(&self, link_id: &str) -> Option<Arc<Link>>;
    pub fn primary(&self) -> Option<Arc<Link>>;
    pub fn standby(&self) -> Option<Arc<Link>>;           // 第一条非 primary 的 Healthy
    pub fn all(&self) -> Vec<Arc<Link>>;
    pub fn healthy(&self) -> Vec<Arc<Link>>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn peer_id(&self) -> &str;

    /// 指定 link_id 升为 primary；被选中的 link 必须在池内且 Healthy。
    pub fn promote(&mut self, link_id: &str) -> PromoteOutcome;

    /// 无条件把 primary 指向"现存最健康的非 target link"，返回新的 primary_id。
    /// 用于 primary Dead 后的被动切换。
    pub fn failover_away_from(&mut self, dead_link_id: &str) -> PromoteOutcome;
}
```

**规则**

- `LinkPool` 不自己持锁；外层 `ConnectionManager` 用 `Mutex<LinkPool>` 包住它。
- 去重粒度：`(peer_id, transport_id)` 唯一 —— 同 transport 重连时用新 link 替换旧 link（旧 link 先 `close()`）。
- `promote` 纯状态变更，不触发 write；真正的切换由 io 层在下一次 `send_raw` 读到新 primary 时生效。

---

## 3. `ConnectionManager` —— peer → LinkPool 映射（Wave 2 改造）

**位置：** `crates/mlink-core/src/core/connection.rs`（已存在，改造 ≤80 行）

```rust
// 原签名（保留，Wave 1 不删）
pub fn add(&mut self, id: String, conn: SharedConnection);
pub fn remove(&mut self, id: &str) -> Option<SharedConnection>;
pub fn shared(&self, id: &str) -> Option<SharedConnection>;

// 新增（Wave 2）
impl ConnectionManager {
    /// 挂一条新 Link 到 peer 下。若 peer 尚无 pool 则创建；
    /// 返回 pool 操作结果（AddResult::Added / Replaced / Rejected）。
    pub fn attach_link(&mut self, peer_id: &str, link: Arc<Link>) -> AddResult;

    /// 摘掉一条 Link；若摘完后 pool 为空，**也要移除 pool 自身**并返回 true
    /// —— 这是 Node::disconnect_peer 判断"整个 peer 断了"的唯一依据。
    pub fn detach_link(&mut self, peer_id: &str, link_id: &str) -> DetachOutcome;

    pub fn pool(&self, peer_id: &str) -> Option<&LinkPool>;
    pub fn pool_mut(&mut self, peer_id: &str) -> Option<&mut LinkPool>;

    /// 向后兼容：现有 send_raw 从这里取"当前 primary 的 Arc<dyn Connection>"。
    /// 无 primary（例如切换中间态）时返回 None —— 上层应排队重传而不是丢消息。
    pub fn primary_conn(&self, peer_id: &str) -> Option<SharedConnection>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddResult {
    Added,      // 全新 peer 或新 transport
    Replaced,   // 同 (peer, transport) 旧 link 被替换
    Rejected,   // 容量已满（MAX_LINKS_PER_PEER=4 硬上限，防脑洞）
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachOutcome {
    Detached,        // 还有其他 link
    PeerGone,        // 这是最后一条，pool 已删除
    NotFound,
}
```

**规则**

- 保留 `shared(id)`：Wave 2 的 io 层迁到 `primary_conn(id)` 后再删 `shared`。
- `primary_conn` = `pool.primary().map(|l| l.conn())`。
- `MAX_LINKS_PER_PEER = 4` 写死在 `connection.rs` 顶部，留余量给未来 Wi-Fi Direct / Thread。

---

## 4. `RetransmitQueue` —— 未 ack 缓存（Wave 1）

**位置：** `crates/mlink-core/src/core/retransmit.rs`（≤180 行）

```rust
use std::time::Instant;

/// 未 ack 消息。payload 是完整 Frame 字节（encode 后），重传时直接 write。
#[derive(Debug, Clone)]
pub struct PendingFrame {
    pub seq: u16,
    pub bytes: Vec<u8>,
    pub first_sent_at: Instant,
    pub last_sent_at: Instant,
    pub resent_count: u8,
    pub msg_type: crate::protocol::types::MessageType,
}

/// 滑动窗口缓存未 ack 的 outbound 帧。每 peer 一个队列。
/// 窗口大小 `MAX_PENDING`（default 256）用于背压：塞满后 `push` 返回 Err，
/// 调用方必须等 ack 或超时清理。
pub struct RetransmitQueue {
    peer_id: String,
    pending: std::collections::VecDeque<PendingFrame>,
    max_pending: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Queued,
    Full,          // 调用方应返回 Backpressure 给上层
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckOutcome {
    /// 被 ack 清掉的帧数。
    pub cleared: usize,
    /// 推进后的 watermark。
    pub watermark: u16,
}

impl RetransmitQueue {
    pub fn new(peer_id: String, max_pending: usize) -> Self;

    /// 记录一条刚 write 出去的帧。已满时不入队。
    pub fn push(&mut self, frame: PendingFrame) -> PushResult;

    /// 对端通过 Ack / Handshake.last_seq 告知已收到 ≤ watermark 的序号。
    /// `seq` 的 wrapping 对比用 `seq_le(a, b)` 小工具判定。
    pub fn ack_through(&mut self, watermark: u16) -> AckOutcome;

    /// 返回所有超过 `older_than` 未 ack 的帧快照（不出队）—— retransmit
    /// 循环每次拿这个列表调 send_raw 重发。
    pub fn due_for_retry(&self, now: Instant, older_than: std::time::Duration) -> Vec<PendingFrame>;

    /// 重传后更新 `last_sent_at` 与 `resent_count`。
    pub fn mark_resent(&mut self, seq: u16, now: Instant);

    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn peer_id(&self) -> &str;
}

/// u16 wrapping 比较：a <= b？(当 a,b 差距 <= 32768 时判定为相邻窗口)
pub fn seq_le(a: u16, b: u16) -> bool;
pub fn seq_lt(a: u16, b: u16) -> bool;
```

**规则**

- `RetransmitQueue` **不自己起定时器**；由 peer 层（Wave 2）在每次 tick / heartbeat 里调 `due_for_retry`。
- `ack_through` 处理 wrap：窗口语义，非线性。
- `MAX_PENDING=256` 够 1 MB 帧上限 × 256 = 256 MB 理论上限，实际远小 —— 到上限说明对端卡死，应触发 failover 而非继续攒。

---

## 5. `HealthChecker` —— 心跳与健康度判定（Wave 1）

**位置：** `crates/mlink-core/src/core/health.rs`（≤140 行）

```rust
use std::time::{Duration, Instant};

/// 按 Link 独立评估健康。纯函数 + 状态机，不持 tokio 任务。
/// 谁调：peer 层 heartbeat tick。
pub struct HealthPolicy {
    pub heartbeat_interval: Duration,   // 发 ping 周期，default 5s
    pub unhealthy_after: Duration,      // 连续多久没收包 → Degraded，default 15s
    pub dead_after: Duration,           // 再多久不恢复 → Dead，default 30s
    pub hysteresis_healthy_probes: u8,  // 连续 N 个 ping-pong 回归才算 Healthy，default 3
}

impl Default for HealthPolicy { /* 给出上面的默认值 */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthVerdict {
    Unchanged,
    Changed(crate::core::link::LinkHealth),
}

pub struct HealthChecker {
    policy: HealthPolicy,
    consecutive_ok: u8,
}

impl HealthChecker {
    pub fn new(policy: HealthPolicy) -> Self;

    /// 单步推进：输入当前 link 的 metrics 快照 + 当前时间，
    /// 输出是否需要改状态。调用方拿 verdict 再调 `link.set_health()`。
    pub fn evaluate(
        &mut self,
        metrics: &crate::core::link::LinkMetrics,
        now: Instant,
    ) -> HealthVerdict;
}
```

**规则**

- `HealthChecker` **不碰 socket** —— 只看 metrics + 时间；可纯单测。
- "是否该发 ping" 由 peer 层据 `policy.heartbeat_interval` 决定，HealthChecker 不负责调度。

---

## 6. `LinkSelector` —— primary 选路（Wave 1）

**位置：** `crates/mlink-core/src/core/selector.rs`（≤100 行）

```rust
use std::sync::Arc;

use crate::core::link::{Link, LinkHealth};

/// 选路策略：同等 Healthy 情况下按"带宽优先"排序。
/// BLE throughput_bps ~ 5e4, TCP ~ 1e9，因此 TCP 自然胜出。
/// 选 transport 看 `Link::transport_capabilities()`（封装了 Transport::capabilities）。
pub trait LinkSelector: Send + Sync {
    fn pick(&self, links: &[Arc<Link>]) -> Option<Arc<Link>>;
    fn name(&self) -> &'static str;
}

/// 默认策略：Healthy 优先；多个 Healthy 按 throughput_bps 降序；平局按 transport_id 字典序。
pub struct BandwidthFirstSelector;

impl LinkSelector for BandwidthFirstSelector { /* ... */ }

/// 测试与手动指定用：完全按指定 transport_id 排序。
pub struct ExplicitSelector {
    pub priority: Vec<String>, // e.g. ["tcp", "ble"]
}

impl LinkSelector for ExplicitSelector { /* ... */ }
```

**规则**

- selector **只返回 link，不改 pool**。promote 动作由 peer 层调 `pool.promote(link_id)` 完成。
- 保留手动指定接口：前端 debug UI 可调用 `set_preferred_transport(peer_id, "ble" | "tcp")` 强制切换。

---

## 7. `DualTransportCoordinator` —— 并行 discover + listen（Wave 2）

**位置：** `crates/mlink-daemon/src/discovery/dual.rs`（≤200 行）

```rust
use std::sync::Arc;

use mlink_core::core::node::Node;

use crate::discovery::DaemonTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DualMode {
    /// 单通道：按 env 变量选的那个（保持旧行为）。
    Single(DaemonTransport),
    /// 双通道：TCP + BLE 同时跑，任一方 handshake 成功都建 link。
    /// Bond 逻辑由 Node 侧按 peer_id 合并 —— coordinator 不 merge。
    Both,
}

impl DualMode {
    /// `MLINK_DAEMON_TRANSPORT=dual` 时返回 `Both`；否则退回旧语义。
    pub fn from_env() -> Self;
}

/// 并行 spawn TCP 与 BLE 两套 discovery，共享同一个 `Arc<Node>`。
/// 两边分别走自己的 listener / scanner / connect_loop。
/// 合并靠 Node 内部按 `app_uuid` 聚合到同一 LinkPool 实现（见 §8）。
pub fn spawn_dual(node: Arc<Node>, mode: DualMode);
```

**规则**

- coordinator 只负责并发启两个现有 `discovery::spawn_*`，**不**做 link 归并。
- 归并通过"两边 handshake 返回同一个 peer_id" 自然命中 LinkPool 的 `add`，无需额外逻辑。

---

## 8. `Node` 的双通道行为变化（Wave 2）

**位置：** `crates/mlink-core/src/core/node/connect.rs` / `io.rs`（改造）

- `register_after_handshake` 原"已连就拒绝"改为"已连且 transport 相同 → 替换；已连且不同 transport → attach 为 standby"。签名不变：
  ```rust
  // 签名冻结 —— Wave 2 实现改变但参数不变
  async fn register_after_handshake(
      &self, peer_hs: Handshake, conn: Box<dyn Connection>,
      name: String, transport_id: &str, site: &'static str, mismatch_peer_id: String,
  ) -> Result<String>
  ```
- `send_raw` 取连接从 `guard.shared(peer_id)` 改为 `guard.primary_conn(peer_id)`。primary 为空时返回新错误：
  ```rust
  // protocol/errors.rs 新增
  pub enum MlinkError {
      // ...
      NoHealthyLink { peer_id: String },
  }
  ```
- `disconnect_peer(peer_id)` 改为显式"拆整个 peer"；新增 `disconnect_link(peer_id, link_id)` 只拆单条。
- `spawn_peer_reader` 改为 `spawn_link_reader(peer_id, link_id)` —— 每条 link 一个独立 reader；EOF 时只 detach 该 link，而不清整个 peer。

## 9. 重传/Ack 协议扩展（Wave 1 / Wave 2）

**位置：** `crates/mlink-core/src/protocol/types.rs`（字段扩展，无新 MessageType）

- 沿用现有 `MessageType::Ack`（0x03）。Ack payload：
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct AckPayload {
      /// 累计 ack —— 表示"seq ≤ watermark 的消息已全部收到"。
      pub watermark: u16,
      /// 可选 SACK 位图（未来优化；Wave 1 只实现 watermark，bitmap 空即可）。
      #[serde(default)]
      pub bitmap: Vec<u8>,
  }
  ```
- Frame.seq 不变（u16 wrapping）。
- `Handshake.last_seq` 保留，resume 流程不变 —— 新增 link 接入时，如果对端在 `last_seq` 里带了水位，本端把 `RetransmitQueue` 里 ≤last_seq 的帧先扫掉，再把剩下的通过新 link 重发。

**规则**

- **不新增 MessageType**，保持向后兼容。
- 旧客户端不发 Ack？—— 本端 `RetransmitQueue` 会累积到 `MAX_PENDING` 触发 `Backpressure`；这是我们"旧客户端降级为单通道"的自动开关：单通道路径下 send 成功即视为已达，直接 `ack_through(seq)`，队列永不涨。

## 10. `CapabilitiesDowngrade` —— 握手级降级（Wave 1）

**位置：** `crates/mlink-core/src/protocol/capabilities.rs`（新增 ≤80 行）

```rust
/// Handshake 阶段交换的能力位图。
/// 目前只保留 1 个位：双通道+累积 Ack。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    /// 对端支持"同一 peer 多 link" 与 cumulative Ack。
    pub dual_link: bool,
}

impl Capabilities {
    /// 双方能力按位取 AND —— 任何一方不支持即降级。
    pub fn negotiate(local: Self, remote: Self) -> Self;
}
```

**扩展 Handshake：**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    // ...原字段...
    #[serde(default)]
    pub capabilities: Capabilities,
}
```

**规则**

- 老客户端序列化不带 `capabilities` 字段，serde `default` 给 `{ dual_link: false }` —— 自动降级。
- 降级后 Node 侧走旧逻辑：同 peer 拒绝第二条连接，`primary_conn` = `shared`。

## 11. WS 协议扩展（Wave 2）

**位置：** `crates/mlink-daemon/src/protocol.rs`（追加，旧 frame 不改）

新增下行事件（不引新 type，复用 `room_state` 已有不够 —— 需新增 4 个）：

| type | 用途 | payload |
| --- | --- | --- |
| `link_added` | 某 peer 新接入一条 link | `{ peer_id, link_id, transport_id, health }` |
| `link_removed` | 某 peer 的某条 link 下线 | `{ peer_id, link_id, reason }` |
| `link_health` | 某条 link 健康度变化 | `{ peer_id, link_id, health, metrics }` |
| `primary_changed` | peer 的 primary link 切换 | `{ peer_id, old: link_id?, new: link_id, reason }` |

新增上行命令：

| type | 用途 | payload |
| --- | --- | --- |
| `set_preferred_transport` | debug UI 手动指定 primary | `{ peer_id, transport_id }` |
| `list_links` | 一次性拉所有 link 状态 | `{}` |
| `disconnect_link` | 强制断一条 link（故障注入） | `{ peer_id, link_id }` |

**规则**

- 新下行事件在 `forward_peer_event` 外新开 `forward_link_event`，不污染原 room_state 路径。
- 旧 WS 客户端不订阅这些 type，广播时直接忽略不识别的 frame（旧逻辑已是这样）——向后兼容无成本。

## 12. Web Debug UI 扩展（Wave 2）

**位置：** `examples/web-debug/index.html`（现 730 行，改造 ≤300 行增量）

- 同 peer 下按 `link_id` 渲染多行状态表：transport / health / rtt / bytes。
- 显示当前 primary 高亮。
- 两个按钮：`Force BLE primary` / `Force TCP primary` 调 `set_preferred_transport`。
- 一个"模拟抖动"按钮调 `disconnect_link`（仅 dev mode）。
- 顶部显示 `primary_changed` 事件流的最近 10 条，用作稳定性眼睛。

**前端→Daemon 协议边界**

- 所有新 type 走已有 `/ws` 通道；不新开路由。
- 前端支持旧版 daemon：收到 `bad_type` 错误时静默降级，隐藏双通道面板。

---

## CHANGELOG

- 2026-04-25 v1 —— arch-a 初版冻结。待 arch-b 评审。
