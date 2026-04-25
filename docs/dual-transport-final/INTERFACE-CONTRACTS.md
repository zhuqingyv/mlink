# Interface Contracts（最终冻结版，dual-transport-final）

> **这是唯一权威。** 所有 Wave 1 / Wave 2 / Wave 3 代码只准 `use` 这里冻结的签名；
> 改名、加字段、换返回类型、换宽度都不行。需要变更 → 先改本文 → broadcast → 再动代码。
>
> 核心决策（team-lead 定稿）：Session 分治主体 + scheduler 内联进 session/io + 硬上限 4 link +
> hysteresis=3 + ExplicitSelector 10 分钟过期。

---

## 0. 术语

| 词 | 意义 | 禁用同义词 |
| --- | --- | --- |
| `peer_id` | 远端 `app_uuid`（握手后身份） | `peer_app_uuid` / `remote_id` |
| `link_id` | 单条物理连接本地唯一 id，形如 `"{kind}:{peer_short}:{nonce_u32}"` | `channel_id` / `conn_id` |
| `TransportKind` | 枚举 `Ble | Tcp | Ipc | Mock`，用于匹配与去重 | 字符串 `"ble"` 不在 Rust 内部用 |
| `transport_id` | WS 线协议上的字符串形态 `"ble"|"tcp"|"ipc"` | 不在 Rust 内部比较 |
| `session_id` | 16 字节随机，首条 link 建立时生成；第二条 link 握手里声明附加到同一 session | —— |
| `active` / `standby` | 逻辑角色；active 承载本轮 send，standby 待命 | `primary`（arch-a 用语，最终统一为 `active`） |
| `seq` | `Session.send_seq` 分配的 **u32** 单调 seq；与 `Frame.seq`（u16）不同 | 不混用 |
| `frame_seq` | 物理 Frame 头里的 u16 wrap seq（既有），不改 | —— |

> **兼容性命脉**：`Frame.seq: u16` 不改；dual-transport 在应用层（session 层）上引入 u32 seq，
> 通过 `AckFrame` 独立走线。旧客户端不懂 u32 seq，经 `session_id == None` 退化为单 link 路径，
> 继续用老的 u16 `Frame.seq` + 隐式 ack_on_write。

---

## 1. `protocol::ack` —— AckFrame + UnackedRing（W1-A）

**位置：** `crates/mlink-core/src/protocol/ack.rs`（新增，≤160 行）

```rust
use crate::protocol::errors::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckFrame {
    /// 累计 ack —— "ack 之前（含）的 seq 全部收到"。
    pub ack: u32,
    /// 相对 `ack+1..ack+128` 的 128 位 SACK bitmap，bit0 = ack+1。
    pub bitmap: u128,
}

/// 定长 20 字节（4 ack + 16 bitmap），直接进 Frame.payload。
pub fn encode_ack(a: &AckFrame) -> Vec<u8>;
pub fn decode_ack(b: &[u8]) -> Result<AckFrame>;

/// 容量 256 的 ring buffer，键是 u32 seq，值是外部定义的 PendingFrame。
pub struct UnackedRing<T: Clone + Send + Sync + 'static> { /* 私有 */ }

impl<T: Clone + Send + Sync + 'static> UnackedRing<T> {
    pub fn new() -> Self;
    /// 满时丢弃最旧项并返回 Some，调用方打 warn 日志。
    pub fn push(&mut self, seq: u32, item: T) -> Option<(u32, T)>;
    /// 累计确认：清除所有 seq <= ack 的项。
    pub fn ack_cum(&mut self, ack: u32);
    /// SACK：按 bitmap 清除 base+1..base+128 范围内位为 1 的项。
    pub fn sack(&mut self, base: u32, bitmap: u128);
    /// 返回 `since`（含）之后所有未确认项的克隆，不改环。
    pub fn unacked_since(&self, since: u32) -> Vec<(u32, T)>;
    pub fn len(&self) -> usize;
}
```

**不变量：** 环容量恒 256；`push` 丢最旧时必须日志 warn。

---

## 2. `protocol::seq` —— SeqGen + DedupWindow（W1-B）

**位置：** `crates/mlink-core/src/protocol/seq.rs`（≤140 行）

