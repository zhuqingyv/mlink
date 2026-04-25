# Dual-Transport TASK LIST（最终版）

> 每个 task = 一个模块 = 一个 PR 交付。交付 = 代码 + 单测 + 1 行 README（Wave 2 额外 1 段时序图注释）。
> 接口签名全部以 `INTERFACE-CONTRACTS.md §N` 为准，禁止偏离。
> 每个代码文件 `wc -l ≤ 200`，超则拆 `tests.rs`。
> 不 mock。

## 总表

| Task ID | 模块 | Wave | 依赖 | 预估行数 | 文件路径 | 状态 |
| --- | --- | --- | --- | --- | --- | --- |
| W1-A | `protocol::ack` | 1 | — | ≤160 | `crates/mlink-core/src/protocol/ack.rs` | pending |
| W1-B | `protocol::seq` | 1 | — | ≤140 | `crates/mlink-core/src/protocol/seq.rs` | pending |
| W1-C | `core::link` | 1 | — | ≤180 | `crates/mlink-core/src/core/link.rs` | pending |
| W1-D | `core::session::types` | 1 | W1-A, W1-B, W1-C（类型引用） | ≤170 | `crates/mlink-core/src/core/session/types.rs` | pending |
| W1-E | `Handshake` 扩展 | 1 | — | +40 | `crates/mlink-core/src/protocol/types.rs` | pending |
| W1-F | `NoHealthyLink` 错误 | 1 | — | +20 | `crates/mlink-core/src/protocol/errors.rs` | pending |
| W2-2 | `core::session::io`（含内联 scheduler） | 2 | W1-A, W1-B, W1-C, W1-D | ≤200 | `crates/mlink-core/src/core/session/io.rs` | pending |
| W2-3 | `core::session::reader` | 2 | W1-A, W1-B, W1-C, W1-D, W2-2 | ≤180 | `crates/mlink-core/src/core/session/reader.rs` | pending |
| W2-4 | `core::session::manager` | 2 | W1-C, W1-D, W2-2, W2-3 | ≤160 | `crates/mlink-core/src/core/session/manager.rs` | pending |
| W2-5 | `core::node::io` 改造 | 2 | W2-4 | 改（≤200） | `crates/mlink-core/src/core/node/io.rs` | pending |
| W2-6 | `core::node::connect` 改造 | 2 | W2-4, W1-E | 改（≤200） | `crates/mlink-core/src/core/node/connect.rs` | pending |
| W2-7 | `core::node::lifecycle` 扩展 | 2 | W2-4 | 改 +40 | `crates/mlink-core/src/core/node/lifecycle.rs` | pending |
| W2-8 | `Node` 新增 dual API | 2 | W2-4, W2-5, W2-6, W2-7 | +40 | `crates/mlink-core/src/core/node/mod.rs` | pending |
| W2-9 | `NodeEvent` 扩展 | 2 | W1-D | +30 | `crates/mlink-core/src/core/node/types.rs` | pending |
| W2-10 | `ConnectionManager` 兼容壳 | 2 | W2-4 | 改 +60 | `crates/mlink-core/src/core/connection.rs` | pending |
| W2-11 | `transport::dual_probe` | 2 | W1-C | ≤130 | `crates/mlink-core/src/transport/dual_probe.rs` | pending |
| W2-12 | `daemon::discovery` 拆分 + Dual | 2 | W2-6, W2-11 | 每文件≤180 | `crates/mlink-daemon/src/discovery/{mod,tcp,ble}.rs` | pending |
| W2-13 | `daemon::protocol` 扩展 | 2 | W1-E | +80 | `crates/mlink-daemon/src/protocol.rs` | pending |
| W2-14 | `daemon::session::transport_debug` | 2 | W2-8, W2-13 | ≤170 | `crates/mlink-daemon/src/session/transport_debug.rs` | pending |
| W2-15 | `daemon::session::dispatch` 挂 handler | 2 | W2-14 | 改 +20 | `crates/mlink-daemon/src/session/dispatch.rs` | pending |
| W2-16 | `daemon::state` 注入 forwarder | 2 | W2-14 | 改 +15 | `crates/mlink-daemon/src/state.rs` | pending |
| W3-11 | core 双链 e2e | 3 | W2-2..W2-10 | ≤250 | `crates/mlink-core/tests/dual_link_e2e.rs` | pending |
| W3-12 | daemon WS e2e | 3 | W2-12..W2-16 | ≤250 | `crates/mlink-daemon/tests/dual_ws.rs` | pending |
| W3-13 | web-debug UI | 3 | W2-13, W2-14 | +180 | `examples/web-debug/index.html` | pending |
| W3-14 | 手工验收手册 | 3 | W3-12, W3-13 | doc | `docs/dual-transport-final/MANUAL-VERIFY.md` | pending |

