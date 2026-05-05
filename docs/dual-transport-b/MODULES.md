# 双通道模块拆解（arch-b：Session 分治方案）

## 设计主线
不在 `Transport/Connection` 层做融合，而是新增一层 **Session** 聚合多条物理 Link。
Node 层只持有 `SessionManager`，所有 send/recv/ack/seq 由 Session 处理；
Transport/Connection 继续只管"把字节搬过去"。

区别于 Bonding（Connection 包一层 Connection）：Session 分治方案让去重/重传/
调度集中到一处，物理通道只是 I/O pipe，单通道退化路径也等价于
"Session 只挂一条 Link"。

## Wave 1（独立可合，不互相阻塞）

### W1-A：protocol/ack.rs — Ack 帧与 Unacked 环
- 文件：`crates/mlink-core/src/protocol/ack.rs`
- 职责：`AckFrame { ack: u32, bitmap: u128 }`（cum ack + 128 位 SACK）；
  编解码走 `MessageType::Ack` 载荷；定义 `UnackedRing<T>`（ring buffer，容量 256）。
- 行数：≈160
- pub API：
  - `pub struct AckFrame { pub ack: u32, pub bitmap: u128 }`
  - `pub fn encode_ack(a: &AckFrame) -> Vec<u8>`
  - `pub fn decode_ack(b: &[u8]) -> Result<AckFrame>`
  - `pub struct UnackedRing<T: Clone> { /* … */ }` with `push(seq,T) / ack_cum(seq) / sack(bitmap) / unacked_since(seq) -> Vec<(u32,T)>`

### W1-B：protocol/seq.rs — SeqGenerator + DedupWindow
- 文件：`crates/mlink-core/src/protocol/seq.rs`
- 职责：u32 单调 seq 生成器；接收端 128 位滑窗去重（`last_seq` + `bitmap`），
  返回 `Accept | Duplicate | Stale`。
- 行数：≈140
- pub API：
  - `pub struct SeqGen(u32)` with `next() -> u32`
  - `pub struct DedupWindow { last: u32, bits: u128 }`
    - `fn observe(seq: u32) -> Observation`
    - `enum Observation { Accept, Duplicate, Stale }`

### W1-C：core/link.rs — Link（物理连接封装）
- 文件：`crates/mlink-core/src/core/link.rs`
- 职责：包装 `Arc<dyn Connection>`，暴露 `id / transport_kind / capabilities / send_frame / recv_frame / health_mark_ok / health_mark_err / health_snapshot`。
- 行数：≈180
- pub API：
  - `pub enum TransportKind { Ble, Tcp, Ipc, Mock }`
  - `pub struct Link { /* conn, kind, caps, health, created_at, last_ok, err_ema */ }`
    - `pub fn new(conn: Arc<dyn Connection>, kind: TransportKind, caps: TransportCapabilities) -> Self`
    - `pub async fn send(&self, bytes: &[u8]) -> Result<()>`
    - `pub async fn recv(&self) -> Result<Vec<u8>>`
    - `pub fn health(&self) -> LinkHealth` (RTT EMA + err rate + last_ok_at)
    - `pub fn caps(&self) -> &TransportCapabilities`
    - `pub fn kind(&self) -> TransportKind`
    - `pub fn id(&self) -> &str`（`kind+peer_id+started_at`）

### W1-D：core/session/types.rs — Session 数据结构
- 文件：`crates/mlink-core/src/core/session/types.rs`
- 职责：`Session` 结构体（links: Vec<Link>、active_id、send_seq、dedup、unacked、events）；
  `SessionEvent { LinkAdded / LinkRemoved / Switched / StreamProgress }`；
  `LinkRole { Active, Standby }`。
- 行数：≈170
- pub API：
  - `pub struct Session { /* … */ }`
  - `pub struct LinkStatus { pub link_id: String, pub kind: TransportKind, pub role: LinkRole, pub health: LinkHealth }`
  - `pub enum LinkRole { Active, Standby }`
  - `pub enum SessionEvent { LinkAdded { peer_id, link_id, kind }, LinkRemoved { peer_id, link_id, reason }, Switched { peer_id, from, to, cause }, StreamProgress { peer_id, stream_id, pct } }`

