# 回归测试清单（arch-b）

## 原则
- 不 mock 任何涉及网络 I/O 的组件；用 `MockTransport::pair` 做 transport 层 harness（既有）。
- 所有旧用例零改动仍需绿：ConnectionManager 兼容壳 + Node 公共 API 不变保证这一点。
- 新加的用例按"功能 × 故障类型"两维覆盖。

## A 既有回归（必跑，不得挂）

### A1 mlink-core 单测
- `cargo test -p mlink-core --lib`
  - 原 `manager_add_and_count` / `manager_remove_returns_conn` / `manager_shared_returns_clone`
    必须仍绿（ConnectionManager 壳继续工作）
  - 原 `perform_handshake_*` 全部保持语义；Handshake 新 serde 字段不影响旧 payload 解析
    → 专门加 `handshake_decodes_legacy_payload_without_session_id`
  - 原 `flags_round_trip_all_combinations`、`frame_serde` 不动

### A2 mlink-daemon 集成
- `cargo test -p mlink-daemon` 全部绿
  - 重点 `ws_protocol.rs`：hello → join → send → message 全链路
  - `build_state` 通过 `MockTransport` 保持 single-link 路径等价

### A3 CLI 端到端
- `cargo test -p mlink-cli`
  - 原有连接/断开/重连 smoke 不变
  - `conn_tracker_tests.rs` 必须仍绿

### A4 旧数据兼容
- 运行旧版 daemon 写过的 `~/.mlink/daemon.json` / `rooms.json`：新版拉起不崩
- 旧版 peer 发来无 `session_id` 的 Handshake → 新版按单 link session 处理

## B Wave 1 新增单测

### B1 protocol/ack.rs
- `ack_frame_roundtrip_20_bytes`
- `unacked_ring_push_overflow_returns_oldest`
- `ack_cum_releases_range`
- `sack_bitmap_holes_not_released`
- `unacked_since_returns_sorted`

### B2 protocol/seq.rs
- `seq_gen_monotonic_wraps_cleanly`
- `dedup_window_accepts_new_sequential`
- `dedup_window_marks_duplicate_inside_window`
- `dedup_window_returns_stale_beyond_128`
- `dedup_snapshot_round_trips_into_ack_frame`

### B3 core/link.rs
- `link_send_delegates_to_connection`
- `link_recv_tracks_rtt_ema`
- `link_health_err_rate_increments_on_error`
- `link_close_drops_inner_connection`
- `link_caps_exposed_readonly`

### B4 core/session/types.rs
- `session_new_empty_links_and_no_active`
- `session_event_broadcast_delivers_to_subscriber`
- `session_id_generation_is_random_unique`

### B5 protocol/types.rs
- `handshake_serde_with_session_id`
- `handshake_serde_without_session_id_defaults_none`
- `handshake_serde_legacy_wire_bytes_still_decodes`

## C Wave 2 新增集成

### C1 Scheduler
- `scheduler_picks_higher_throughput_active`
- `scheduler_hysteresis_blocks_premature_switchback`
- `scheduler_promote_on_failure_bypasses_hysteresis`

### C2 Session send + retransmit
- `session_send_assigns_monotonic_seq`
- `session_send_fails_over_on_active_write_error` (两条 MockLink，一条返回 Err)
- `session_flush_unacked_over_new_link_preserves_order`
- `session_unacked_ring_trims_on_cum_ack`
- `session_unacked_ring_trims_on_sack_bitmap`

### C3 Session reader + dedup
- `session_reader_merges_frames_from_two_links`
- `session_reader_emits_ack_within_50ms`
- `session_reader_drops_duplicate_seq_silently`
- `session_reader_on_eof_removes_link_and_emits_event`

### C4 SessionManager
- `manager_attach_first_link_creates_new_session`
- `manager_attach_second_link_with_matching_session_id_joins`
- `manager_rejects_duplicate_kind`
- `manager_drop_closes_all_links`

