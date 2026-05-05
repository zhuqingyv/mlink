# Dual-Transport TASK LIST（arch-a v1）

> 每个 task = 一个模块 = 一个交付。交付 = 代码 + 单测 + 简 README。
> 接口签名看 INTERFACE-CONTRACTS.md §X（指向对应章节）。
> Wave 1 完成判据：8 个 PR 全合入，`cargo test -p mlink-core --lib` 全绿，触碰的文件 ≤200 行硬上限。

## 任务依赖图

```
Wave 1（并行）
  W1-A link                      无依赖
  W1-B link::pool                依赖 W1-A（import type）
  W1-C retransmit                无依赖
  W1-D health                    依赖 W1-A（import type）
  W1-E selector                  依赖 W1-A（import type）
  W1-F capabilities              无依赖
  W1-G types/Handshake/Ack       依赖 W1-F（Capabilities 嵌入 Handshake）
  W1-H errors::NoHealthyLink     无依赖

Wave 2（串接，W1 全合后）
  W2-1 connection 改造           依赖 W1-A W1-B
    ↓
  W2-2 node::connect 改            依赖 W2-1 W1-F W1-G
  W2-3 node::io 改                依赖 W2-1 W1-C W1-H
  W2-4 node::lifecycle 改         依赖 W2-1
  W2-5 heartbeat 新增              依赖 W2-3 W1-C W1-D
  W2-6 bond 新增                   依赖 W2-1 W1-B W1-E
  W2-7 NodeEvent 扩展              依赖 W1-A
  W2-8 daemon/discovery/dual       依赖 W2-9
  W2-9 daemon/discovery 拆分       独立可先行
  W2-10 daemon/protocol 扩展       依赖 W1-G
  W2-11 daemon/session/link_events 依赖 W2-7 W2-10
  W2-12 daemon/session/dispatch 扩 依赖 W2-10 W2-6
  W2-13 web-debug/index.html 扩    依赖 W2-10
  W2-14 daemon::state 扩           依赖 W1-E W2-6

Wave 3（测试，W2 主干合后）
  W3-1..W3-4 单测已随 W1 各模块交付（这里只是门禁复查）
  W3-5 core e2e                   依赖 W2-1..W2-6
  W3-6 daemon e2e                 依赖 W2-8..W2-14
  W3-7 manual verify 脚本         依赖 W3-6
```

---

## Wave 1 任务详表

### W1-A · `core::link`

**目标：** 把一条 `Arc<dyn Connection>` 和它的统计数据包装成可引用的 `Link`。
**签名：** 见 INTERFACE-CONTRACTS §1。
**完成判据：**
1. `Link::new / id / peer_id / transport_id / conn / metrics / set_health / record_*` 全部实现。
2. `LinkMetrics` 用 `parking_lot::RwLock` 包裹；读快照分离写冲突。
3. 单测：`record_rx` 累加 bytes；`set_health` 改写后 `metrics()` 读到新值。
4. 文件不 `use` tokio runtime / broadcast。
5. `wc -l mod.rs ≤ 120`。

### W1-B · `core::link::pool`

**签名：** 见 §2。
**完成判据：**
1. `add / remove / get / primary / standby / healthy / promote / failover_away_from` 实现。
2. `promote` 幂等、`remove` 被动切换逻辑（选 transport 字典序小者）。
3. 单测：
    - 空池 primary() → None
    - 加两条 link 后 primary 是第一条 Healthy
    - remove primary 自动切
    - promote 到 Degraded 返回 NoHealthy
4. `wc -l pool.rs ≤ 180`。

### W1-C · `core::retransmit`

**签名：** 见 §4。
**完成判据：**
1. `push / ack_through / due_for_retry / mark_resent / seq_le / seq_lt` 实现。
2. wrap 比较正确：`seq_le(65535, 0) == true`（判相邻窗口）；`seq_le(10, 40000) == false`（跨窗口视为过期）。
3. 单测：
    - 连 push 10 个帧 → ack_through(5) 清掉 6 条（含 0）
    - due_for_retry 只返回超时帧
    - Full 时 push 返回 `PushResult::Full`，队列不变
    - wrap 边界：push seq=65534,65535,0,1；ack_through(0) 清掉 65534 65535 0
4. `wc -l retransmit.rs ≤ 180`。

### W1-D · `core::health`

**签名：** 见 §5。
**完成判据：**
1. `HealthPolicy / HealthChecker / HealthVerdict` 实现。
2. 滞回：`consecutive_ok` 达到 `hysteresis_healthy_probes` 才从 Degraded → Healthy。
3. 单测：
    - 初始 Probing，收到 1 个 hb → 仍 Probing（未达 3 次）
    - 收到 3 次 hb → Changed(Healthy)
    - Healthy 下 15s 无 hb → Changed(Degraded)
    - Degraded 下 30s 无 hb → Changed(Dead)
    - Dead 是吸收态，之后 evaluate 返回 Unchanged
4. `wc -l health.rs ≤ 140`。

### W1-E · `core::selector`