### W1-E：protocol/types.rs — Handshake 扩展
- 文件：改 `crates/mlink-core/src/protocol/types.rs`（+40）
- 职责：Handshake 新增 `#[serde(default)] session_id: Option<[u8;16]>`、
  `#[serde(default)] join_session: bool`。用于第二条 link 携带首条 link 生成的
  session_id 表明"附加到同一 session"。`MessageType::Ack` 已存在，不动。
- 行数：文件总行 +40
- pub API：
  - `pub struct Handshake { …, session_id: Option<[u8;16]>, join_session: bool }`

## Wave 2（依赖 Wave 1）

### W2-A：core/session/scheduler.rs — active/standby 选举 + switch
- 文件：`crates/mlink-core/src/core/session/scheduler.rs`
- 职责：按 capabilities.throughput_bps + health 给 link 评分；维护 hysteresis
  （3 次连续更优才 promote），暴露 `select_active / on_link_failure / on_link_recover`；
  切换时间 P99 < 5ms（纯 Rust 逻辑 + Arc swap）。
- 行数：≈150
- pub API：
  - `pub struct Scheduler { /* hysteresis counters */ }`
  - `pub fn select_active(links: &[LinkStatus]) -> Option<&str>`
  - `pub fn promote_on_failure(failing: &str, links: &[LinkStatus]) -> Option<String>`
  - `pub fn should_switchback(current: &str, best: &str, tick: u32) -> bool`

### W2-B：core/session/io.rs — 发送路径 + 重传
- 文件：`crates/mlink-core/src/core/session/io.rs`
- 职责：`send_raw(peer, MessageType, payload)` 串起 seq 分配 → 构 frame →
  active link.write；成功后写入 UnackedRing；link.write 错则原子切换并重发
  unacked；ack 帧进来回收 ring。
- 行数：≈190
- pub API：
  - `impl Session { pub async fn send(&self, msg_type: MessageType, payload: Vec<u8>) -> Result<u32> }`
  - `impl Session { pub(crate) async fn on_ack(&self, frame: &AckFrame) }`
  - `impl Session { pub(crate) async fn flush_unacked_over(&self, link_id: &str) -> Result<()> }`

### W2-C：core/session/reader.rs — 收取路径（select 多 link）
- 文件：`crates/mlink-core/src/core/session/reader.rs`
- 职责：对每条 link 起 reader 子任务 → 合并进 mpsc；主循环从 mpsc 取 frame →
  走 DedupWindow → 发 ack（cum + bitmap）→ 向上抛 NodeEvent；link EOF 驱动
  `on_link_failure` 并交给 Scheduler。
- 行数：≈180
- pub API：
  - `impl Session { pub fn spawn_readers(&self, out: broadcast::Sender<NodeEvent>) }`
  - `impl Session { pub(crate) async fn on_link_eof(&self, link_id: &str) }`

### W2-D：core/session/manager.rs — SessionManager（替代 ConnectionManager 角色）
- 文件：`crates/mlink-core/src/core/session/manager.rs`
- 职责：`HashMap<app_uuid, Arc<Session>>`；`attach_link(app_uuid, link, role_hint)`
  去重/合并；`remove_link` 拆单条；`drop_session` 断 peer；对 Node 暴露最小接口。
- 行数：≈160
- pub API：
  - `pub struct SessionManager { /* map + mutex */ }`
  - `pub fn new() -> Self`
  - `pub async fn attach_link(&self, peer_id: &str, link: Link, session_id_hint: Option<[u8;16]>) -> AttachOutcome`
  - `pub async fn get(&self, peer_id: &str) -> Option<Arc<Session>>`
  - `pub async fn drop(&self, peer_id: &str) -> Option<Arc<Session>>`
  - `pub async fn list_status(&self) -> Vec<(String, Vec<LinkStatus>)>`
  - `pub enum AttachOutcome { CreatedNew { session_id: [u8;16] }, AttachedExisting, RejectedDuplicateKind }`

### W2-E：core/node/io.rs — Node 适配层
- 文件：改 `crates/mlink-core/src/core/node/io.rs`（≈+80，替换 connections 调用点）
- 职责：保持 `send_raw / spawn_peer_reader / recv_raw` 签名不变，内部从
  `SessionManager.get(peer_id)` 拿 `Arc<Session>` 调用 `session.send` /
  读取合并流，`ConnectionManager` 退化为测试兼容 facade（不删）。
