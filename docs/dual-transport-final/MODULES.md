# Dual-Transport 模块清单（最终合成版）

> **架构主线：** arch-b 的 Session 分治（聚合多 link + 集中 seq/dedup/retransmit） + arch-a 的简化点（selector/scheduler 不独立模块、硬上限 4、hysteresis=3、ExplicitSelector 10 分钟过期）。
>
> **范围：** BLE + TCP 两种 transport。Wi-Fi Direct / Thread 等未来扩展不预留。
>
> **契约硬约束：**
> - 每个代码文件 ≤ 200 行（含测试；超则拆 tests.rs）。
> - 不 mock。
> - 向后兼容：旧客户端 Handshake 不带 `session_id` 字段 → 走单 link Session 路径。
> - 总行数 ≤ ~1500（arch-b 2040 精简后目标）。
>
> **签名冻结：** 所有接口以 INTERFACE-CONTRACTS.md 为准，开发时禁止偏离。

---

## Wave 1 —— 非业务，全并行（6 模块）

| # | 模块 | 文件路径 | 职责 | 行数 | pub API 摘要 |
| --- | --- | --- | --- | --- | --- |
| W1-A | `protocol::ack` | `crates/mlink-core/src/protocol/ack.rs` | `AckFrame{ack,bitmap}` 编解码 + `UnackedRing<T>` ring buffer（cap 256） | ≤160 | `AckFrame` / `encode_ack` / `decode_ack` / `UnackedRing::{new,push,ack_cum,sack,unacked_since,len}` |
| W1-B | `protocol::seq` | `crates/mlink-core/src/protocol/seq.rs` | u32 `SeqGen` + 128 位 `DedupWindow` → `Accept/Duplicate/Stale` | ≤140 | `SeqGen::{new,next}` / `DedupWindow::{new,observe,snapshot}` / `Observation` |
| W1-C | `core::link` | `crates/mlink-core/src/core/link.rs` | 包装 `Arc<dyn Connection>` + health metrics + kind/caps/id | ≤180 | `Link::{new,id,kind,caps,health,send,recv,close}` / `LinkHealth` / `TransportKind` |
| W1-D | `core::session::types` | `crates/mlink-core/src/core/session/types.rs` | `Session` 数据结构、`SessionEvent`、`LinkStatus`、`LinkRole`、`PendingFrame`、`SwitchCause` | ≤170 | 见契约 §4 |
| W1-E | `protocol::types` 扩展 | `crates/mlink-core/src/protocol/types.rs`（+40） | `Handshake` 新增 `#[serde(default)] session_id: Option<[u8;16]>` + `session_last_seq: u32` | +40 | 见契约 §9 |
| W1-F | `protocol::errors` 扩展 | `crates/mlink-core/src/protocol/errors.rs`（+20） | `MlinkError::NoHealthyLink { peer_id }` + `ErrorCode::NoHealthyLink = 0x08` | +20 | `MlinkError` 新变体 + serde 往返 |

**Wave 1 纯净度自查：**
- 全部不 `use` `tokio::spawn` / `broadcast::Sender`（W1-D 内部持 `broadcast::Sender` 结构字段，但不 spawn）。
- 允许跨 W1 模块 `use` 类型（参考既验证范式）。
- W1-A/B/C 任务间互不阻塞；W1-D 需 W1-A/B/C 编译完成（只取类型名），开发并行、合入串行（D 最后合）。

---

## Wave 2 —— 业务胶水（依赖排序）