**Wave 1/2 总模块：23 个（新增 11 文件 + 改造 12 文件）；Wave 3：4 任务。**

---

## 依赖拓扑图

```
Wave 1（完全并行）
  W1-A protocol/ack.rs           ── 独立
  W1-B protocol/seq.rs           ── 独立
  W1-C core/link.rs              ── 独立（只引 transport::Connection）
  W1-E Handshake +字段           ── 独立
  W1-F MlinkError +变体          ── 独立
  W1-D core/session/types.rs     ── 引用 A/B/C 的类型名（合入顺序放最后）

Wave 2（组依赖）
  组 A（Session 引擎，强串行）
    W2-2 session/io.rs           ← W1-A W1-B W1-C W1-D
    W2-3 session/reader.rs       ← W1-A W1-B W1-C W1-D W2-2（共享 Session::impl block）
    W2-4 session/manager.rs      ← W1-C W1-D W2-2 W2-3

  组 B（Node 接入，W2-4 合入后启动）
    W2-9  NodeEvent +变体        ← W1-D（可与组 A 并行）
    W2-10 ConnectionManager 壳   ← W2-4
    W2-5  node/io.rs             ← W2-4 W2-9
    W2-6  node/connect.rs        ← W2-4 W1-E W2-9
    W2-7  node/lifecycle.rs      ← W2-4
    W2-8  Node dual API          ← W2-4 W2-5 W2-6 W2-7

  组 C（Transport + Daemon）
    W2-11 dual_probe             ← W1-C（可与组 A 并行）
    W2-12 discovery 拆分+Dual    ← W2-6 W2-11
    W2-13 daemon/protocol        ← W1-E（可与组 A 并行）
    W2-14 transport_debug        ← W2-8 W2-13
    W2-15 dispatch 挂 handler    ← W2-14
    W2-16 state 注入 forwarder   ← W2-14

Wave 3
  W3-11 core e2e                 ← 组 A + 组 B 全合
  W3-12 daemon WS e2e            ← 组 C 全合
  W3-13 web-debug UI             ← W2-13 W2-14
  W3-14 手工验收手册             ← W3-12 W3-13
```

## 并行度分析

| 阶段 | 可并行任务数 | 临界路径 |
| --- | --- | --- |
| Wave 1 | 6 | W1-D（引用 A/B/C，合入最后） |
| Wave 2 组 A | 1（W2-2 → W2-3 → W2-4 串） | W2-4 |
| Wave 2 组 B + C 并行 | 6（W2-5/6/7/8 串；W2-9/10/11/13 全并行；W2-12 等 6；W2-14/15/16 等 8+13） | W2-8 → W2-14 |
| Wave 3 | W3-11 / W3-12 / W3-13 全并行；W3-14 最后 | W3-14 |

**关键路径长度：** W1-D → W2-2 → W2-3 → W2-4 → W2-5 → W2-8 → W2-14 → W3-12 = **8 步串行**（其余全可并行打满人头）。

---

## Wave 1 任务详表

### W1-A · `protocol::ack`
**契约：** INTERFACE-CONTRACTS §1。
**完成判据：**
1. `AckFrame` 定长 20 字节编解码（4 byte u32 ack + 16 byte u128 bitmap，小端）。
2. `UnackedRing<T>` capacity=256，`push` 满时丢最旧并返回 Some + warn 日志。
3. `ack_cum(ack)` O(256) 扫描清除；`sack(base, bitmap)` 按位清。
4. 5 条单测（W3-1 清单），`wc -l ≤ 160`。

### W1-B · `protocol::seq`
**契约：** §2。
**完成判据：**
1. `SeqGen::next` 单调递增，过 u32::MAX 后 wrap 到 0（允许）。
2. `DedupWindow::observe` 用 `wrapping_sub` 判方向；`seq_diff < 2^31` 视作"后续"。
3. `snapshot` 返回 `(last, bitmap)`，bitmap 映射到 `last..last+128` 的已见位。
4. 5 条单测（W3-2），`wc -l ≤ 140`。

