# Dual-Transport 模块清单（arch-a v1）

> 用户目标：BLE + TCP 同时连同一 peer、断一补一毫秒级无感、seq+重传零丢失零重复、可加速可降速、前端 debug UI 可视化/可切换。
>
> 策略：**Bonding 模式**（源自 mnemo id:402）。物理多 link，逻辑单 peer；对 Node 上层透明，对 WS / UI 显式。
>
> 每个模块文件 ≤200 行。超限必须再拆。下文 "行" 数为估算。

---

## Wave 1 —— 非业务模块（并行开工，只 `use` §INTERFACE-CONTRACTS 签名）

| # | 模块 | 文件路径 | 职责（一句话） | 行数 | 依赖 |
| --- | --- | --- | --- | --- | --- |
| W1-A | `core::link` | `crates/mlink-core/src/core/link/mod.rs` | 单条 Link 的状态 + 指标容器（不碰 socket） | ≤120 | transport trait |
| W1-B | `core::link::pool` | `crates/mlink-core/src/core/link/pool.rs` | 同 peer 的多 Link 管理 + primary 指针 | ≤180 | W1-A |
| W1-C | `core::retransmit` | `crates/mlink-core/src/core/retransmit.rs` | u16-seq 滑动窗口 + 累积 Ack 清理 | ≤180 | — |
| W1-D | `core::health` | `crates/mlink-core/src/core/health.rs` | 纯状态机：metrics + 时间 → HealthVerdict | ≤140 | W1-A（import type） |
| W1-E | `core::selector` | `crates/mlink-core/src/core/selector.rs` | LinkSelector trait + Bandwidth / Explicit 两实现 | ≤100 | W1-A（import type） |
| W1-F | `protocol::capabilities` | `crates/mlink-core/src/protocol/capabilities.rs` | Capabilities 结构 + `negotiate()` | ≤80 | — |
| W1-G | `protocol::types::AckPayload` | `crates/mlink-core/src/protocol/types.rs` | 扩展现有 `Handshake`（+`capabilities`）+ 新增 `AckPayload` serde | +60 | — |
| W1-H | `protocol::errors::NoHealthyLink` | `crates/mlink-core/src/protocol/errors.rs` | 新增 `MlinkError::NoHealthyLink { peer_id }` | +15 | — |

**Wave 1 纯净度自查**
- 全部 **不** `use` `tokio::spawn` / `broadcast::Sender` / `Node` / `DaemonState`。
- 允许 `use crate::transport::Connection`（trait 定义层，不含业务）。
- 允许跨 W1 模块 `import type`（参考 id:381 已验证范式）。

---

## Wave 2 —— 业务胶水（W1 全 PR 合入后启动）

| # | 模块 | 文件路径 | 职责 | 行数 | 依赖 |
| --- | --- | --- | --- | --- | --- |
| W2-1 | `core::connection` 改造 | `crates/mlink-core/src/core/connection.rs` | 从 `HashMap<peer, SharedConnection>` 改为 `HashMap<peer, LinkPool>`；新增 `attach_link / detach_link / primary_conn` | 改 +80 | W1-A, W1-B |
| W2-2 | `core::node::connect` 改造 | `crates/mlink-core/src/core/node/connect.rs` | `register_after_handshake` 容纳多 link；读 Capabilities 决定是否降级 | 改 +50 | W2-1, W1-F |
| W2-3 | `core::node::io` 改造 | `crates/mlink-core/src/core/node/io.rs` | `send_raw` 使用 primary_conn；新增 `spawn_link_reader`（原 `spawn_peer_reader` 降为薄 wrapper） | 改 +70 | W2-1, W1-C |
| W2-4 | `core::node::lifecycle` 改造 | `crates/mlink-core/src/core/node/lifecycle.rs` | 新增 `disconnect_link`；`disconnect_peer` 改为拆整个 pool | 改 +30 | W2-1 |
| W2-5 | `core::node::heartbeat` 新增 | `crates/mlink-core/src/core/node/heartbeat.rs` | 定时 tick：对每 link 用 HealthChecker 推进、对每 peer 扫 retransmit 队列 | ≤160 | W1-C, W1-D, W2-3 |
| W2-6 | `core::node::bond` 新增 | `crates/mlink-core/src/core/node/bond.rs` | Node 聚合层：driver for LinkSelector + primary 切换事件发射 | ≤140 | W1-B, W1-E, W2-1 |
| W2-7 | `core::node::types::NodeEvent` 扩展 | `crates/mlink-core/src/core/node/types.rs` | 新增 `LinkAdded / LinkRemoved / LinkHealthChanged / PrimaryChanged` | 改 +30 | W1-A |
| W2-8 | `daemon::discovery::dual` 新增 | `crates/mlink-daemon/src/discovery/dual.rs` | `spawn_dual` 并行两套 discovery | ≤180 | 现 discovery.rs |
| W2-9 | `daemon::discovery` 拆分 | `crates/mlink-daemon/src/discovery/mod.rs` + `tcp.rs` + `ble.rs` | 把现 318 行的 `discovery.rs` 按 transport 拆 + 公共 connect_loop 抽取 | 每文件 ≤180 | — |
| W2-10 | `daemon::protocol` 扩展 | `crates/mlink-daemon/src/protocol.rs` | 新增 4 下行 + 3 上行 type 的 serde 结构 | 改 +100 | W1-G |
| W2-11 | `daemon::session::link_events` 新增 | `crates/mlink-daemon/src/session/link_events.rs` | 订阅 `LinkAdded/...`，转 WS 帧 | ≤140 | W2-7, W2-10 |
| W2-12 | `daemon::session::dispatch` 扩展 | `crates/mlink-daemon/src/session/dispatch.rs` + 新 handler 文件 | 分发 `set_preferred_transport / list_links / disconnect_link` | 改 +30 + 新 ≤160 | W2-10, W2-6 |
| W2-13 | `web-debug/index.html` 扩展 | `examples/web-debug/index.html` | 双通道状态表 + 切换按钮 + 抖动按钮 + 事件流 | +300 | W2-10 |
| W2-14 | `daemon::state` 扩展 | `crates/mlink-daemon/src/state.rs` | `DaemonState` 增 selector 共享句柄（用于 set_preferred_transport） | 改 +20 | W1-E |