```rust
pub struct SeqGen { next: u32 }

impl SeqGen {
    pub fn new() -> Self;
    /// u32 wrap，永不复用短周期。
    pub fn next(&mut self) -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Accept,     // 是新 seq，接收
    Duplicate,  // 窗口内重复
    Stale,      // 窗口外（seq < last - 127），丢弃不 ack
}

pub struct DedupWindow {
    last: u32,
    bits: u128,
}

impl DedupWindow {
    pub fn new() -> Self;
    pub fn observe(&mut self, seq: u32) -> Observation;
    /// 供编 AckFrame 使用：snapshot 出 (last, bitmap 映射到 last..last+128)。
    pub fn snapshot(&self) -> (u32, u128);
}
```

**wrap 语义：** `observe` 用 `seq.wrapping_sub(self.last)` 判断方向，差值 < 2^31 视为"后续"。

---

## 3. `core::link` —— 物理连接包装（W1-C）

**位置：** `crates/mlink-core/src/core/link.rs`（≤180 行）

```rust
use std::sync::{Arc, Mutex};
use crate::transport::{Connection, TransportCapabilities};
use crate::protocol::errors::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind { Ble, Tcp, Ipc, Mock }

impl TransportKind {
    /// WS 协议字符串映射（"ble" / "tcp" / "ipc" / "mock"）。
    pub fn as_wire(self) -> &'static str;
    pub fn from_wire(s: &str) -> Option<Self>;
}

#[derive(Debug, Clone, Copy)]
pub struct LinkHealth {
    pub rtt_ema_us: u32,      // 指数移动平均 RTT
    pub err_rate_milli: u16,  // 0..=1000 (千分比)
    pub last_ok_at_ms: u64,   // 单调时钟毫秒
    pub inflight: u16,        // 当前在途帧数
}

pub struct Link {
    id: String,
    kind: TransportKind,
    caps: TransportCapabilities,
    conn: Arc<dyn Connection>,
    health: Mutex<LinkHealth>,
}

impl Link {
    pub fn new(
        id: String,
        conn: Arc<dyn Connection>,
        kind: TransportKind,
        caps: TransportCapabilities,
    ) -> Self;

    pub fn id(&self) -> &str;
    pub fn kind(&self) -> TransportKind;
    pub fn caps(&self) -> &TransportCapabilities;
    /// 拷贝快照，短持锁。不得跨 await 持有返回值。
    pub fn health(&self) -> LinkHealth;

    pub async fn send(&self, bytes: &[u8]) -> Result<()>;
    pub async fn recv(&self) -> Result<Vec<u8>>;
    pub async fn close(&self);
}
```

**约束：**
- `send/recv` 必须 `&self`（依赖既有 `Connection::&self` 契约）。
- `health` 用 `std::sync::Mutex`，**禁止跨 await 持锁**（参考 `core/connection.rs` 注释）。
- `Link` 不自 spawn 任何 tokio task。

---

## 4. `core::session::types` —— Session 数据结构（W1-D）

**位置：** `crates/mlink-core/src/core/session/types.rs`（≤170 行）

```rust
use crate::core::link::{Link, LinkHealth, TransportKind};
use crate::protocol::ack::UnackedRing;
use crate::protocol::seq::{SeqGen, DedupWindow};
use crate::protocol::types::MessageType;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRole { Active, Standby }

#[derive(Debug, Clone)]
pub struct LinkStatus {
    pub link_id: String,
    pub kind: TransportKind,
    pub role: LinkRole,
    pub health: LinkHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchCause {
    ActiveFailed,       // send/recv/EOF 错
    BetterCandidate,    // 带宽显著变优（3 连）
    ManualUserRequest,  // WS transport_switch
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    LinkAdded   { peer_id: String, link_id: String, kind: TransportKind },
    LinkRemoved { peer_id: String, link_id: String, reason: String },
    Switched    { peer_id: String, from: String, to: String, cause: SwitchCause },
    StreamProgress { peer_id: String, stream_id: u16, pct: u8 },
}

#[derive(Debug, Clone)]
pub struct PendingFrame {
    pub bytes: Vec<u8>,          // 已编码完整 frame
    pub msg_type: MessageType,
}

pub struct Session {
    pub peer_id: String,
    pub session_id: [u8; 16],
    pub(crate) links: RwLock<Vec<Arc<Link>>>,
    pub(crate) active: RwLock<Option<String>>,
    pub(crate) send_seq: Mutex<SeqGen>,
    pub(crate) dedup: Mutex<DedupWindow>,
    pub(crate) unacked: Mutex<UnackedRing<PendingFrame>>,
    pub(crate) events: broadcast::Sender<SessionEvent>,
    /// Scheduler 内联状态（hysteresis 计数器等），不独立文件。
    pub(crate) sched_state: Mutex<SchedulerState>,
}

/// scheduler 内联状态（合入 session/io.rs，但类型声明在此）。
#[derive(Debug, Default)]
pub struct SchedulerState {
    pub better_streak: std::collections::HashMap<String, u8>,
    pub tick: u32,
}

/// 上限：单 peer 最多挂 4 条 link（arch-a 硬上限）。
pub const MAX_LINKS_PER_PEER: usize = 4;

/// switchback hysteresis：连续 N 拍更优才回切，默认 3（arch-a 防抖）。
pub const SWITCHBACK_HYSTERESIS: u8 = 3;

/// Ack 合并门限：50ms 或累计 8 条未 ack 择一触发。
pub const ACK_MAX_DELAY_MS: u64 = 50;
pub const ACK_MAX_PENDING: usize = 8;

/// ExplicitSelector 覆盖过期（arch-a 采纳）。
pub const EXPLICIT_OVERRIDE_TTL_SECS: u64 = 600;
```

