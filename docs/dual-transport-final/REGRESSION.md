# Dual-Transport REGRESSION 清单（最终版）

> 拆分 + 改造后必须通过的回归 + 新增测试。任一红灯阻塞合入。
> A 类（既有）不准改内容，只允许改 import path（导入路径迁移）。

---

## A. 既有回归（459 测试零退化）

### A1. `mlink-core` 单测
`cargo test -p mlink-core --lib` 全绿。重点守门：

- **`core::connection::tests`**
  - `role_negotiation_smaller_uuid_is_central`
  - `role_negotiation_is_deterministic_pairwise`
  - `role_negotiation_handles_equal_uuids`
  - `perform_handshake_times_out_on_silent_peer`
  - `perform_handshake_round_trip`
  - `perform_handshake_rejects_non_handshake_reply`
  - `perform_handshake_with_resume_streams`
  - `manager_add_and_count` ← 守门 ConnectionManager 兼容壳
  - `manager_remove_returns_conn` ← 守门
  - `manager_shared_returns_clone` ← 守门（`shared(id)` 语义保持 = 返回 active link 的 Arc）
- **`protocol::frame::tests::*`**（7 个 frame 编解码）
- **`protocol::types::tests::*`**（所有 serde / priority / flags；Handshake 加字段后 serde 测试仍绿）
- **`core::node::tests::*`**（959 行全部）

**关键点：** `core/node/tests.rs` 里的测试使用 `attach_connection` test hook 构造 mock 连接。W2-7 改造时 `attach_connection` 必须继续返回 `peer_id` 并让 `shared / send_raw / recv_raw` 行为不变，才能守住 959 行测试零改动。

### A2. `mlink-daemon` 集成测试
`cargo test -p mlink-daemon --test ws_protocol` 全绿。
- 重点 `hello → join → send → message` 全链路在 single-link Session 路径下等价。
- `MockTransport` pair 构造 + `build_state` 拉起 → WS 连上 → 发 message → peer 收 message 不变。

### A3. `mlink-cli` 测试
`cargo test -p mlink-cli` 全绿：
- `conn_tracker_tests.rs`
- `role_and_channels_tests.rs`

### A4. 旧数据兼容
- 旧 daemon 写的 `~/.mlink/daemon.json` / `rooms.json` 新 daemon 拉起不崩。
- 旧 peer 发来无 `session_id` 的 Handshake → 新端按单 link Session 处理（不重试第二条）。
- 旧 peer 不发 Ack → 本端 single-link 路径下直接 `ack_through(seq)` 兜底（避免 UnackedRing 爆）。

---

## B. Wave 1 新增单测（每模块内嵌 `#[cfg(test)] mod tests`）

### B1 · `protocol::ack::tests`（W3-1）
- `ack_frame_roundtrip_20_bytes`
- `unacked_ring_push_overflow_returns_oldest`
- `ack_cum_releases_range`
- `sack_bitmap_holes_not_released`
- `unacked_since_returns_sorted`

### B2 · `protocol::seq::tests`（W3-2）
- `seq_gen_monotonic_wraps_cleanly`（递增至 u32::MAX → 回 0）
- `dedup_window_accepts_new_sequential`
- `dedup_window_marks_duplicate_inside_window`
- `dedup_window_returns_stale_beyond_128`
- `dedup_snapshot_round_trips_into_ack_frame`

### B3 · `core::link::tests`（W3-3）
- `link_send_delegates_to_connection`
- `link_recv_tracks_rtt_ema`
- `link_health_err_rate_increments_on_error`
- `link_close_drops_inner_connection`
- `link_caps_exposed_readonly`
- `transport_kind_wire_round_trip`

### B4 · `core::session::types::tests`（W3-4）
- `session_new_empty_links_and_no_active`
- `session_event_broadcast_delivers_to_subscriber`
- `session_id_generation_is_random_unique`

### B5 · `protocol::types::tests`（W3-9）
- `handshake_serde_with_session_id`
- `handshake_serde_without_session_id_defaults_none`
- `handshake_serde_legacy_wire_bytes_still_decodes`（用真实 msgpack dump 对比）

### B6 · `protocol::errors::tests`（W3-10）
- `no_healthy_link_display_contains_peer_id`
- `no_healthy_link_wire_error_code_round_trip`（0x08）

---

## C. Wave 2 新增集成单测