**Wave 2 README 要求（引自 id:377）**
每个 W2 模块必须有时序图 + 竞态分析 + 错误传播路径。重点：
- W2-3 `spawn_link_reader` 并发：多 reader 共享 Arc\<Node\>，EOF 只摘自己那条 link
- W2-5 heartbeat tick：HealthChecker → LinkPool.promote → NodeEvent 的顺序
- W2-8 dual spawn：TCP 连通率高先到 / BLE 后到时的 LinkPool.add 时序

---

## Wave 3 —— 测试与回归（并行 W2 后半期）

| # | 模块 | 路径 | 职责 |
| --- | --- | --- | --- |
| W3-1 | link pool 单测 | `core/link/pool.rs` 内嵌 `#[cfg(test)]` | add/remove/promote/failover |
| W3-2 | retransmit 单测 | `core/retransmit.rs` 内嵌 | push/ack wrap/due_for_retry |
| W3-3 | health 单测 | `core/health.rs` 内嵌 | 滞回；degraded→dead |
| W3-4 | selector 单测 | `core/selector.rs` 内嵌 | 带宽优先；ExplicitSelector |
| W3-5 | dual-link e2e | `crates/mlink-core/tests/dual_link_e2e.rs` | 两条 MockTransport pair，主动 disconnect 主，seq 连续无丢 |
| W3-6 | daemon dual e2e | `crates/mlink-daemon/tests/dual_ws.rs` | WS 看到 link_added / primary_changed / 旧客户端降级 |
| W3-7 | web-debug 手工脚本 | `docs/dual-transport/MANUAL-VERIFY.md` | 配合 W3-6 的手验步骤清单（不写代码） |

---

## 巨模块预警

- `crates/mlink-daemon/src/discovery.rs` 当前 318 行，W2-9 强制拆分。不拆会吸所有双通道逻辑到 500+ 行。
- `examples/web-debug/index.html` 已 730 行；W2-13 增量前先拆外联 JS/CSS（可选 refactor），但在本 Phase 不做以免阻塞。
- `core/node/mod.rs` + 子模块：保持现 module 切分，不强行塞 bond/heartbeat 进 mod.rs。

---

## 人员编排建议（供 team-lead）

- Wave 1 并行上 8 人（W1-A…W1-H 独立文件 / 独立单测，无冲突）。
- Wave 2 并行分三组：
  - 核心组：W2-1 → W2-2 / W2-3 / W2-4 串行
  - 循环组：W2-5 + W2-6 + W2-7 捆绑（一人干完发）
  - 外围组：W2-8 / W2-9 / W2-10 / W2-11 / W2-12 / W2-14 并行
- Wave 3 测试员一人包全部 W3-\*；不可与 Wave 2 实现者重叠（参考 id:377 "产出和验证不同人"）。