### W1-C · `core::link`
**契约：** §3。
**完成判据：**
1. `Link::send/recv` `&self` 直接代理 `Arc<dyn Connection>`；不新 spawn。
2. `health` 用 `std::sync::Mutex<LinkHealth>`，短锁读写；不得跨 await。
3. `TransportKind::as_wire/from_wire` 覆盖 4 种 + 未知返回 None。
4. 5 条单测（W3-3），`wc -l ≤ 180`。

### W1-D · `core::session::types`
**契约：** §4。
**完成判据：**
1. `Session` struct 字段全部 `pub(crate)`；外部只通过 §7 §8 方法访问。
2. `MAX_LINKS_PER_PEER = 4`、`SWITCHBACK_HYSTERESIS = 3`、`ACK_MAX_DELAY_MS = 50`、`ACK_MAX_PENDING = 8`、`EXPLICIT_OVERRIDE_TTL_SECS = 600` 五个常量齐全。
3. `SchedulerState` 默认构造可用；`SessionEvent` 全 `Clone`。
4. 3 条单测（W3-4），`wc -l ≤ 170`。

### W1-E · `Handshake` 扩展
**契约：** §5。
**完成判据：**
1. 新增 2 个 `#[serde(default)]` 字段：`session_id` / `session_last_seq`。
2. 既有测试 `constants_are_correct / message_type_round_trip / ...` 全绿。
3. 3 条新单测（W3-9：`with_session_id / without_session_id_defaults_none / legacy_wire_bytes_still_decodes`）。
4. `wc -l ≤ 250`（原 406 含测试，超就拆 `handshake.rs`）。

### W1-F · `NoHealthyLink` 错误
**契约：** §6。
**完成判据：**
1. `MlinkError::NoHealthyLink { peer_id }` 变体 + `Display` 覆盖。
2. `ErrorCode::NoHealthyLink = 0x08` wire 映射 + 往返测。
3. 2 条单测（W3-10）。

---

## Wave 2 任务详表

### W2-2 · `core::session::io`（含内联 scheduler）
**契约：** §7。
**步骤：**
1. `Session::send` 三步：分配 seq → 构 Frame → `active link.send`；成功后 `unacked.push`。
2. 写失败：`promote_on_failure` → 切 active → retry 一次；仍失败返回 `Err(NoHealthyLink)`。
3. `on_ack` 调 `unacked.ack_cum(ack.ack)` + `unacked.sack(ack.ack, ack.bitmap)`。
4. `flush_unacked_over(link_id)` 把 `unacked_since(ack+1)` 全部经新 link 重发（不改 seq）。
5. 内联 scheduler：`score / pick_best / promote_on_failure / should_switchback`。
6. **锁顺序：** 先 `active.read` → 取 active id → clone → drop lock → `links.read` → 按 id 取 Arc<Link> → drop → `link.send.await`。
7. 5 条单测（W3-5 + W3-6），`wc -l ≤ 200`。超则拆 `scheduler.rs`（仍然内联算法而不是独立模块）。

### W2-3 · `core::session::reader`
**契约：** §8。
**步骤：**
1. `spawn_readers` 对每条 link `tokio::spawn` 一个子任务 → 解帧 → 打入 `mpsc::unbounded_channel`。
2. 中央任务读 mpsc：`dedup.observe(seq)` → Accept 时广播 `NodeEvent::MessageReceived` + 累加 ack pending。
3. Ack 合并 timer：`tokio::time::interval(50ms)` + pending 计数器；任一触发即经 active link 发 AckFrame。
4. 收到 `MessageType::Ack` payload → `decode_ack` → `Session::on_ack`；不发 NodeEvent。
5. link EOF → `on_link_eof` → 关闭子任务 + 从 `links` 摘 + emit `LinkRemoved` + 触发 `promote_on_failure`。
6. 4 条单测（W3-7），`wc -l ≤ 180`。

### W2-4 · `core::session::manager`
**契约：** §9。
**步骤：**
1. `attach_link` 决策树按 `AttachOutcome` 四变体覆盖；`session_id` 由首条用 `getrandom` 或等价 CSPRNG 生成。
2. `detach_link` 摘完后若 session 为空自动 drop；返回 bool 告诉 caller "peer gone"。
3. `list_status` 聚合所有 peer → 所有 link 的 LinkStatus 快照。
4. 4 条单测（W3-8），`wc -l ≤ 160`。