### C1 · scheduler 单测（W3-5，合入 `core/session/io.rs` 内嵌）
- `scheduler_picks_higher_throughput_active`（TCP caps vs BLE caps）
- `scheduler_hysteresis_blocks_premature_switchback`（2 拍更优不切，3 拍切）
- `scheduler_promote_on_failure_bypasses_hysteresis`

### C2 · session/io 单测（W3-6，合入 `core/session/io.rs` 内嵌）
- `session_send_assigns_monotonic_seq`
- `session_send_fails_over_on_active_write_error`（两条 MockLink，一条返回 Err）
- `session_flush_unacked_over_new_link_preserves_order`
- `session_unacked_ring_trims_on_cum_ack`
- `session_unacked_ring_trims_on_sack_bitmap`

### C3 · session/reader 单测（W3-7，合入 `core/session/reader.rs` 内嵌）
- `session_reader_merges_frames_from_two_links`
- `session_reader_emits_ack_within_50ms`（用 `tokio::time::pause` 驱动）
- `session_reader_drops_duplicate_seq_silently`
- `session_reader_on_eof_removes_link_and_emits_event`

### C4 · SessionManager 单测（W3-8，合入 `core/session/manager.rs` 内嵌）
- `manager_attach_first_link_creates_new_session`
- `manager_attach_second_link_with_matching_session_id_joins`
- `manager_rejects_duplicate_kind`
- `manager_rejects_capacity_over_four`
- `manager_drop_closes_all_links`

### C5 · Node 适配回归（扩展 `core/node/tests.rs`）
- `node_send_raw_routes_through_session`（mock 路径等价性）
- `node_attach_secondary_link_emits_link_added`
- `node_switch_active_link_respects_kind_request`
- `node_disconnect_one_link_keeps_peer_alive`（双 link 时断一条只发 `LinkRemoved`，无 `PeerDisconnected`）
- `node_disconnect_last_link_emits_peer_disconnected`

### C6 · Daemon WS 新协议（W3-12，`tests/dual_ws.rs`）
- `ws_transport_list_returns_sessions_snapshot`
- `ws_transport_switch_changes_active_and_broadcasts_event`
- `ws_transport_state_fires_on_link_added`
- `ws_transport_state_fires_on_switched`
- `legacy_client_ignores_new_types`（不订阅新 type 的 WS 不报错）
- `disconnect_link_dev_only`（`MLINK_DAEMON_DEV=1` 时生效，其余返回 `bad_type`）

---

## D. core 集成 e2e（W3-11，`tests/dual_link_e2e.rs`）

1. `basic_bond`：MockTransport 两 pair 模拟 BLE + TCP；两条都接上后 `peer_link_status().len() == 2`，active 为 TCP（throughput 更高）。
2. `primary_dies_seq_continuous`：
   - 发 10 条消息 → 第 5 条后 `drop` TCP MockConnection
   - 断时所有 unacked 通过 BLE 重发
   - 对端 DedupWindow 接收 seq 连续 0..9，无重无漏
3. `both_down_then_one_up`：
   - 双 link 都断 → UnackedRing 保留
   - 重新 attach 一条 TCP link → flush_unacked 重发
   - 对端拿到的帧 `resent_count == 1`（或在测试里放记录位）
4. `downgrade_legacy_peer`：
   - 构造对端 Handshake `session_id=None`（模拟旧客户端）
   - 本端第二次 attach 同 peer 返回 `RejectedDuplicateKind`
5. `backpressure_at_max_pending`：
   - 调小 `UnackedRing` cap 到 4
   - 不 ack 地连发 5 条 → 第 5 条 push 返回 Some（丢最旧）+ 日志 warn
   - 测试用 log capture 断言 warn 出现

---

## E. 手工验收（W3-14，`MANUAL-VERIFY.md`）

### D1 双通道并存 — 主路径
1. 两台 Mac（A/B）同一房间。
2. 启动 daemon：`MLINK_DAEMON_TRANSPORT=dual`。
3. web-debug 查看 `transport_list`：A 到 B 应有 2 条 link，active=tcp。
4. **判据：** 1s 内两条 link handshake 成功，无 `PeerDisconnected` 事件。

### D2 毫秒级故障切换
1. D1 状态达成。
2. 脚本 1s 内发 200 条 ping。
3. 中途 `ifconfig en0 down` 断 TCP。
4. **判据：** `Switched { cause: ActiveFailed }` WS 事件 → 下一条 ping 发出的时间差 ≤ 10ms（由 WS event ts 对比）；收端 200 条全收。
5. 恢复网络 → 20s 内触发 `Switched { cause: BetterCandidate }` 回切。

