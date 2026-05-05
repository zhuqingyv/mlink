# audit-core — mlink-core 测试覆盖审计

审计对象: `crates/mlink-core/src/core/node.rs` (1485 行，生产代码 ~755 行 + 测试 ~680 行)
拆分计划: `docs/refactor/core-node-split.md` — 6 个生产文件 + 1 个 tests.rs
基线结果: **全绿**

## 1. 测试基线

| Suite | 通过 | 失败 | 忽略 |
|---|---|---|---|
| lib (含 `core::node::tests`) | 220 | 0 | 0 |
| phase5_tests.rs | 10 | 0 | 2 |
| phase6_tests.rs | 12 | 0 | 0 |
| phase8_tests.rs | 11 | 0 | 0 |
| phase9_tests.rs | 22 | 0 | 0 |
| tcp_tests.rs | 10 | 0 | 0 |
| **总计** | **285** | **0** | **2** |

`core::node::tests` 模块单独过滤后 22 个全过。基线可作拆分对照。

## 2. node.rs 方法 × 测试覆盖矩阵

下表把拆分计划的 6 个目标文件作为列，给每个方法标出对应 node.rs 内测试 + 外部集成测试。

### 2.1 `node/mod.rs`（构造、生命周期、简短访问器）

| 方法 | 行号 | 直接测试 | 间接（集成）覆盖 | 评估 |
|---|---|---|---|---|
| `new` | 106–126 | `new_starts_idle` ✅ | 几乎所有测试都会 `Node::new` | **充分** |
| `state` | 156 | `new_starts_idle`, `start_transitions_to_discovering`, `stop_returns_to_idle`, `attach_connection_enables_send_recv_without_handshake` | — | **充分** |
| `app_uuid` | 160 | `new_starts_idle` 断言非空；多处 `node.app_uuid()` 作对比 | — | **充分** |
| `config` | 164 | 无直接；`config()` 被上层 daemon 使用 | — | **薄但零风险**（getter） |
| `subscribe` | 168 | `spawn_peer_reader_emits_message_received_event`, `reader_eof_cleans_up_...` 都先 subscribe | — | **充分** |
| `start` | 172 | `start_transitions_to_discovering`, `start_rejects_double_start`, `stop_returns_to_idle` | phase5 `test_node_stop`, `test_node_start_discovering` | **充分** |
| `stop` | 184 | `stop_returns_to_idle` | phase5 `test_node_stop` | **充分** |
| `role_for` | 725 | `role_for_is_deterministic` ✅ | phase9 `test_node_attach_and_send_raw` 间接 | **充分** |
| `connection_count` | 743 | `accept_incoming_matching_room_registers_peer`, `accept_incoming_mismatched_room_rejects`, `attach_connection_enables_send_recv_without_handshake`, `has_peer_tracks_attach_and_disconnect`, `reader_eof_...` | — | **充分** |
| `trust_store` | 748 | 无直接；返回 `Arc<RwLock<_>>` 是接口兼容测试 | `security.rs` 单测覆盖 TrustStore 本身 | **getter，零风险** |

### 2.2 `node/types.rs`（类型）

| 项 | 直接测试 | 评估 |
|---|---|---|
| `NodeState` | `node_state_is_copy` ✅ | **充分**（枚举，Copy 语义测过） |
| `NodeConfig` + `Default` | `node_config_default_encrypt_false` ✅, `test_config_helper_used` | phase5 `test_node_config` | **充分** |
| `PeerState::new` | 通过 `attach_connection`, `send_raw` 等覆盖（每次 `or_insert_with(PeerState::new)`）| **充分** |
| `PeerState::next_seq` | `send_raw_not_blocked_by_parked_reader`（assert writes==1，隐式走 seq），`spawn_peer_reader_emits_...`（对端 send_raw 两次） | phase9 `test_node_encrypted_send_recv`（两次 send） | **充分但隐式** — 纯单测缺 seq 递增断言 |
| `NodeEvent` | `spawn_peer_reader_emits_message_received_event`, `reader_eof_...` | phase9, phase6 | **充分** |
| 心跳常量 | `check_heartbeat_missing_peer_fails` 间接；`HEARTBEAT_TIMEOUT` 在 `check_heartbeat` 内使用 | — | **弱** — 常量值本身无单测 |

### 2.3 `node/rooms.rs`（房间成员关系）