**不变量：**
- `MAX_LINKS_PER_PEER = 4`（硬上限，防脑洞；BLE+TCP 目前只会到 2）。
- `session_id` 首条 link 由 `attach_link` 生成，必须是 CSPRNG 随机，不泄露 app_uuid。
- `links` 使用 `RwLock<Vec<Arc<Link>>>` 而非 HashMap；单 peer 最多 4 条，Vec 扫描可忽略。
- **禁止 await 期间持 `active` / `links` 的锁**（释放后再 send）。

---

## 5. `protocol::types::Handshake` 扩展（W1-E）

**位置：** `crates/mlink-core/src/protocol/types.rs`（+40 行）

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub app_uuid: String,
    pub version: u8,
    pub mtu: u16,
    pub compress: bool,
    pub encrypt: bool,
    pub last_seq: u16,                             // 兼容旧 u16 seq（不改）
    pub resume_streams: Vec<StreamResumeInfo>,
    #[serde(default)]
    pub room_hash: Option<[u8; 8]>,
    /// 第二条 link 握手时携带首条 link 生成的 session_id → 声明要合并到同一 Session。
    /// 首次握手 None；对端在应答 Handshake 里回填自己分配的 session_id。
    #[serde(default)]
    pub session_id: Option<[u8; 16]>,
    /// session 级 u32 last seq（dual-link resume 用）；旧 peer 不发。
    #[serde(default)]
    pub session_last_seq: u32,
}
```

**兼容性：** 旧客户端不发这两个字段 → serde default → 走单 link Session 路径（`AttachOutcome::CreatedNew` 后不再接受第二条同 `peer_id` 的 link）。

---

## 6. `protocol::errors::MlinkError` 扩展（W1-F）

**位置：** `crates/mlink-core/src/protocol/errors.rs`（+20 行）

```rust
pub enum MlinkError {
    // ...既有变体保持...
    NoHealthyLink { peer_id: String },
}

// wire 映射，新增码位 0x08
pub enum ErrorCode {
    // ...
    NoHealthyLink = 0x08,
}
```

`Display` 格式：`"no healthy link for peer {peer_id}"`（前端匹配靠这个前缀）。

---

## 7. `core::session::io` —— Session 方法（W2-2，含 scheduler 内联）

**位置：** `crates/mlink-core/src/core/session/io.rs`（≤200 行）

```rust
use crate::core::session::types::{Session, LinkStatus, SwitchCause};
use crate::protocol::ack::AckFrame;
use crate::protocol::errors::Result;
use crate::protocol::types::MessageType;

impl Session {
    /// 上层唯一发送入口。流程：
    /// 1) 分配 u32 seq；
    /// 2) 构 Frame（MTU 按 active link）→ 编码；
    /// 3) 选 active link（若 None，调内联 `pick_best`）→ `link.send`；
    /// 4) 成功 → UnackedRing.push；失败 → 内联 `on_active_failure`
    ///    → promote 候选 → retry 一次；仍失败返回 Err(NoHealthyLink)。
    pub async fn send(
        &self,
        msg_type: MessageType,
        payload: Vec<u8>,
        encrypt_key: Option<&[u8]>,
        compress: bool,
    ) -> Result<u32>;

