# Dual-Transport REGRESSION 清单（arch-a v1）

> 拆分 + 改造后必须保持通过的既有测试 + 新增必测场景。任何一个红灯都应阻塞合入。
> "既有 A" 类不允许被修改，只允许改它们的 import path。

## A. 既有测试（不准改，只允许改 import path）

### A1. `mlink-core` 单测

`cargo test -p mlink-core --lib` 全部：

- `core::connection::tests::role_negotiation_smaller_uuid_is_central`
- `core::connection::tests::role_negotiation_is_deterministic_pairwise`
- `core::connection::tests::role_negotiation_handles_equal_uuids`
- `core::connection::tests::perform_handshake_times_out_on_silent_peer`
- `core::connection::tests::manager_add_and_count`
- `core::connection::tests::manager_remove_returns_conn`
- `core::connection::tests::manager_shared_returns_clone`
- `core::connection::tests::perform_handshake_round_trip`
- `core::connection::tests::perform_handshake_rejects_non_handshake_reply`
- `core::connection::tests::perform_handshake_with_resume_streams`
- `protocol::frame::tests::*`（7 个 frame 编解码测试）
- `protocol::types::tests::*`（全部 serde / priority / flags）
- `core::node::tests::*`（959 行的全部既有测试）

> **关键点：** `manager_shared_returns_clone` 必须保留 —— 它守门 `shared(id)` 语义；W2-1 改造后 `shared == primary_conn`，测试应继续过。

### A2. `mlink-daemon` 集成测试

`cargo test -p mlink-daemon --test ws_protocol` 全绿。

### A3. CLI 测试

`cargo test -p mlink-cli` 全绿（`conn_tracker_tests.rs` / `role_and_channels_tests.rs`）。

## B. Wave 1 新增单测（每模块）

### B1. `core::link::tests`

- `record_rx_accumulates_bytes`
- `set_health_visible_to_snapshot`
- `record_heartbeat_rx_updates_timestamp`
- `metrics_read_while_write_ok`（多线程读写不 panic）

### B2. `core::link::pool::tests`

- `empty_pool_has_no_primary`
- `add_promotes_first_link_to_primary`
- `add_same_transport_replaces_old_link` (Replaced)
- `add_different_transport_goes_standby` (Added)
- `cap_enforced_fourth_link_rejected` (Rejected)
- `remove_primary_auto_promotes_lex_smallest_transport`
- `promote_target_not_healthy_returns_nohealthy`
- `failover_away_from_dead_link`

### B3. `core::retransmit::tests`

- `push_until_full_then_rejects`
- `ack_through_clears_cumulative`
- `ack_through_wrap_boundary_65534_to_1`
- `due_for_retry_respects_interval`
- `mark_resent_updates_last_sent`
- `seq_le_wrap_semantics`（≥ 10 个 case）

### B4. `core::health::tests`

- `starts_in_probing`
- `three_consecutive_ok_transitions_to_healthy`
- `silence_15s_degraded`
- `silence_30s_dead`
- `dead_is_absorbing` (evaluate 后续总 Unchanged)
- `hysteresis_requires_3_probes_to_restore`

### B5. `core::selector::tests`

- `no_healthy_returns_none`
- `bandwidth_first_picks_tcp_over_ble`
- `tie_break_by_transport_id_lex`
- `explicit_selector_follows_priority_list`

### B6. `protocol::capabilities::tests`

- `negotiate_and_truth_table`
- `handshake_serde_backward_compatible_without_capabilities_field`
- `handshake_serde_with_capabilities_round_trip`

### B7. `protocol::types::ack_payload_tests`

- `ack_payload_round_trip`
- `ack_payload_bitmap_default_empty`

### B8. `errors::no_healthy_link_tests`

- `display_contains_peer_id`
- `wire_error_code_round_trip`

## C. Wave 2 集成测试

### C1. `crates/mlink-core/tests/dual_link_e2e.rs`（W3-5）

1. **basic_bond** ：两个 MockTransport pair，A 先建 BLE link，再建 TCP link；`LinkPool::len() == 2`；primary 最终为 TCP。
2. **primary_dies_seq_continuous** ：10 帧发到 A；第 5 帧发出后断 TCP；第 6..10 帧必须通过 BLE 无丢无重；对端观察到的 seq 为 0..9 连续。
3. **both_down_then_one_up** ：两条都断；再起 TCP；重传队列里未 ack 的帧通过新 TCP 重发一次（resent_count == 1）。
4. **downgrade_legacy_peer** ：对端 Handshake 不带 capabilities；本端走单通道路径（第二条连接被拒，复用既有 DEDUP）。
5. **backpressure_at_max_pending** ：把 `MAX_PENDING` 调小到 4；不 ack 地发 5 帧，最后一次返回 `MlinkError::Backpressure`。

### C2. `crates/mlink-daemon/tests/dual_ws.rs`（W3-6）

使用 in-process daemon (`build_state`) + 两个 MockTransport pair 联通 2 个 Node：

1. **ws_sees_link_added** ：WS 客户端 hello 后，建立第 2 条 link，收到 `link_added` 帧。
2. **ws_sees_primary_changed** ：断 primary，收到 `primary_changed{old, new, reason="link_down"}`。
3. **list_links_returns_live_state** ：`list_links` 返回所有 peer/link 数据。
4. **set_preferred_transport_forces_switch** ：调 `set_preferred_transport{peer, "ble"}` 后 primary 切换至 BLE。
5. **legacy_client_ignores_new_types** ：不订阅 link_* 的 session 不收新帧；不报错。
6. **disconnect_link_dev_only** ：非 dev 模式 `disconnect_link` 返回 `bad_type`；`MLINK_DAEMON_DEV=1` 时生效。

### C3. 降级与兼容性

1. **single_mode_unchanged** ：`MLINK_DAEMON_TRANSPORT=tcp`（默认）下的行为与 baseline 完全一致（既有 WS 协议测试 A2 全绿）。
2. **ble_mode_unchanged** ：同上对 BLE。
3. **dual_mode_old_peer_no_ack** ：本端 dual 模式，对端不发 Ack（模拟旧客户端）—— 经过 10 秒 heartbeat，send_raw 不阻塞不挤爆 RetransmitQueue（靠自动 ack_through(seq) 兜底，参考 INTERFACE-CONTRACTS §9 规则）。

## D. 性能 / 稳定性眼睛

不设失败门禁，跑下来记在 MANUAL-VERIFY.md：

- **切换延迟** ：primary Dead → 下一条成功 write 的时间 ≤ 200ms（目标毫秒级；网络抖动允许 1 次 retry）。
- **重传吞吐** ：1 MB 消息 TCP primary 切 BLE standby 后，带宽自然降到 BLE 上限不卡死。
- **无消息丢失** ：压测 10k 消息 × 5 次随机断链，对端接收次序 + 数量与发送方完全一致。
- **前端稳定** ：web-debug 挂 30 分钟，`primary_changed` 事件流不丢、不越界、UI 不卡。

## E. 运行命令速查

```sh
# Wave 1
cargo test -p mlink-core --lib

# Wave 2 全量
cargo test -p mlink-core
cargo test -p mlink-daemon

# 集成
cargo test -p mlink-core --test dual_link_e2e
cargo test -p mlink-daemon --test dual_ws

# 全量 CI
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```