| 方法 | 直接测试 | 评估 |
|---|---|---|
| `add_room_hash` | `connect_peer_matching_room_roundtrip_is_accepted`, `accept_incoming_matching_room_registers_peer`, `node_in_multiple_rooms_...`, `node_rejects_peer_whose_room_is_not_in_set` | **充分** |
| `remove_room_hash` | `connect_peer_matching_room_roundtrip_is_accepted` | **充分** |
| `room_hashes` | `connect_peer_matching_room_roundtrip_is_accepted`（空→1→空），`node_in_multiple_rooms_...`（len==2） | **充分** |
| `wire_room_hash` (priv) | 通过 `connect_peer`/`accept_incoming` 路径间接（`local_hs.room_hash = self.wire_room_hash()`）| **充分** — 多房间场景里两测都选到其中一个并得到匹配 |

**关键并发点**: `room_hashes` 是 `StdMutex<HashSet>`，**guard 不能跨 .await**。`connect_peer` / `accept_incoming` 里的 snapshot-then-drop 模式已由 `accept_incoming_matching_room_registers_peer` 等测试覆盖（因为它们驱动出真正的 `tokio::spawn` + `.await` 路径），**如果拆分后 `wire_room_hash` 或新 helper 意外让 guard 跨 await，`tokio::spawn` 时会立即编译失败**（`StdMutexGuard: !Send`）。此为**编译器级保护**，无需新增测试。

### 2.4 `node/connect.rs`（握手）— **拆分计划最危险文件**

| 方法 / 场景 | 直接测试 | 覆盖判定 |
|---|---|---|
| `connect_peer` 失败路径 | `connect_peer_via_mock_transport_errors_cleanly` ✅ | 弱：只覆盖 transport.connect 失败；未覆盖 handshake 失败 |
| `connect_peer` 成功路径（房间匹配） | **无直接测试** ⚠️ | **缺口** — 仅 `accept_incoming` 侧有 E2E |
| `connect_peer` 房间不匹配拒绝 | **无直接测试** ⚠️ | **缺口** |
| `connect_peer` 去重（dedup） | **无直接测试** ⚠️ | **缺口** — 只有 `accept_incoming_is_idempotent_when_already_connected` 覆盖入站去重 |
| `accept_incoming` 成功 + 房间匹配 | `accept_incoming_matching_room_registers_peer` ✅, `node_in_multiple_rooms_accepts_peers_from_any_of_them` ✅ | **强** |
| `accept_incoming` 房间不匹配 | `accept_incoming_mismatched_room_rejects` ✅, `node_rejects_peer_whose_room_is_not_in_set` ✅ | **强** |
| `accept_incoming` 去重 | `accept_incoming_is_idempotent_when_already_connected` ✅ | **强** |
| `accept_incoming` 握手失败 | 仅通过 `MockTransport` 的 handshake 路径覆盖；无专测 | 弱 |
| **helper 提取** (`register_after_handshake`) | **无专门测试** ⚠️ | 由两个被调方法的成功/失败/去重/房间测试**矩阵交叉**保护 |

**拆分计划 Step 6 的 helper 提取风险**: 计划说两个 ~200 行姊妹方法有 ~130 行重复的 "snapshot room → check → dedup → register" 逻辑，提取到 `register_after_handshake`。**accept_incoming 侧有 4 个专测覆盖这段逻辑（match / mismatch / dedup / multi-room），但 connect_peer 的这段逻辑完全没有直接测试** —— 如果 helper 在重构中改错了返回值/所有权/日志，accept 路径的测试不会报警。

### 2.5 `node/io.rs`（帧读写 + 心跳）

| 方法 | 直接测试 | 评估 |
|---|---|---|
| `send_raw` 成功 | `attach_connection_enables_send_recv_without_handshake` ✅, `spawn_peer_reader_emits_...`（对端 send_raw） | phase9 × 6 测用 send_raw，phase6 `test_broadcast`, `test_send_message` | **充分** |
| `send_raw` PeerGone | `send_raw_on_missing_peer_fails` ✅, phase5 `test_send_raw_missing_peer`, phase9 `test_node_send_raw_missing_peer` | **充分** |
| `send_raw` 加密 | — 在 node.rs 里无专测 | phase9 `test_node_encrypted_send_recv` | **通过外部充分** |
| `send_raw` 压缩 | 通过 payload 大小触发 `should_compress` | phase9 `test_e2e_large_payload_roundtrip` | **充分** |
| **`send_raw` 并发锁死 (核心回归)** | `send_raw_not_blocked_by_parked_reader` ✅ | **强 — 专测守护** |
| `recv_raw` 成功 | phase9 `test_node_recv_raw_over_mock`, `test_node_bidirectional_attached` | **充分** |
| `recv_raw` PeerGone | phase9 内隐式 | 弱直接，但接口简单 |
| `spawn_peer_reader` 消息事件 | `spawn_peer_reader_emits_message_received_event` ✅ | **充分** |
| **`spawn_peer_reader` EOF 清理** | `reader_eof_cleans_up_connection_and_peer_manager` ✅（断言 4 个不变量：无连接/无 peer/无状态 Disconnected/reader 自己退出） | **强** |
| `recv_once` (priv fn) | 通过 `spawn_peer_reader` 两测覆盖 + `recv_raw` 镜像结构 | **充分** |
| `send_heartbeat` | phase9 `test_node_heartbeat_roundtrip` | **充分** |
| `check_heartbeat` | `check_heartbeat_missing_peer_fails` ✅ + phase9 `test_node_heartbeat_roundtrip` | **充分** |

