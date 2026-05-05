# 接口契约（冻结版，arch-b）

所有签名以本文为准，实现阶段不得擅自变动；
需要改动必须回到本文档修订并重广播。

## 1. protocol/ack.rs

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckFrame {
    /// 已连续收到的最高 seq（cumulative）。
    pub ack: u32,
    /// 相对 `ack+1..ack+128` 的 128 位 SACK bitmap，bit0 = ack+1。
    pub bitmap: u128,
}

pub fn encode_ack(a: &AckFrame) -> Vec<u8>;             // 20 字节定长
pub fn decode_ack(b: &[u8]) -> crate::protocol::errors::Result<AckFrame>;

pub struct UnackedRing<T: Clone + Send + Sync + 'static> { /* capacity 256 */ }
impl<T: Clone + Send + Sync + 'static> UnackedRing<T> {
    pub fn new() -> Self;
    /// 越界时丢弃最旧项（返回 Some 上抛）。
    pub fn push(&mut self, seq: u32, item: T) -> Option<(u32, T)>;
    pub fn ack_cum(&mut self, ack: u32);
    pub fn sack(&mut self, base: u32, bitmap: u128);
    /// 克隆 `since` 之后（含）的全部未确认项；不修改环。
    pub fn unacked_since(&self, since: u32) -> Vec<(u32, T)>;
    pub fn len(&self) -> usize;
}
```

## 2. protocol/seq.rs

```rust
pub struct SeqGen { next: u32 }
impl SeqGen {
    pub fn new() -> Self;
    pub fn next(&mut self) -> u32;   // wrap 2^32，永不复用短周期
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation { Accept, Duplicate, Stale }

pub struct DedupWindow { last: u32, bits: u128 }
impl DedupWindow {
    pub fn new() -> Self;
    /// 单调推进 `last`；bits 保留最近 128 个 seq 的已见标记；
    /// seq < last - 127 返回 Stale（客户端应丢弃 & 不回 ack 进 bitmap）。
    pub fn observe(&mut self, seq: u32) -> Observation;
    pub fn snapshot(&self) -> (u32, u128); // 供编 AckFrame 使用
}
```

## 3. core/link.rs

```rust
use crate::transport::{Connection, TransportCapabilities};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind { Ble, Tcp, Ipc, Mock }

#[derive(Debug, Clone, Copy)]
pub struct LinkHealth {
    pub rtt_ema_us: u32,
    pub err_rate_milli: u16,   // 0..=1000
    pub last_ok_at_ms: u64,    // 单调时钟毫秒
    pub inflight: u16,
}

pub struct Link { /* 内部: conn, kind, caps, health(Mutex), id */ }
impl Link {
    pub fn new(
        id: String,
        conn: std::sync::Arc<dyn Connection>,
        kind: TransportKind,
        caps: TransportCapabilities,
    ) -> Self;

    pub fn id(&self) -> &str;
    pub fn kind(&self) -> TransportKind;
    pub fn caps(&self) -> &TransportCapabilities;
    pub fn health(&self) -> LinkHealth;