### D3 不丢不重 — BLE 断大消息中段
1. 手动 `Force BLE active`（web-debug 按钮）。
2. 发 100KB 消息（会触发 `StreamStart/StreamChunk*N/StreamEnd` 多帧）。
3. 中途 kill 对方 BLE central（模拟硬断）。
4. Session 自动切 tcp 并 flush_unacked。
5. **判据：** 接收端恰好装出 1 条完整 100KB 消息，`md5 = 发送端 md5`，无重复无缺块。

### D4 带宽自适应
1. D1 状态稳定。
2. 分别 active=ble 与 active=tcp 发 5MB 文件，记录时长。
3. **判据：** tcp 吞吐 ≥ 10× ble；切换期间无丢帧（D3 已覆盖正确性）。

### D5 web-debug UI
- Transport 面板显示每条 link 的 kind / role / RTT / err%。
- Active link 高亮色。
- `Force BLE` / `Force TCP` 按钮 → 1s 内 UI 反映切换，覆盖倒计时显示。
- `稳定性测试 1000 ping` 按钮 → 输出统计：切换次数 / 丢包=0 / 重复=0。
- 断网后 UI 自动更新 standby→active，≤ 1s。

---

## F. 性能基线（不设失败门禁但必测记录）

| 场景 | 目标 | 采集工具 |
| --- | --- | --- |
| 单 link send 吞吐（TCP loopback） | ≥ 95% of baseline（基线跑一次 HEAD） | `cargo bench -p mlink-core`（现有 benches 或新加） |
| 单 link send 吞吐（BLE） | ≥ 45KB/s（硬件限制） | 手测 |
| Session active 切换延迟 | P99 ≤ 5ms | 集成测插桩：记 `Switched` 事件 ts - 首条新 send 完成 ts |
| Ack RTT（TCP loopback） | P99 ≤ 2ms | 集成测插桩 |
| 1 peer / 2 link / idle 驻留内存 | ≤ 64KB | `heaptrack` 或 `cargo instruments` |

---

## G. 负面用例（必须优雅拒绝，不 panic）

- 同一 peer 两条同 kind link（例两条 BLE）→ `AttachOutcome::RejectedDuplicateKind` + caller 关连接。
- 超过 `MAX_LINKS_PER_PEER=4` → `RejectedCapacity` + 关连接。
- 对端发来 `session_id` 但本地 SessionManager 无匹配 → 按 `CreatedNew` 新建（日志 info：session_id mismatch, starting fresh）。
- 手动 `transport_switch` 到不存在的 kind → `MlinkError::HandlerError("transport_not_available")`。
- 1 小时无消息：heartbeat 仍推进；ack 窗口不在 idle 时爆炸（无未 ack 项就无 ack 发）。
- 恶意 peer 发 `seq = last - 200`（窗外）→ DedupWindow::Stale → 帧丢弃，不计入 ack bitmap。
- `disconnect_link` 非 dev 环境下请求 → 返回 `bad_type`，不真断。

---

## H. 运行命令速查

```sh
# Wave 1
cargo test -p mlink-core --lib protocol::ack
cargo test -p mlink-core --lib protocol::seq
cargo test -p mlink-core --lib core::link
cargo test -p mlink-core --lib core::session::types
cargo test -p mlink-core --lib protocol::types
cargo test -p mlink-core --lib protocol::errors

# Wave 2 单测
cargo test -p mlink-core --lib core::session
cargo test -p mlink-core --lib core::node

# Wave 2 集成
cargo test -p mlink-core --test dual_link_e2e
cargo test -p mlink-daemon --test dual_ws

# 全量回归
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check

# 性能基线（非门禁）
cargo bench -p mlink-core
```

---

## I. 每 PR 检查清单

- [ ] A1/A2/A3 既有 459 测试全绿（本 PR 未触碰的测试不得失败）
- [ ] 本 task 对应的 B/C/D 新增测试已加（Wave 1 单测 ≥ 5 条/模块；Wave 2 集成按清单）
- [ ] `wc -l <file>` ≤ 限定行数
- [ ] clippy 零新 warning
- [ ] 非测试路径无 `unwrap()`
- [ ] `git grep -n "await" <file>` 人工检查：所有 await 之前必须已经 drop 锁 / clone Arc