| # | 模块 | 文件路径 | 职责 | 行数 | 依赖 |
| --- | --- | --- | --- | --- | --- |
| W2-1 | `core::session::scheduler` 内联 | `crates/mlink-core/src/core/session/io.rs`（包含 scheduler 逻辑） | active/standby 选举 + 切换评分 + hysteresis=3 防抖 | 合入 W2-2 | W1-C, W1-D |
| W2-2 | `core::session::io` | `crates/mlink-core/src/core/session/io.rs` | `Session::send` 发送路径 + UnackedRing 写入 + `on_ack` + `flush_unacked_over` + 内联 scheduler 算法 | ≤200 | W1-A, W1-B, W1-C, W1-D |
| W2-3 | `core::session::reader` | `crates/mlink-core/src/core/session/reader.rs` | 多 link reader 合并 + DedupWindow + cumulative+SACK ack 生成（50ms/8 条触发）+ EOF 驱动 failover | ≤180 | W1-A, W1-B, W1-C, W1-D, W2-2 |
| W2-4 | `core::session::manager` | `crates/mlink-core/src/core/session/manager.rs` | `SessionManager`：`HashMap<peer_id, Arc<Session>>` + `attach_link` 合并 + `drop` | ≤160 | W1-C, W1-D, W2-2, W2-3 |
| W2-5 | `core::node::io` 改造 | `crates/mlink-core/src/core/node/io.rs` | `send_raw`/`recv_raw` 改由 `SessionManager.get(peer_id)` → `session.send/...`；`spawn_peer_reader` 改调 `session.spawn_readers` | 改（保持≤200） | W2-4 |
| W2-6 | `core::node::connect` 改造 | `crates/mlink-core/src/core/node/connect.rs` | `register_after_handshake` 改调 `SessionManager.attach_link`，按 `AttachOutcome` 发 `LinkAdded` / 正常 `PeerConnected` | 改（保持≤200） | W2-4, W1-E |
| W2-7 | `core::node::lifecycle` 扩展 | `crates/mlink-core/src/core/node/lifecycle.rs` | 新增 `disconnect_link(peer,link)`；`disconnect_peer` 改为 `SessionManager.drop(peer)` 关所有 link | 改 +40 | W2-4 |
| W2-8 | `core::node` 新增 API | `crates/mlink-core/src/core/node/mod.rs`（+40） | `attach_secondary_link` / `peer_link_status` / `switch_active_link` / `subscribe_session_events` | +40 | W2-4 |
| W2-9 | `core::node::types::NodeEvent` 扩展 | `crates/mlink-core/src/core/node/types.rs`（+30） | 新增 `LinkAdded/LinkRemoved/Switched` 变体（与 `SessionEvent` 一一对应） | +30 | W1-D |
| W2-10 | `core::connection` 兼容壳 | `crates/mlink-core/src/core/connection.rs`（改） | `add/shared/remove` 内部转 `SessionManager`，保留签名确保既有测试绿 | 改 +60 | W2-4 |
| W2-11 | `transport::dual_probe` | `crates/mlink-core/src/transport/dual_probe.rs` | 首连后按对端 capabilities + 本地可用 transport 拨第二条：`try_probe_secondary` | ≤130 | W1-C |
| W2-12 | `daemon::discovery` 拆分 + Dual | `crates/mlink-daemon/src/discovery/{mod.rs,tcp.rs,ble.rs}` | 318 行拆分 + `DaemonTransport::Dual` + 并行两 scanner | 每文件 ≤180 | W2-6, W2-11 |
| W2-13 | `daemon::protocol` 扩展 | `crates/mlink-daemon/src/protocol.rs` | 新增 `transport_state` / `transport_list` / `transport_switch` / `link_added` / `link_removed` / `primary_changed` 的 serde | 改 +80 | W1-E |
| W2-14 | `daemon::session::transport_debug` | `crates/mlink-daemon/src/session/transport_debug.rs` | 订阅 session events 转 WS + `handle_transport_list` / `handle_transport_switch` | ≤170 | W2-8, W2-13 |
| W2-15 | `daemon::session::dispatch` 挂 handler | `crates/mlink-daemon/src/session/dispatch.rs`（+20） | 挂 `transport_list` / `transport_switch` 两路由；`disconnect_link` 走 `MLINK_DAEMON_DEV=1` 开关 | 改 +20 | W2-14 |
| W2-16 | `daemon::state` 扩展 | `crates/mlink-daemon/src/state.rs`（+15） | 启动时 `spawn_session_event_forwarder(state)` | 改 +15 | W2-14 |

---

## Wave 3 —— 测试 + UI（并行 W2 后半期）