    pub async fn send(&self, bytes: &[u8]) -> crate::protocol::errors::Result<()>;
    pub async fn recv(&self) -> crate::protocol::errors::Result<Vec<u8>>;
    pub async fn close(&self);
}
```

约束：`Link::send/recv` 必须 `&self`，内部 health 用 `std::sync::Mutex<LinkHealth>` 托管，
禁止跨 await 持锁（参考 core/connection.rs 注释约定）。

## 4. core/session/types.rs

```rust
use crate::core::link::{Link, LinkHealth, TransportKind};
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

#[derive(Debug, Clone)]
pub enum SessionEvent {
    LinkAdded    { peer_id: String, link_id: String, kind: TransportKind },
    LinkRemoved  { peer_id: String, link_id: String, reason: String },
    Switched     { peer_id: String, from: String, to: String, cause: SwitchCause },
    StreamProgress { peer_id: String, stream_id: u16, pct: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchCause {
    ActiveFailed,          // send/recv 错
    BetterCandidate,       // 带宽显著变优
    ManualUserRequest,     // WS transport_switch
}

pub struct Session {
    pub peer_id: String,
    pub session_id: [u8; 16],
    pub(crate) links: RwLock<Vec<Arc<Link>>>,
    pub(crate) active: RwLock<Option<String>>,       // active link id
    pub(crate) send_seq: Mutex<crate::protocol::seq::SeqGen>,
    pub(crate) dedup:    Mutex<crate::protocol::seq::DedupWindow>,
    pub(crate) unacked:  Mutex<crate::protocol::ack::UnackedRing<PendingFrame>>,
    pub(crate) events:   broadcast::Sender<SessionEvent>,
}

#[derive(Debug, Clone)]
pub struct PendingFrame {
    pub bytes: Vec<u8>,          // 已编码完整 frame
    pub msg_type: crate::protocol::types::MessageType,
}
```

## 5. core/session/scheduler.rs

```rust
use crate::core::session::types::{LinkRole, LinkStatus, SwitchCause};

pub struct Scheduler {
    /// 连续 N 拍更优才 switchback，默认 3。
    pub switchback_hysteresis: u8,
    tick: u32,
    better_streak: std::collections::HashMap<String, u8>,
}

impl Scheduler {
    pub fn new() -> Self;
    /// 按 caps.throughput_bps * (1 - err_rate) - rtt_penalty 打分。
    pub fn select_best(&self, links: &[LinkStatus]) -> Option<String>;
    /// 活跃 link 故障时立刻返回下一个候选（绕过 hysteresis）。
    pub fn promote_on_failure(&mut self, failing: &str, links: &[LinkStatus])
        -> Option<(String, SwitchCause)>;
    /// 每个心跳 tick 调用；达标时允许回切（返回 ManualUserRequest 由调用方覆盖）。
    pub fn should_switchback(&mut self, current: &str, best: &str) -> bool;
}
```

## 6. core/session/io.rs（Session 方法）

```rust
impl Session {
    /// 上层 send_raw 的唯一入口：分配 seq → 构 Frame → 选 active → 写；
    /// 写失败立刻触发 scheduler.promote_on_failure 并重试一次；仍失败返回错。
    pub async fn send(
        &self,
        msg_type: crate::protocol::types::MessageType,
        payload: Vec<u8>,
        encrypt_key: Option<&[u8]>,
        compress: bool,
    ) -> crate::protocol::errors::Result<u32>;

    pub(crate) async fn on_ack(&self, ack: &crate::protocol::ack::AckFrame);

    /// 把 `unacked_since(ack+1)` 全部通过 `link_id` 重发。
    pub(crate) async fn flush_unacked_over(
        &self,
        link_id: &str,
    ) -> crate::protocol::errors::Result<()>;

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SessionEvent>;
    pub async fn status(&self) -> Vec<LinkStatus>;
}
```

## 7. core/session/reader.rs（Session 方法）

```rust
impl Session {
    /// 为每条 link 起后台 reader：把解出的 (frame,payload) 合并到内部
    /// mpsc，一个中央任务做 dedup + ack 回发 + 向 out 广播 NodeEvent。
    pub fn spawn_readers(
        self: &std::sync::Arc<Self>,
        out: tokio::sync::broadcast::Sender<crate::core::node::NodeEvent>,
    );

    pub(crate) async fn on_link_eof(&self, link_id: &str);
}
```

## 8. core/session/manager.rs

```rust
use std::sync::Arc;
use crate::core::link::Link;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    CreatedNew { session_id: [u8; 16] },
    AttachedExisting,
    /// 同 kind 已存在一条 link，按策略拒收第二条（例如两条 BLE）。
    RejectedDuplicateKind,
}

pub struct SessionManager { /* tokio::Mutex<HashMap<String, Arc<Session>>> */ }

impl SessionManager {
    pub fn new() -> Self;

    /// 若 peer 不存在且无 session_id_hint：新建 Session 并返回 CreatedNew；
    /// 若 peer 存在且 session_id_hint 匹配：挂成第二条 link，返回 AttachedExisting；
    /// 若相同 TransportKind 已在 session 里：RejectedDuplicateKind。
    pub async fn attach_link(
        &self,
        peer_id: &str,
        link: Link,
        session_id_hint: Option<[u8; 16]>,
    ) -> AttachOutcome;