**签名：** 见 §6。
**完成判据：**
1. `LinkSelector` trait + `BandwidthFirstSelector` + `ExplicitSelector` 实现。
2. `BandwidthFirstSelector` 只选 Healthy；多条 Healthy 按 throughput_bps 降序；同 throughput 按 transport_id 字典序。
3. 单测：
    - 没有 Healthy → None
    - BLE(5e4) + TCP(1e9) 都 Healthy → 选 TCP
    - 两个 TCP → 选 transport_id 小者
    - `ExplicitSelector(["ble","tcp"])` 返回 BLE
4. `wc -l selector.rs ≤ 100`。

### W1-F · `protocol::capabilities`

**签名：** 见 §10。
**完成判据：**
1. `Capabilities { dual_link }` + `negotiate(a, b)` 位与。
2. serde 向后兼容：反序列化缺字段用 `default`。
3. 单测：a.dual=true, b.dual=false → negotiate=false；旧 JSON `{}` → 反序列化成 `{ dual_link: false }`。
4. `wc -l capabilities.rs ≤ 80`。

### W1-G · `protocol::types` 扩展（Handshake + AckPayload）

**签名：** 见 §9 §10。
**完成判据：**
1. `Handshake` 新增 `#[serde(default)] capabilities: Capabilities`。
2. 新增 `AckPayload { watermark, bitmap }`，`#[serde(default)] bitmap`。
3. 旧版 Handshake msgpack 反序列化**不变**（加测试）。
4. `wc -l types.rs ≤ 250`（原 207 + 扩 ~40）。超就拆 `handshake.rs`。

### W1-H · `errors::NoHealthyLink`

**完成判据：**
1. `MlinkError` 新增 `NoHealthyLink { peer_id: String }` 变体。
2. `Display` + `Error` 派生。
3. 单测覆盖字符串格式（利于前端匹配）。
4. **同时**加入 `ErrorCode` wire enum（选 `0x08`），serde 往返测。

---

## Wave 2 任务详表

### W2-1 · `ConnectionManager` 改造

**签名：** §3。
**步骤：**
1. 内部 `HashMap<String, SharedConnection>` 改 `HashMap<String, LinkPool>`。
2. 保留 `add/remove/shared` 这三个老 API，作为"单 link 视图"的 wrapper：
    - `add(id, conn)` 内部包一层 `Link` + `LinkPool` + `promote`
    - `shared(id)` == `primary_conn(id)`
3. 新增 `attach_link / detach_link / pool / pool_mut / primary_conn`。
4. `MAX_LINKS_PER_PEER=4` 常量。
5. 单测：Added / Replaced / Rejected 三条路径；`detach_link` 摘到空触发 `PeerGone`。
6. `wc -l connection.rs ≤ 200`（原 377 含测试）。超就把测试拆 `tests.rs`。

**REGRESSION 必须不破：**
- 所有 `perform_handshake` 相关测试（io timeout、round trip、resume streams）**逐字**不改。

### W2-2 · `node::connect` 改造

**步骤：**
1. `register_after_handshake` 读 `peer_hs.capabilities`。
2. 若 `negotiate(local.caps, peer.caps).dual_link == false`：保持旧行为（同 peer 第二条连接拒绝）。
3. 若 `true`：同 peer、相同 transport_id 时按 `attach_link(Replaced)` 处理（关旧 link）；不同 transport_id 时 `attach_link(Added)` 并作为 standby。
4. 构造 `Link` 时调 `LinkPool::next_id` 产生 `link_id`。
5. 新增 `NodeEvent::LinkAdded` 事件发射（见 W2-7）。
6. 旧行为（RoomMismatch、DEDUP 日志）保留。

### W2-3 · `node::io` 改造

**步骤：**
1. `send_raw` 取 connection 从 `guard.shared(peer_id)` → `guard.primary_conn(peer_id)`。
2. 无 primary 时返回 `NoHealthyLink{peer_id}` 而不是 `PeerGone`。
3. 新增 `spawn_link_reader(peer_id, link_id)`：
    - 读到 EOF 只 `detach_link(peer_id, link_id)`，不清整个 peer
    - 若 `DetachOutcome::PeerGone` → 再走现有 `PeerDisconnected` 路径
    - 若还有别的 link → 发 `LinkRemoved` 事件 + 若摘掉的是 primary 再发 `PrimaryChanged`
4. 原 `spawn_peer_reader` 改为：取该 peer 所有 link，逐个 `spawn_link_reader`。
5. send_raw 成功后：调 `retransmit_queue.push(PendingFrame{..})`。

### W2-4 · `node::lifecycle` 改造

**步骤：**
1. 新增 `disconnect_link(peer_id, link_id)`：close 单条并走 detach 分支。
2. `disconnect_peer(peer_id)` 内部循环调 `disconnect_link`。
3. `attach_connection` 测试钩子改为产出单 link LinkPool（保持旧测试兼容）。

### W2-5 · `node::heartbeat` 新增