    /// 收到对端 AckFrame：UnackedRing.ack_cum + sack。
    pub(crate) async fn on_ack(&self, ack: &AckFrame);

    /// 将 `unacked_since(ack+1)` 全部通过 `link_id` 重发；用于 failover。
    pub(crate) async fn flush_unacked_over(&self, link_id: &str) -> Result<()>;

    /// 广播订阅。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<
        crate::core::session::types::SessionEvent
    >;

    /// active/standby 快照。
    pub async fn status(&self) -> Vec<LinkStatus>;
}

// ---- 内联 scheduler（不独立模块） ----

impl Session {
    /// 选分：caps.throughput_bps * (1 - err_rate_milli/1000) - rtt_ema_us/10。
    pub(crate) fn score(status: &LinkStatus) -> i64;

    /// 从 links 里选最高分，返回 link_id。
    pub(crate) async fn pick_best(&self) -> Option<String>;

    /// 活跃故障立刻切下一个（绕过 hysteresis）。返回新 active + 原因。
    pub(crate) async fn promote_on_failure(
        &self,
        failing_id: &str,
    ) -> Option<(String, SwitchCause)>;

    /// 心跳 tick 时判断：best 连续 N 拍更优才允许回切。返回 true 时调用方执行切换。
    pub(crate) async fn should_switchback(&self, current: &str, best: &str) -> bool;
}
```

**SLO 目标（session/io 内部保证）：**
- active 故障 → standby 接管 → 首条新 frame 发出，P99 ≤ 5ms（纯内存 + Arc swap）。
- 单 link 退化路径吞吐 ≥ 旧单 link 路径的 95%。
- idle session (1 peer, 2 link) 内存 ≤ 64KB。

**禁忌：**
- `send` 内部不得跨 await 持 `active` / `links` / `unacked` 三锁；先 clone Arc、释放锁、再 await。
- `score` 纯函数，测试可单独覆盖。

---

## 8. `core::session::reader` —— 收取路径（W2-3）

**位置：** `crates/mlink-core/src/core/session/reader.rs`（≤180 行）

```rust
use crate::core::node::NodeEvent;

impl Session {
    /// 对每条 link 起后台 reader，把解出的 (Frame, payload) 合并到内部 mpsc。
    /// 中央任务：dedup（Observe） → 若 Accept 则向 `out` 广播 `NodeEvent::MessageReceived`
    /// 并累加 ack pending；50ms / 8 条凑一次调 `send_ack`。
    pub fn spawn_readers(
        self: &std::sync::Arc<Self>,
        out: tokio::sync::broadcast::Sender<NodeEvent>,
    );

    /// link EOF 触发：detach + emit LinkRemoved + 调内联 promote_on_failure。
    pub(crate) async fn on_link_eof(&self, link_id: &str);
}
```

**Ack 合并策略（不变量）：**
- 定时器 50ms 或累计 8 条 Accept 中任一条件达成即编 AckFrame 经 active link 送回。
- AckFrame 经 `MessageType::Ack`（既有 0x03）Frame 包装，payload = `encode_ack(&frame)`。
- 收到 MessageType::Ack → 解码后调 `Session::on_ack`，**不**作为 NodeEvent 发出。

---

## 9. `core::session::manager` —— SessionManager（W2-4）

**位置：** `crates/mlink-core/src/core/session/manager.rs`（≤160 行）

```rust
use std::sync::Arc;
use crate::core::link::Link;
use crate::core::session::types::{LinkStatus, Session};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    /// 首条 link：新建 Session，返回生成的 session_id（caller 在 handshake 回填此值）。
    CreatedNew { session_id: [u8; 16] },
    /// session_id_hint 匹配 → 挂成第二条 link。
    AttachedExisting,
    /// 同 TransportKind 已存在一条 link（如两条 BLE）—— 拒收，caller 断新连。
    RejectedDuplicateKind,
    /// 超过 MAX_LINKS_PER_PEER（=4）—— 拒收。
    RejectedCapacity,
}

pub struct SessionManager { /* tokio::Mutex<HashMap<String, Arc<Session>>> */ }

impl SessionManager {
    pub fn new() -> Self;

    /// 合并点：
    /// - peer 不存在 → 新建 Session 并记录 session_id；
    /// - peer 存在 + session_id_hint 匹配 + kind 不重复 + 未满 → AttachedExisting；
    /// - kind 冲突 → RejectedDuplicateKind；
    /// - 容量满 → RejectedCapacity。
    pub async fn attach_link(
        &self,
        peer_id: &str,
        link: Link,
        session_id_hint: Option<[u8; 16]>,
    ) -> AttachOutcome;