    pub async fn get(&self, peer_id: &str) -> Option<Arc<Session>>;
    pub async fn contains(&self, peer_id: &str) -> bool;
    pub async fn drop(&self, peer_id: &str) -> Option<Arc<Session>>;
    pub async fn list_status(&self) -> Vec<(String, Vec<LinkStatus>)>;
    pub async fn count(&self) -> usize;
}
```

## 9. protocol/types.rs 改动

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub app_uuid: String,
    pub version: u8,
    pub mtu: u16,
    pub compress: bool,
    pub encrypt: bool,
    pub last_seq: u16,          // 兼容旧字段；新代码读 session_last_seq
    pub resume_streams: Vec<StreamResumeInfo>,
    #[serde(default)]
    pub room_hash: Option<[u8; 8]>,
    /// 第二条 link 携带首条 link 返回的 session_id 声明要合并到同一 Session。
    /// 首次握手填 None；对端新建 Session 后回握应填入自己生成的 id。
    #[serde(default)]
    pub session_id: Option<[u8; 16]>,
    #[serde(default)]
    pub session_last_seq: u32,
}
```

## 10. transport/dual_probe.rs

```rust
use crate::core::link::{Link, TransportKind};
use crate::transport::{DiscoveredPeer, Transport};

pub struct TransportSet {
    pub ble: Option<Box<dyn Transport>>,
    pub tcp: Option<Box<dyn Transport>>,
    pub ipc: Option<Box<dyn Transport>>,
}

pub async fn try_probe_secondary(
    primary_kind: TransportKind,
    peer: &DiscoveredPeer,
    transports: &mut TransportSet,
) -> crate::protocol::errors::Result<Link>;
```

## 11. Node 层变动（Node 公共 API 保持不变）

```rust
pub struct Node {
    // 旧：connections: Arc<Mutex<ConnectionManager>>
    // 新：
    sessions: Arc<crate::core::session::manager::SessionManager>,
    // 其余字段不变
}

impl Node {
    // 签名全部保持原样：
    pub async fn connect_peer(&self, transport: &mut dyn Transport, discovered: &DiscoveredPeer) -> Result<String>;
    pub async fn accept_incoming(&self, conn: Box<dyn Connection>, transport_id: &str, fallback_name: String) -> Result<String>;
    pub async fn send_raw(&self, peer_id: &str, msg_type: MessageType, payload: &[u8]) -> Result<()>;
    pub async fn disconnect_peer(&self, peer_id: &str) -> Result<()>;
    pub fn spawn_peer_reader(&self, peer_id: String) -> tokio::task::JoinHandle<()>;
    // 新增（Wave 2 专用于 debug / dual）：
    pub async fn attach_secondary_link(&self, peer_id: &str, conn: Box<dyn Connection>, kind: TransportKind) -> Result<()>;
    pub async fn peer_link_status(&self, peer_id: &str) -> Vec<LinkStatus>;
    pub async fn switch_active_link(&self, peer_id: &str, to_kind: TransportKind) -> Result<()>;
    pub fn subscribe_session_events(&self) -> broadcast::Receiver<SessionEvent>;
}
```

## 12. daemon/session/transport_debug.rs

```rust
pub(super) async fn handle_transport_list(
    socket: &mut axum::extract::ws::WebSocket,
    state: &crate::DaemonState,
    id: Option<&str>,
) -> bool;

pub(super) async fn handle_transport_switch(
    socket: &mut axum::extract::ws::WebSocket,
    state: &crate::DaemonState,
    id: Option<&str>,
    payload: serde_json::Value,  // { peer_id, to_kind: "ble"|"tcp"|"ipc" }
) -> bool;

/// 后台任务：订阅 Session events 并转发到所有 WS 会话（每个会话一份 broadcast）。
pub fn spawn_session_event_forwarder(state: crate::DaemonState);
```

WS 新增入站类型：`transport_list` / `transport_switch`
WS 新增出站事件：`transport_state { peer_id, active_link_id, links: [LinkStatus] }`

## 13. ConnectionManager 命运

保留为兼容壳：
- `ConnectionManager::add(peer_id, conn)` → 内部调用 `SessionManager.attach_link` 构造单 link Session。
- `shared(peer_id) -> Option<Arc<dyn Connection>>` 返回 Session active link 的 Connection（用于 tests）。
- 新代码不要依赖；Wave 2 结束后 `#[deprecated]`。

---

## 非契约但必须遵守的不变量

1. **Session 内发送顺序**：同一 msg_type 的 seq 严格递增，跨 link 也不回退。
   scheduler 做 failover 时先把 unacked 复制一份到新 link，再开放新 seq 分配。
2. **Ack 延迟**：最多 50ms 或累计 8 条未 ack 中任一条件达成就触发 ack 帧。
3. **切换延迟 SLO**：active 故障 → standby 接管 → 首条新 frame 发出，P99 ≤ 5ms
   （纯内存操作，不含网络）。
4. **Dedup 窗口 128**：窗口之外的 seq 必须丢弃（不重传、不透传），避免长分区回放。
5. **MTU 约束**：每条 link 独立 MTU；Session 发送时按 active link 的 MTU 切分。
6. **内存上限**：UnackedRing 256 × frame ≈ 16 KB / peer。超界丢最旧并日志。