- 行数变化：原 249 → 约 260（仍 ≤200 的内部子函数拆 helper）
- pub API：不变。

### W2-F：core/node/connect.rs — attach 到 Session
- 文件：改 `crates/mlink-core/src/core/node/connect.rs`（+60）
- 职责：`register_after_handshake` 去掉 `contains -> 拒收` 分支，改调
  `SessionManager.attach_link`；处理 `AttachOutcome::AttachedExisting`
  → 只 emit `LinkAdded`，不再 emit `PeerConnected`。
- 行数变化：+60（保持 ≤200，必要时抽 helper）
- pub API：不变。

### W2-G：transport/dual_probe.rs — 第二通道主动拨号
- 文件：`crates/mlink-core/src/transport/dual_probe.rs`
- 职责：节点建立首条连接后，根据对端 Handshake.capabilities 与本地可用 transport
  决定是否拨第二条（例：BLE 首连后本地有 TCP → 探测 TCP listener）；封装
  `try_probe(other_kind, discovered_peer) -> Result<Link>`。
- 行数：≈130
- pub API：
  - `pub async fn try_probe_secondary(primary_kind: TransportKind, peer: &DiscoveredPeer, transports: &TransportSet) -> Result<Link>`
  - `pub struct TransportSet { pub ble: Option<Box<dyn Transport>>, pub tcp: Option<Box<dyn Transport>>, pub ipc: Option<Box<dyn Transport>> }`

### W2-H：daemon/session 扩展 — debug 通道事件与指令
- 文件：`crates/mlink-daemon/src/session/transport_debug.rs`
- 职责：订阅 `SessionEvent` 转发到 WS；新增入站类型 `transport_switch { peer_id, to_kind }`；
  `transport_list` 返回当前所有 peer 的 link 状态快照。
- 行数：≈170
- pub API：
  - `pub(super) async fn handle_transport_switch(...) -> bool`
  - `pub(super) async fn handle_transport_list(...) -> bool`
  - WS 新增 event 类型 `transport_state`（payload: `{ peer_id, links: [LinkStatus], active: link_id }`）

### W2-I：daemon/discovery.rs — 双 transport 并行拨号
- 文件：改 `crates/mlink-daemon/src/discovery.rs`（+50）
- 职责：当 `MLINK_DAEMON_TRANSPORT=dual` 时同时 spawn_tcp + spawn_ble，
  两套 scanner 打 peer 到同一 `peer_rx`；`connect_loop` 去重逻辑放宽——
  允许同 app_uuid 不同 wire_id 经不同 transport 并列拨号（靠 SessionManager 合并）。
- 行数变化：≈+50
- pub API：
  - `pub enum DaemonTransport { Tcp, Ble, Dual }`（扩枚举）

### W2-J：web-debug/index.html — 双通道 UI
- 文件：改 `examples/web-debug/index.html`（+180）
- 职责：新增右侧 Transport 面板：展示每个 peer 的 link 列表（kind/role/RTT/err%），
  active 高亮；按钮 "切换到 BLE/TCP" 触发 `transport_switch`；订阅
  `transport_state` 事件实时刷新；加一条"稳定性测试：1000 条 ping"脚本按钮，
  输出切换次数 / 丢包 / 重复。
- 行数变化：≈+180
- pub API：不涉及。

## 回归影响面（单通道回归路径）
- `SessionManager.attach_link` 接到首条 Link 必须行为等价于旧 `ConnectionManager.add`
- 单 link Session 的 send/recv 路径退化为一次性写 + 单 reader，性能与旧路径差距 ≤ 5%
- Handshake 无 `session_id` 字段时按旧语义（首条新 session）处理

## 总行数估算
- 新增文件：W1-A(160) + W1-B(140) + W1-C(180) + W1-D(170) + W2-A(150)
  + W2-B(190) + W2-C(180) + W2-D(160) + W2-G(130) + W2-H(170) ≈ 1630
- 改造文件：W1-E(+40) + W2-E(+80) + W2-F(+60) + W2-I(+50) + W2-J(+180) ≈ 410
- 合计 ≈ 2040 行 / 15 文件。比 Bonding 方案多 ~900 行，换来 seq/去重/重传
  集中、Node/Connection 完全解耦、测试点更明确。