    pub async fn get(&self, peer_id: &str) -> Option<Arc<Session>>;
    pub async fn contains(&self, peer_id: &str) -> bool;
    /// 断整个 peer：关所有 link + 从 map 移除 + 返回已摘 Session（caller 可收尾）。
    pub async fn drop(&self, peer_id: &str) -> Option<Arc<Session>>;
    /// 断单条 link；若摘完后 session 为空自动 drop 整个 peer。返回 true=peer_gone。
    pub async fn detach_link(&self, peer_id: &str, link_id: &str) -> bool;

    pub async fn list_status(&self) -> Vec<(String, Vec<LinkStatus>)>;
    pub async fn count(&self) -> usize;
}
```

---

## 10. `core::node` 变动（W2-5..W2-10）

**公共 API 不变** —— 老测试零改动。

```rust
pub struct Node {
    // 旧: connections: Arc<Mutex<ConnectionManager>>
    // 新: 两者并存（connections 作为兼容壳，内部转发 SessionManager）
    sessions: Arc<SessionManager>,
    connections: Arc<Mutex<ConnectionManager>>, // 保留，shared/add/remove 转发给 sessions
    // 其余字段不变
    // 新增 ExplicitSelector 热覆盖表：
    explicit_override: Arc<RwLock<HashMap<String, (TransportKind, Instant)>>>,
    // (peer_id -> (forced_kind, expire_at))
}

// 签名 100% 保持原样（冻结）：
impl Node {
    pub async fn connect_peer(&self, transport: &mut dyn Transport, discovered: &DiscoveredPeer) -> Result<String>;
    pub async fn accept_incoming(&self, conn: Box<dyn Connection>, transport_id: &str, fallback_name: String) -> Result<String>;
    pub async fn send_raw(&self, peer_id: &str, msg_type: MessageType, payload: &[u8]) -> Result<()>;
    pub async fn recv_raw(&self, peer_id: &str) -> Result<(Frame, Vec<u8>)>;
    pub async fn disconnect_peer(&self, peer_id: &str) -> Result<()>;
    pub fn spawn_peer_reader(&self, peer_id: String) -> tokio::task::JoinHandle<()>;
    pub async fn send_heartbeat(&self, peer_id: &str) -> Result<()>;
    pub async fn check_heartbeat(&self, peer_id: &str) -> Result<bool>;
}

// Wave 2 新增（W2-8）：
impl Node {
    /// 主动挂第二条 link（手动 / dual_probe 成功后）。
    pub async fn attach_secondary_link(
        &self,
        peer_id: &str,
        conn: Box<dyn Connection>,
        kind: TransportKind,
    ) -> Result<()>;

    /// 查某 peer 下所有 link 状态（含 active 标记）。
    pub async fn peer_link_status(&self, peer_id: &str) -> Vec<LinkStatus>;

    /// 手动切 active：写入 explicit_override（10 分钟 TTL）并立刻触发 promote。
    pub async fn switch_active_link(&self, peer_id: &str, to_kind: TransportKind) -> Result<()>;

    pub fn subscribe_session_events(&self) -> broadcast::Receiver<SessionEvent>;

    /// 断单条 link（W2-7 扩展；保留用于前端 disconnect_link 调试）。
    pub async fn disconnect_link(&self, peer_id: &str, link_id: &str) -> Result<()>;
}
```

### NodeEvent 扩展（W2-9）

```rust
pub enum NodeEvent {
    // 旧变体保留
    PeerDiscovered   { peer_id: String },
    PeerConnected    { peer_id: String },
    PeerDisconnected { peer_id: String },
    PeerLost         { peer_id: String },
    Reconnecting     { peer_id: String, attempt: u32 },
    MessageReceived  { peer_id: String, payload: Vec<u8> },