### 2.6 `node/lifecycle.rs`（peer 生命周期）

| 方法 | 直接测试 | 评估 |
|---|---|---|
| `peers` | 间接通过 `peer_manager.list()` + phase 测试 | **充分** |
| `get_peer` | `reader_eof_...`（断言 None） | **充分** |
| `peer_state` | `attach_connection_enables_send_recv_without_handshake`, `reader_eof_...` | **充分** |
| `has_peer` | `has_peer_tracks_attach_and_disconnect` ✅, `reader_eof_...`, dedup 测 | **强** |
| `attach_connection` | `attach_connection_enables_send_recv_without_handshake` ✅ + 多个使用它的 E2E | **充分** |
| `disconnect_peer` | `has_peer_tracks_attach_and_disconnect` ✅ | phase9 `test_node_disconnect_removes_connection` | **充分** |
| `set_peer_aes_key` | phase9 `test_node_encrypted_send_recv` | **充分** |
| `set_peer_state` (priv) | 通过 `connect_peer`, `mark_reconnecting` 间接 | **充分** |
| `mark_lost` | — | **零直接测试**，但只是一行 `events_tx.send`，拆分零风险 |
| `mark_reconnecting` | — | **零直接测试**；会调 `set_peer_state` + 事件 — 如果拆分出错事件缺失 | **弱** |

## 3. 关键风险点与专测守护矩阵（对照拆分计划 Section "风险点"）

| 风险 | 计划里的具体断言 | 守护测试 | 判定 |
|---|---|---|---|
| `connections.lock()` 跨 `.await` 持有 → 复发 BLE 死锁 | 代码 L577–L586 / L656–L661 注释 + Arc clone 模式 | `send_raw_not_blocked_by_parked_reader` **专测+断言 write 计数** | **强守护** |
| `room_hashes` 的 StdMutex guard 不跨 `.await` | 代码 L279–L291, L406–L418 注释 | **无专测**，但 `tokio::spawn` 的 `Send` bound 是**编译期硬保护**；`accept_incoming` 测套会实际 `.await` 这段代码 | **编译器保护充分，运行期保护中等** |
| `PeerState.seq` 单调递增 | L531–L561 | **无专测 assert seq 递增** | **弱** — 但 `next_seq` 极简 (2 行)，拆分零风险 |
| helper 提取后 `Box<dyn Connection>` 所有权只能 move 一次 | 计划 L222 + 代码 L269, L401 失败路径 close | accept 侧三个 error 测（`...mismatched_room`, 未有 handshake 失败测；所有权编译期保证） | **中等** |
| helper 提取后日志 site 信息保留（site: &str） | 计划 L350 | **无测试** — 纯运维诊断，不影响行为 | N/A |
| `HOME_LOCK` 不拆分测试文件 | 计划 L378–L379 | 拆分计划里**测试保持单文件** (`tests.rs` ~680 行) | **由计划保证** |

## 4. 缺口清单与补测建议（按重要性降序）

### 必须补（拆分前）

1. **`connect_peer` 握手 **成功** 路径的专测** ⚠️
   - 现状：`connect_peer_via_mock_transport_errors_cleanly` 只覆盖 `transport.connect` 失败；真正执行到 `perform_handshake` → 房间校验 → dedup → 注册 的**正路径无专测**。
   - 风险：Step 6 的 helper 提取，`connect_peer` 侧的"snapshot room → check → dedup → register"完全靠 `accept_incoming` 镜像测试间接守护。如果 helper 改错了 `connect_peer` 专有的 `transport.mtu()` / `Peer::name` / `transport.id()` 等字段填装，测试不会报错。
   - 建议补：`connect_peer_matching_room_happy_path` — 用 `MockTransport` + `mock_pair` 模拟对端完成 `perform_handshake`，断言 `connection_count == 1 && peer_manager.get(peer_id).name == discovered.name`。