### W2-5 · `core::node::io` 改造
**步骤：**
1. `send_raw`：`self.sessions.get(peer_id).ok_or(PeerGone)?` → `session.send(...)`。
2. `recv_raw`：通过 session reader 线路返回（若无 session → `PeerGone`）。
3. `spawn_peer_reader`：改为 `self.sessions.get(peer_id)?.spawn_readers(self.events_tx.clone())`；不再 per-peer 只 reader。
4. `send_heartbeat / check_heartbeat` 保持语义：通过 session active link 发 Heartbeat frame；`last_heartbeat` 字段由 Link health 内部推进。
5. 所有既有 Node 测试逐字通过（A1 清单）。

### W2-6 · `core::node::connect` 改造
**步骤：**
1. `register_after_handshake` 去掉"contains → 拒收"分支；改调 `sessions.attach_link(peer_id, link, peer_hs.session_id)`。
2. 根据 `AttachOutcome`：
    - `CreatedNew { session_id }` → 构造响应 Handshake 时回填 `session_id`；emit `PeerConnected` + `LinkAdded`。
    - `AttachedExisting` → 只 emit `LinkAdded`。
    - `RejectedDuplicateKind` / `RejectedCapacity` → `conn.close().await` + 返回 Err。
3. 旧的 `RoomMismatch` / DEDUP 日志逻辑完整保留。

### W2-7 · `core::node::lifecycle`
**步骤：**
1. `disconnect_peer` 改调 `sessions.drop(peer_id)` 关所有 link + emit `PeerDisconnected`。
2. 新增 `disconnect_link(peer_id, link_id)` → `sessions.detach_link`；若返回 `peer_gone=true`，补发 `PeerDisconnected`。
3. `attach_connection`（test hook）保持签名；内部转 `sessions.attach_link` 构造单 link Session。

### W2-8 · `Node` dual API
**步骤：**
1. 在 `node/mod.rs` 新增 `impl Node` 块，只写 4 个新 API + 1 个 `disconnect_link`。
2. `switch_active_link`：写 `explicit_override` 表（10 分钟 TTL）+ 立即 `session.promote` 到对应 kind 的 link_id；若无匹配 kind 返回 `MlinkError::HandlerError("transport_not_available")`。
3. `explicit_override` 的过期检查放 `Session::pick_best` 开头：查 override 若过期则清除，否则强制选 kind 匹配的 link。

### W2-9 · `NodeEvent` 扩展
**步骤：**
1. 在 `node/types.rs` 加 3 个变体；`Clone` 已派生无需改。
2. 现有 `broadcast::channel(128)` 容量足够（link 事件稀疏）。

### W2-10 · `ConnectionManager` 兼容壳
**步骤：**
1. `ConnectionManager` 新字段：`sessions: Option<Arc<SessionManager>>`（Node 构造时注入）。
2. `add/shared/remove/contains/list_ids/count` 全部改为代理调用 `sessions`，保持语义。
3. 既有测试（A1）逐字通过。
4. `wc -l ≤ 200`；超则拆测试到 `connection_tests.rs`。

### W2-11 · `transport::dual_probe`
**步骤：**
1. `try_probe_secondary(primary_kind, peer, &mut TransportSet)`：
    - Tcp primary + 本地有 BLE + peer 在 BLE 扫描范围 → 走 `ble.connect`
    - Ble primary + 本地有 TCP + peer metadata 含 TCP 端口 → 走 `tcp.connect`
2. 返回 `Link`（由 caller 传入 `SessionManager.attach_link`）。
3. 失败不 panic；返回 `MlinkError::TransportError`。

### W2-12 · `daemon::discovery` 拆分 + Dual
**步骤：**
1. 物理拆 318 行为 `mod.rs / tcp.rs / ble.rs`（行为零改动，由现有 `ws_protocol` 测试守门）。
2. `mod.rs` 新增 `DaemonTransport::Dual`；`spawn` 时并行调两个子 spawn。
3. `from_env` 读 `MLINK_DAEMON_TRANSPORT=dual` 返回 Dual。
4. Dual 模式下两 scanner 发现同 `peer_id` 会各自 `node.connect_peer` / `node.accept_incoming`，第二条自然触发 `attach_link(AttachedExisting)`。