    // 新增（dual-transport）
    LinkAdded   { peer_id: String, link_id: String, kind: TransportKind },
    LinkRemoved { peer_id: String, link_id: String, reason: String },
    Switched    { peer_id: String, from: String, to: String, cause: SwitchCause },
}
```

**语义约定（不变量）：**
- 双 link 时断单条 → 只发 `LinkRemoved`（+若是 active 再发 `Switched`）。
- 断最后一条 → 发 `LinkRemoved` + `PeerDisconnected`。
- 首条建连 → 发 `LinkAdded` + `PeerConnected`；第二条建连仅 `LinkAdded`（无第二次 `PeerConnected`）。

### ConnectionManager 兼容壳（W2-10）

```rust
// 签名保持 —— 老测试直接过：
impl ConnectionManager {
    pub fn add(&mut self, id: String, conn: SharedConnection);
    pub fn remove(&mut self, id: &str) -> Option<SharedConnection>;
    pub fn shared(&self, id: &str) -> Option<SharedConnection>;
    pub fn contains(&self, id: &str) -> bool;
    pub fn list_ids(&self) -> Vec<String>;
    pub fn count(&self) -> usize;
}
```

**实现改动：**
- 内部仍持 `HashMap<String, SharedConnection>`，但 `add` 同时透传给 `SessionManager.attach_link`（构造单 link Session）。
- `shared(id)` 返回 active link 的 `Arc<dyn Connection>`（通过 `SessionManager` 查）。
- 保留这层是为了让 `manager_add_and_count / manager_remove_returns_conn / manager_shared_returns_clone` 等既有测试逐字不改。
- Wave 3 结束后打 `#[deprecated(note = "use SessionManager")]`，保留至下一 phase 清理。

---

## 11. `transport::dual_probe` —— 第二通道主动拨号（W2-11）

**位置：** `crates/mlink-core/src/transport/dual_probe.rs`（≤130 行）

```rust
use crate::core::link::{Link, TransportKind};
use crate::transport::{DiscoveredPeer, Transport};
use crate::protocol::errors::Result;

pub struct TransportSet {
    pub ble: Option<Box<dyn Transport>>,
    pub tcp: Option<Box<dyn Transport>>,
    pub ipc: Option<Box<dyn Transport>>,
}

/// 给定首条连接的 transport kind + 发现到的 peer，尝试拨第二条：
/// - primary=Ble → 试 Tcp（本地有 listener 且 peer 广播里含 TCP 端口时）；
/// - primary=Tcp → 试 Ble（本地有 BLE 且 peer 在扫描到）；
/// - 其余返回 Err(NoSecondary)。
pub async fn try_probe_secondary(
    primary_kind: TransportKind,
    peer: &DiscoveredPeer,
    transports: &mut TransportSet,
) -> Result<Link>;
```

---

## 12. Daemon WS 协议扩展（W2-13）

**位置：** `crates/mlink-daemon/src/protocol.rs`（+80 行）

### 新增下行事件

| type | 何时发 | payload |
| --- | --- | --- |
| `link_added` | `SessionEvent::LinkAdded` | `{ peer_id, link_id, transport_id }` |
| `link_removed` | `SessionEvent::LinkRemoved` | `{ peer_id, link_id, reason }` |
| `primary_changed` | `SessionEvent::Switched` | `{ peer_id, old: link_id, new: link_id, cause: "active_failed" | "better_candidate" | "manual" }` |
| `transport_state` | `transport_list` 响应 + 变化推送 | `{ peer_id, active_link_id, links: [{link_id, transport_id, role, rtt_ema_us, err_rate_milli}] }` |

### 新增上行命令

| type | 用途 | payload |
| --- | --- | --- |
| `transport_list` | 拉当前所有 peer/link 快照 | `{}` |
| `transport_switch` | 手动切 active（写入 ExplicitSelector 10 分钟） | `{ peer_id, to_kind: "ble"\|"tcp"\|"ipc" }` |
| `disconnect_link` | 故障注入，仅 `MLINK_DAEMON_DEV=1` 开放 | `{ peer_id, link_id }` |

### daemon handler 签名（W2-14）

```rust
// crates/mlink-daemon/src/session/transport_debug.rs（≤170 行）
pub(super) async fn handle_transport_list(
    socket: &mut axum::extract::ws::WebSocket,
    state: &crate::DaemonState,
    id: Option<&str>,
) -> bool;

pub(super) async fn handle_transport_switch(
    socket: &mut axum::extract::ws::WebSocket,
    state: &crate::DaemonState,
    id: Option<&str>,
    payload: serde_json::Value,
) -> bool;

pub(super) async fn handle_disconnect_link(
    socket: &mut axum::extract::ws::WebSocket,
    state: &crate::DaemonState,
    id: Option<&str>,
    payload: serde_json::Value,
) -> bool;

/// 后台：订阅 Node session events，转发到所有 WS 会话。
pub fn spawn_session_event_forwarder(state: crate::DaemonState);
```