**职责：** 一个后台 loop（`spawn_heartbeat_task`）每秒 tick 做：
1. 对 peer_states 每 peer，对 peer.links 每 link：
    - 如果距 `last_heartbeat_rx` > `HealthPolicy.heartbeat_interval`：发一个 Ping 帧（新增 `MessageType::Heartbeat`，已存在）
    - 调 `HealthChecker::evaluate`，若 `Changed(new)`：`link.set_health(new)` + 发 `LinkHealthChanged` 事件；若 `new == Dead`：触发 `disconnect_link`
2. 扫 `RetransmitQueue::due_for_retry(now, retry_interval)`：
    - 对每条 due 帧，用当前 primary_conn 重发
    - 若重发失败 + primary 变 Degraded：换 standby 再试一次
    - `mark_resent(seq, now)`
3. 单测用 `tokio::time::pause()` 驱动虚拟时间。

### W2-6 · `node::bond`

**职责：**
1. 持有 `Arc<dyn LinkSelector>`（默认 BandwidthFirst，可被 `set_selector` 替换）。
2. 订阅 `LinkHealthChanged` / `LinkAdded` / `LinkRemoved`，对该 peer 跑一次 selector.pick，若结果 ≠ 当前 primary 就 `pool.promote`。
3. 向外发 `PrimaryChanged`。
4. `set_preferred_transport(peer_id, transport_id)`：临时替换该 peer 的 selector 为 `ExplicitSelector`。10 分钟后自动失效回默认。

### W2-7 · `NodeEvent` 扩展

```rust
pub enum NodeEvent {
    // 旧变体保留
    LinkAdded { peer_id, link_id, transport_id, health },
    LinkRemoved { peer_id, link_id, reason: String },
    LinkHealthChanged { peer_id, link_id, old: LinkHealth, new: LinkHealth },
    PrimaryChanged { peer_id, old: Option<String>, new: String, reason: String },
}
```

### W2-8 · `daemon::discovery::dual`

**步骤：**
1. `DualMode::from_env()` 读 `MLINK_DAEMON_TRANSPORT`：`dual` → Both，其他维持旧。
2. `spawn_dual(node, DualMode::Both)` 同时调用已拆分后的 `spawn_tcp` 与 `spawn_ble`。

### W2-9 · `daemon::discovery` 拆分

**目标：** 318 行的单文件拆为 `mod.rs` + `tcp.rs` + `ble.rs` + `connect_loop.rs`。
**契约：** 只是物理拆分 + 导出，**行为完全不变**（由旧 tests 守门）。

### W2-10 · `daemon::protocol` 扩展

新增序列化结构（配合 §11）：
```rust
pub struct LinkAddedPayload { peer_id, link_id, transport_id, health }
pub struct LinkRemovedPayload { peer_id, link_id, reason }
pub struct LinkHealthPayload { peer_id, link_id, health, rtt_ewma_ms, bytes_tx, bytes_rx }
pub struct PrimaryChangedPayload { peer_id, old: Option<String>, new: String, reason }
pub struct SetPreferredTransportRequest { peer_id, transport_id }
pub struct DisconnectLinkRequest { peer_id, link_id }
```

### W2-11 · `daemon::session::link_events`

**步骤：**
1. 订阅 node_events；对 `LinkAdded/...` 编码为对应 WS frame，广播到所有 session。
2. 不做 room 过滤：UI 需要看到全局 link 活动。

### W2-12 · `daemon::session::dispatch` 扩展

1. 新增 3 handler 文件：`handlers_links.rs` 放 list_links / set_preferred_transport / disconnect_link。
2. `list_links` 从 `state.node` 一次性扫所有 peer → LinkPool → Link 统计，回一个 `links` 帧。
3. `set_preferred_transport` 转发到 `state.node.set_preferred_transport(...)`（W2-6）。
4. `disconnect_link` 仅在 `MLINK_DAEMON_DEV=1` 时开放；否则 `bad_type` 拒绝。

### W2-13 · web-debug 扩展

见 INTERFACE-CONTRACTS §12 行为规范。最小实现：
1. 顶部加 Toggle：`Dual-Transport [on/off]`。
2. peer 列表每行展开成 link 表格 + 当前 primary 徽章。
3. 右侧事件流固定 10 行，按 primary_changed / link_health / link_added / link_removed 色码。
4. 两按钮 + 抖动按钮，接 WS 上行 type。

### W2-14 · `daemon::state`

`DaemonState` 加：
```rust
pub selector: Arc<parking_lot::RwLock<Arc<dyn LinkSelector>>>, // 可热替换
```
在 `build_state` 里 `Arc::new(BandwidthFirstSelector)` 初始化并注入 Node。

---

## 交付门禁

每个 task 交付 PR 时需在 PR body 勾选：

- [ ] `wc -l` 每个新文件 ≤ 限定行数
- [ ] `cargo test -p mlink-core --lib` 本 task 新增测试全绿
- [ ] 触碰的旧测试 **不改**
- [ ] 没有引入新的 tokio::spawn 到 Wave 1 模块
- [ ] 没有 `unwrap()` 在非测试路径
- [ ] README 1 行目标 + 1 段接口说明（Wave 2 还需时序图）