### W2-13 · `daemon::protocol` 扩展
**步骤：**
1. 加 6 个 payload struct：`LinkAddedPayload` / `LinkRemovedPayload` / `PrimaryChangedPayload` / `TransportStatePayload` / `TransportSwitchRequest` / `DisconnectLinkRequest`。
2. 在 `WsEvent`（若有，或直接用 serde_json::Value）的 `type` 常量表新增 6 项。

### W2-14 · `daemon::session::transport_debug`
**步骤：**
1. `spawn_session_event_forwarder(state)` 启后台任务：订阅 `node.subscribe_session_events` → 按事件类型编 JSON → broadcast 到所有 WS 会话。
2. `handle_transport_list`：调 `node.peer_link_status` 遍历所有 peer → 构 `transport_state` 响应。
3. `handle_transport_switch`：解析 `to_kind`（白名单 `ble/tcp/ipc`）→ `node.switch_active_link`。
4. `handle_disconnect_link`：检查 `MLINK_DAEMON_DEV=1`；通过 → `node.disconnect_link`；否则 `bad_type`。

### W2-15 · `dispatch` 挂 handler
**步骤：**
1. 在 `session/dispatch.rs` 的 `match type` 里加 3 条 arm 分发到 `transport_debug` 的三个 handler。
2. `wc -l` 保持当前文件上限。

### W2-16 · `daemon::state`
**步骤：**
1. 在 `build_state`（或 daemon 启动流程）最后一步调 `transport_debug::spawn_session_event_forwarder(state.clone())`。

---

## Wave 3 任务详表

### W3-11 · core 双链 e2e（`tests/dual_link_e2e.rs`）
**场景（必过）：**
1. `basic_bond`：MockTransport pair × 2（A↔B 两条），先建 BLE link，再建 TCP link → `peer_link_status` 返回 2 条，active 为 TCP。
2. `primary_dies_seq_continuous`：发 10 条消息，第 5 条后强制断 TCP → 6..10 经 BLE 无丢无重，对端 seq 连续 0..9。
3. `both_down_then_one_up`：双断 → 起一条 TCP → unacked 重发，对端收到的帧 `resent_count == 1`。
4. `downgrade_legacy_peer`：对端 Handshake 不带 `session_id` → 第二条连接被拒 `RejectedDuplicateKind`。
5. `backpressure_at_max_pending`：调小 ACK_MAX_PENDING 或 UnackedRing cap → 发到满触发重发而非 panic。

### W3-12 · daemon WS e2e（`tests/dual_ws.rs`）
**场景：**
1. `ws_transport_list_returns_sessions_snapshot`
2. `ws_transport_switch_changes_active_and_broadcasts_event`
3. `ws_transport_state_fires_on_link_added`
4. `ws_transport_state_fires_on_switched`
5. `legacy_client_ignores_new_types`（不订阅新 type 的 session 不收新帧，不报错）
6. `disconnect_link_dev_only`

### W3-13 · web-debug UI
参照 INTERFACE-CONTRACTS §14 行为规范实现。

### W3-14 · 手工验收手册
`docs/dual-transport-final/MANUAL-VERIFY.md`，包含：
- D1 双通道并存主路径
- D2 毫秒级故障切换（5s 内 TCP→BLE 切换 P99 ≤ 10ms wall clock）
- D3 不丢不重（100KB 大消息切换不破坏流）
- D4 带宽自适应（TCP vs BLE 时长比 ≥ 10×）
- D5 前端 UI 检查表

---

## 交付门禁（每个 PR）

- [ ] `wc -l <file>` 每个新/改文件 ≤ 限定行数（超 = 打回拆分）
- [ ] `cargo test -p mlink-core --lib` 全绿（含本 task 新增单测）
- [ ] `cargo test --workspace` 全绿（A1/A2/A3 既有 459 测试零退化）
- [ ] `cargo clippy --workspace -- -D warnings` 无新警告
- [ ] 不引入 `unwrap()` 于非测试路径
- [ ] PR body 勾选本 task 所在 Wave 的完成判据
- [ ] Wave 2 PR 需附 1 张时序图（mermaid 贴 PR body）

## 时间预估

- Wave 1：1.5 天 / 6 人并行
- Wave 2 组 A（W2-2/3/4）：1.5 天 / 2~3 人串联
- Wave 2 组 B + C 并行：2 天 / 6 人
- Wave 3：2 天 / 3 人
- **总计 ≈ 5 工日（打满 6~8 人并行）** 