**安全约束：**
- `transport_switch` 的 `to_kind` 白名单校验；非白名单返回 `bad_type`。
- `disconnect_link` 非 dev 模式返回 `bad_type`。
- 旧客户端不订阅新 type → 广播 JSON 无识别字段直接忽略（现有 `forward_*` 路径行为保留）。

---

## 13. daemon 其它（W2-12 / W2-15 / W2-16）

### W2-12 discovery 拆分

```rust
// crates/mlink-daemon/src/discovery/mod.rs
pub enum DaemonTransport { Tcp, Ble, Ipc, Dual }
pub fn spawn(node: Arc<Node>, mode: DaemonTransport);

// crates/mlink-daemon/src/discovery/tcp.rs
pub fn spawn_tcp(node: Arc<Node>);

// crates/mlink-daemon/src/discovery/ble.rs
pub fn spawn_ble(node: Arc<Node>);
```

`DaemonTransport::Dual` 时并行调 `spawn_tcp` + `spawn_ble`，共享同一 `Arc<Node>`；合并靠 `SessionManager.attach_link` 自然命中。

### W2-15 dispatch 扩展

挂三个新 handler（`transport_list` / `transport_switch` / `disconnect_link`）到既有 `dispatch.rs`。

### W2-16 state 扩展

`build_state` 启动时调 `transport_debug::spawn_session_event_forwarder(state)`；无需在 DaemonState 结构内加 selector 字段（选 active 由 Node 持有 ExplicitSelector 表完成）。

---

## 14. Web Debug UI 契约（W3-13）

**位置：** `examples/web-debug/index.html`（+180 行）

- 右侧新增 Transport 面板：每 peer 一行，展开后显示 N 条 link（transport_id / role 标签 / RTT / err%）。
- active link 用背景色高亮。
- 两按钮 `Force BLE` / `Force TCP` → 调 `transport_switch`；下方小字显示覆盖剩余时间（10 分钟倒计时）。
- 右下角"稳定性测试"按钮：发 1000 条 ping，统计切换次数 / 丢包 / 重复。
- 顶部 `primary_changed` 事件流固定 10 行，按 cause 色码（红=ActiveFailed / 蓝=BetterCandidate / 灰=Manual）。
- 收到 `bad_type` → 静默降级（daemon 不支持时隐藏 Transport 面板）。

---

## 15. 非契约但必须遵守的不变量

1. **发送顺序**：同一 Session 内，`send_seq` 单调递增；failover 时先把 `unacked_since(ack+1)` 经新 link 全发完，再开放新 seq 分配。
2. **Ack 延迟**：50ms 或累计 8 条未 ack 任一条件达成即发 AckFrame。
3. **切换 SLO**：active 故障 → standby 接管 → 首条新 frame 发出，P99 ≤ 5ms（纯内存）。
4. **DedupWindow 128**：窗外 seq 一律丢（不重传、不透传），防长分区回放。
5. **MTU 边界**：每条 link 独立 MTU；Session 发送时按 **active link** 的 MTU 切分；failover 若新 link MTU 更小，已 unacked 的大 frame 原样重发（transport 层 TCP 不 care，BLE 层自行分片）。
6. **内存上限**：UnackedRing 256 × frame ≈ 16KB/peer，两条 link idle 合计 ≤ 64KB。
7. **线程安全口径**：一律遵循 `先 clone Arc → drop 锁 → await` 模式；违反 = PR 打回（见 memory id=project_connection_ownership_pattern）。
8. **日志字段**：所有 link 相关日志必须带 `peer_id`、`link_id`、`kind` 三字段（便于 web-debug 关联）。

---

## CHANGELOG

- 2026-04-25 v1（final）—— 合成 arch-a + arch-b，team-lead 定稿冻结。
  - 主体：arch-b Session 分治 + UnackedRing + 128 位 SACK + SessionManager + Node 兼容壳
  - 简化：scheduler 内联进 session/io（不独立模块）；heartbeat 不独立文件（沿用 Node 既有心跳机制 + Session 内部 ack 合并 timer）
  - arch-a 采纳：MAX_LINKS_PER_PEER=4、hysteresis=3、ExplicitSelector 10 分钟 TTL
  - 去除：Wi-Fi Direct / Thread / IPC 的 transport kind 扩展预留仅保留 enum 位，不做 discovery 端适配