2. **`connect_peer` dedup 专测** ⚠️
   - 现状：`accept_incoming` 侧有 `accept_incoming_is_idempotent_when_already_connected`，`connect_peer` 侧**无镜像**。
   - 建议补：`connect_peer_dedup_when_already_connected` — 先 `attach_connection`，再 driver 完成 outbound 握手，断言 `connection_count == 1 && 返回 existing_peer_id`。

3. **`connect_peer` 房间拒绝专测** ⚠️
   - 补 `connect_peer_rejects_mismatched_room` — 对称于 `accept_incoming_mismatched_room_rejects`。

### 建议补（不阻塞拆分）

4. `mark_reconnecting` 专测 — 断言事件 `Reconnecting { attempt }` + PeerState 翻转。
5. `PeerState.next_seq` 递增专测 — 断言 `send_raw` 两次后 frame.seq 为 0, 1。
6. `HEARTBEAT_TIMEOUT` 常量语义测 — `check_heartbeat` 在 `last_heartbeat + 46s` 场景下返回 false（需注入时间或断言常量）。

## 5. 安全性评估：node.rs 的测试能否支撑安全拆分？

| Step | 内容 | 守护强度 | 判定 |
|---|---|---|---|
| 0 | 基线全绿 | 285/285 | ✅ |
| 1 | `node.rs` → `node/mod.rs`（仅路径移动） | 全测试 | ✅ **安全** |
| 2 | 抽 `types.rs` | types 类全覆盖 + 其它测试依赖它 | ✅ **安全** |
| 3 | 抽 `rooms.rs` | 4 个房间测试（含 multi-room + mismatch）| ✅ **安全** |
| 4 | 抽 `lifecycle.rs` | `has_peer_tracks_attach_and_disconnect`, `attach_connection_...`, phase9 的 disconnect 测 | ✅ **安全**（`mark_lost` / `mark_reconnecting` 是纯事件送信，拆分零风险） |
| 5 | 抽 `io.rs` | **`send_raw_not_blocked_by_parked_reader` + `reader_eof_...` + `spawn_peer_reader_emits_...` 三个专测**；phase9 大量覆盖 | ✅ **安全 —— 最关键的并发不变量有专测** |
| 6 | 抽 `connect.rs` + `register_after_handshake` helper | **accept 侧 4 个专测强守护；connect 侧仅有一个错误路径测，正路径/dedup/房间拒绝无专测** | ⚠️ **有缺口** |
| 7 | 移动 tests → `node/tests.rs` | 编译通过即可 | ✅ **安全** |

## 6. 结论

- **基线质量**: 285 测试全绿，node.rs 内 22 个专测覆盖率良好，并发不变量（send/reader 锁粒度、reader EOF 清理）有**专测强守护**。
- **可安全拆分的 step**: 0–5 + 7，**总计 6/7 步安全**。
- **唯一风险点**: **Step 6（connect.rs）的 `connect_peer` 成功路径 / dedup / 房间拒绝三个场景无专测**，完全依赖 `accept_incoming` 对称实现的间接保护 + 类型系统。
- **强烈建议**: 在执行 Step 6 之前，先补 `connect_peer_matching_room_happy_path` / `connect_peer_dedup_when_already_connected` / `connect_peer_rejects_mismatched_room` 这 3 个镜像测试。三个都可直接复制对应 `accept_incoming_*` 测试的结构，把 `node_a.accept_incoming(Box::new(ca), ...)` 改为 `node_a.connect_peer(&mut transport, &discovered).await`，配合 `MockTransport::with_preconnected(cb)` 即可（需要看 MockTransport 是否支持；若不支持可复用 `attach_connection` + 手工 handshake 驱动 + Node 内部的 `perform_handshake` 替代路径）。
- **不阻塞拆分但应近期补**: `next_seq` 递增、`mark_reconnecting` 事件、心跳超时常量测。
- **其它合计 285 个 suite 为边界网**：任意拆分步骤意外改动 Node 的公开行为（`send_raw` / `recv_raw` / 心跳 / attach / 事件）都会被 phase5/phase6/phase8/phase9 触发。

**总体判定**: **可安全拆分 Step 0–5 + 7；Step 6 必须先补 3 个 connect_peer 镜像测试再执行**。拆分过程中每 Step 跑 `cargo test -p mlink-core` 作为 gate，特别关注 `send_raw_not_blocked_by_parked_reader` / `reader_eof_...` / `accept_incoming_*` 四组测试。