| # | 模块 | 路径 | 类别 | 职责 |
| --- | --- | --- | --- | --- |
| W3-1 | ack 单测 | `protocol/ack.rs` 内嵌 | unit | `ack_frame_roundtrip_20_bytes`、`unacked_ring_push_overflow_returns_oldest`、`ack_cum_releases_range`、`sack_bitmap_holes_not_released`、`unacked_since_returns_sorted` |
| W3-2 | seq 单测 | `protocol/seq.rs` 内嵌 | unit | `seq_gen_monotonic_wraps_cleanly`、`dedup_window_accepts_new_sequential`、`dedup_window_marks_duplicate_inside_window`、`dedup_window_returns_stale_beyond_128`、`dedup_snapshot_round_trips_into_ack_frame` |
| W3-3 | link 单测 | `core/link.rs` 内嵌 | unit | `link_send_delegates_to_connection`、`link_recv_tracks_rtt_ema`、`link_health_err_rate_increments_on_error`、`link_close_drops_inner_connection`、`link_caps_exposed_readonly` |
| W3-4 | session types 单测 | `core/session/types.rs` 内嵌 | unit | `session_new_empty_links_and_no_active`、`session_event_broadcast_delivers_to_subscriber`、`session_id_generation_is_random_unique` |
| W3-5 | scheduler 单测 | `core/session/io.rs` 内嵌 | unit | `scheduler_picks_higher_throughput_active`、`scheduler_hysteresis_blocks_premature_switchback`（3 连优才切）、`scheduler_promote_on_failure_bypasses_hysteresis` |
| W3-6 | io 单测 | `core/session/io.rs` 内嵌 | unit | `session_send_assigns_monotonic_seq`、`session_send_fails_over_on_active_write_error`、`session_flush_unacked_over_new_link_preserves_order`、`session_unacked_ring_trims_on_cum_ack`、`session_unacked_ring_trims_on_sack_bitmap` |
| W3-7 | reader 单测 | `core/session/reader.rs` 内嵌 | unit | `session_reader_merges_frames_from_two_links`、`session_reader_emits_ack_within_50ms`、`session_reader_drops_duplicate_seq_silently`、`session_reader_on_eof_removes_link_and_emits_event` |
| W3-8 | manager 单测 | `core/session/manager.rs` 内嵌 | unit | `manager_attach_first_link_creates_new_session`、`manager_attach_second_link_with_matching_session_id_joins`、`manager_rejects_duplicate_kind`、`manager_drop_closes_all_links` |
| W3-9 | handshake serde 单测 | `protocol/types.rs` 内嵌 | unit | `handshake_serde_with_session_id`、`handshake_serde_without_session_id_defaults_none`、`handshake_serde_legacy_wire_bytes_still_decodes` |
| W3-10 | NoHealthyLink 单测 | `protocol/errors.rs` 内嵌 | unit | `display_contains_peer_id`、`wire_error_code_round_trip` |
| W3-11 | core 双链 e2e | `crates/mlink-core/tests/dual_link_e2e.rs` | integration | `basic_bond`、`primary_dies_seq_continuous`、`both_down_then_one_up`、`downgrade_legacy_peer`、`backpressure_at_max_pending` |
| W3-12 | daemon WS e2e | `crates/mlink-daemon/tests/dual_ws.rs` | integration | `ws_transport_list_returns_sessions_snapshot`、`ws_transport_switch_changes_active`、`ws_transport_state_fires_on_link_added`、`ws_transport_state_fires_on_switched`、`disconnect_link_dev_only` |
| W3-13 | web-debug UI 扩展 | `examples/web-debug/index.html`（+180） | UI | Transport 面板（每 peer 展示 links 表）、active 高亮、Force BLE/TCP 切换按钮、稳定性测试按钮（1000 条 ping） |
| W3-14 | 手工验收 | `docs/dual-transport-final/MANUAL-VERIFY.md` | doc | 双端联调步骤（双通道并存 / 毫秒级切换 / 不丢不重 / 带宽自适应 / UI 验证） |

---

## 巨模块预警

- `crates/mlink-daemon/src/discovery.rs` 现 318 行，W2-12 强制物理拆分（行为零改动）。
- `examples/web-debug/index.html` 现 730 行；W3-13 +180 触顶，Phase 外考虑拆 JS/CSS。
- `core/node/mod.rs` 保持现 11 行 + 子模块切分；W2-8 新增 4 个 API 要走独立 impl block。

---

## 总行数估算

**新增文件：** W1-A(160) + W1-B(140) + W1-C(180) + W1-D(170) + W2-2(200) + W2-3(180) + W2-4(160) + W2-11(130) + W2-14(170) = **1490**

**改造增量：** W1-E(+40) + W1-F(+20) + W2-5(重写≤0) + W2-6(重写≤0) + W2-7(+40) + W2-8(+40) + W2-9(+30) + W2-10(+60) + W2-12(拆分 0 行新逻辑 + Dual +50) + W2-13(+80) + W2-15(+20) + W2-16(+15) = **~400**

**合计 ≈ 1490 新增 + 400 改造 = ~1890 行**（比 B 的 2040 精简 150+；受 scheduler 内联 / 无 heartbeat 独立文件 / 无 Wi-Fi Direct 预留贡献）。

---

## 人员编排建议

- **Wave 1**：并行 6 人（W1-A..W1-F 独立文件）。合入顺序：A/B/C/E/F 先合，D 最后合（引用前四者类型）。
- **Wave 2 组 A（Session 引擎，强依赖）**：W2-2 → W2-3 ∥ W2-4 三人 1.5 天。
- **Wave 2 组 B（Node 接入）**：W2-5 / W2-6 / W2-7 / W2-8 / W2-9 / W2-10 一人串做，约 1 天。
- **Wave 2 组 C（Transport + Daemon）**：W2-11 / W2-12 并行；W2-13/14/15/16 一人串做。
- **Wave 3**：单测随 Wave 1/2 各模块交付；e2e + UI + 手工验收另分两人（产出与验证不同人）。

总工期预估 5 个工日 / 6~8 人并行。