### C5 Node 适配
- `node_send_raw_routes_through_session`（对已有 tests 里的 mock 路径做等价性验证）
- `node_attach_secondary_link_emits_link_added`
- `node_switch_active_link_respects_kind_request`
- `node_peer_disconnected_emits_after_last_link_dies`
  （双 link 时断一条只发 `LinkRemoved`，不发 `PeerDisconnected`）

### C6 Daemon WS 协议
- `ws_transport_list_returns_sessions_snapshot`
- `ws_transport_switch_changes_active_and_broadcasts_event`
- `ws_transport_state_fires_on_link_added`
- `ws_transport_state_fires_on_switched`

## D 手工/端到端场景（release gate）

跑在 macOS（有 BLE）+ Linux（TCP only） 两个环境。

### D1 双通道并存 — 主路径
1. A / B 两台 mac，同房间
2. 启动 daemon，`MLINK_DAEMON_TRANSPORT=dual`
3. 观察 `transport_list`：A 到 B 有 2 条 link（ble + tcp），active = tcp
4. Pass 判据：1s 内两条 link 都 handshake 成功，无 `PeerDisconnected` 事件

### D2 毫秒级故障切换
1. 已建立 D1 状态
2. 拔掉 A 的网线（或 `ifconfig en0 down`）断掉 TCP
3. 脚本 1s 内发 200 条 ping，测每条 RTT + 是否收端收到
4. Pass 判据：切换事件发生 ≤ 10ms（查 WS 事件 ts diff），收端 200 条全收
5. 插回网线：20s 内 scheduler 发回切（事件 `Switched { cause: BetterCandidate }`）

### D3 不丢不重 — BLE 断半条消息
1. D1 状态，手动设置 active=ble
2. 发送一条 100KB 大消息（会被 StreamChunk 切片 → seq 递增）
3. 发送过程中 kill 对方 BLE central（模拟硬断）
4. Session 应自动切 tcp 并 flush_unacked
5. Pass 判据：接收端恰好装出 1 条完整 100KB 消息，无重复，无缺块

### D4 带宽自适应
1. D1 状态，连接稳定后发 5MB 文件
2. 对比 active=ble 与 active=tcp 两次时长
3. Pass 判据：tcp 吞吐 ≥ 10× ble；切换期间无丢帧（D3 已覆盖正确性）

### D5 前端 debug UI
手动走读（无法自动化的 UX 路径）：
- 展示每条 link 的 kind / role / RTT / err%
- active 高亮
- "切换到 BLE" 按钮点击后 1s 内 UI 显示 ble active
- 断开 tcp 后 UI 自动更新 standby→active，1s 内
- 稳定性脚本按钮（1000 条 ping）输出统计正确

## E 性能基线（回归防劣化）

| 场景 | 旧基线 | 新目标 | 工具 |
| --- | --- | --- | --- |
| 单 link send 吞吐（TCP） | ~100MB/s | ≥ 95MB/s（差距 ≤ 5%） | `cargo bench -p mlink-core` |
| 单 link send 吞吐（BLE） | ~50KB/s | ≥ 45KB/s | 手测 |
| Session active 切换延迟 | N/A | P99 ≤ 5ms | 集成测插桩 |
| Ack RTT（TCP loopback） | N/A | P99 ≤ 2ms | 集成测插桩 |
| 内存：1 peer / 2 link / idle | N/A | ≤ 64KB | `heaptrack` |

## F 负面用例（必须优雅拒绝）

- 同一 peer 两条 BLE link：第二条 AttachOutcome::RejectedDuplicateKind
- 对端发来 session_id 但本地无匹配：拒连，返回 CreatedNew（新 session）
- 手动 `transport_switch` 到不存在的 kind：error `transport_not_available`
- 1 小时无消息：heartbeat 仍在、ack 窗口正确推进
- 恶意 peer 发送 seq = last - 200（窗口外）：DedupWindow 返回 Stale，丢弃不 ack
