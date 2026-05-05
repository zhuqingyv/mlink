# 开发任务清单 + 依赖图（arch-b）

## 依赖拓扑

```
Wave 1（完全并行）
  W1-A protocol/ack.rs          ── 独立，纯工具
  W1-B protocol/seq.rs          ── 独立，纯工具
  W1-C core/link.rs             ── 独立，对外只用 transport::Connection
  W1-D core/session/types.rs    ── 依赖 W1-A, W1-B, W1-C 的"类型名字"可通过
                                    `use` 前置引用；实际编译前 Wave 1 其余模块
                                    必须合入
  W1-E protocol/types.rs (+40)  ── 独立

Wave 2（按依赖串并）
  W2-A scheduler.rs   ← W1-D (LinkStatus)
  W2-B session/io.rs  ← W1-A, W1-B, W1-D, W2-A
  W2-C session/reader.rs ← W1-A, W1-B, W1-D, W2-A
  W2-D manager.rs     ← W1-C, W1-D, W2-B, W2-C
  W2-E node/io.rs     ← W2-D
  W2-F node/connect.rs ← W2-D, W1-E
  W2-G dual_probe.rs  ← W1-C
  W2-H daemon transport_debug ← W2-D, W2-E, W2-F
  W2-I daemon discovery ← W2-F, W2-G
  W2-J web-debug UI   ← W2-H
```

## 分工建议（7 人，跟既有"10 人团队"命名）

### Wave 1（并行 3 天上限）
| 人 | 任务 | 文件 | 产出 |
| --- | --- | --- | --- |
| dev-1 | W1-A | protocol/ack.rs | AckFrame + UnackedRing + 单测 |
| dev-2 | W1-B | protocol/seq.rs | SeqGen + DedupWindow + 单测 |
| dev-3 | W1-C | core/link.rs | Link + LinkHealth + 单测 |
| dev-4 | W1-D | core/session/types.rs | Session 结构 + SessionEvent + LinkStatus |
| dev-5 | W1-E | protocol/types.rs | Handshake 新字段 + serde 兼容测试 |

Wave 1 合入门禁：全员 `cargo test -p mlink-core --lib` 通过，
`cargo build -p mlink-core`、`cargo build -p mlink-daemon` 两项绿。

### Wave 2（并行分组，约 5 天）

**组 A（Session 引擎，内部串行）**
| 序 | 人 | 任务 |
| --- | --- | --- |
| 1 | dev-1 | W2-A scheduler.rs |
| 2 | dev-2 | W2-B session/io.rs（发送 + 重传 + ack 处理） |
| 3 | dev-3 | W2-C session/reader.rs（多 reader 合并 + dedup + ack 生成） |
| 4 | dev-4 | W2-D manager.rs + 替换 ConnectionManager 出口 |

组 A 内部 1 → (2 ∥ 3) → 4；2 和 3 可同时开工，接口走 INTERFACE-CONTRACTS.md。

**组 B（Node 接入，依赖组 A W2-D）**
| 序 | 人 | 任务 |
| --- | --- | --- |
| 5 | dev-5 | W2-E node/io.rs 接到 SessionManager |
| 6 | dev-6 | W2-F node/connect.rs（去重改成 attach_link） |

**组 C（Transport & Daemon，依赖组 B）**
| 序 | 人 | 任务 |
| --- | --- | --- |
| 7 | dev-7 | W2-G transport/dual_probe.rs |
| 8 | dev-1 | W2-H daemon transport_debug（W2-D/E/F 完成后启动） |
| 9 | dev-2 | W2-I daemon discovery.rs Dual 分支 |

**组 D（前端 & 回归）**
| 序 | 人 | 任务 |
| --- | --- | --- |
| 10 | dev-3 | W2-J web-debug/index.html |
| 11 | dev-4 | 回归用例（REGRESSION.md 全跑） |

## 完成判据（每个 Wave）

### Wave 1 完成标准
- 每个新模块带 ≥ 5 条单测
- `cargo test -p mlink-core --lib` 覆盖新模块 ≥ 90%
- 无新增 clippy warning
- CONTRACTS 文件未改 = 不开 Wave 2

### Wave 2 完成标准
- `cargo test --workspace` 全绿（含 mlink-daemon 的 ws_protocol 集成测）
- 手工场景全过：
  - 双通道并存：BLE + TCP 同时挂；`transport_list` 返回 2 条 link
  - 切换毫秒级：断开 active 一条，计时直到下一条 frame 发出 < 10ms
  - 不丢不重：压测 1000 条 ping 含一次强制切换，收端计数正好 1000 且无重复
  - 带宽自适应：BLE→TCP 升速后吞吐提升 ≥ 10×（跑 1MB 数据传输测时长对比）
  - web-debug UI：展示活跃 / 备用、按钮手动切换、断链后显示 1/2 active
- 旧用例零回归（见 REGRESSION.md）

## 风险与缓解（独立视角）

| 风险 | 诊断 | 缓解 |
| --- | --- | --- |
| 切换抖动 | BLE 短暂丢包被判故障，但几百毫秒后又恢复 | hysteresis=3 tick，错误只有真正 `write().await` Err 才计故障 |
| seq 32bit wrap | 长连接 1000 条/s ≈ 50 天环一圈 | DedupWindow 只看 mod 2^32 差值 < 2^31 为"后续" |
| Ack 风暴 | 每条 frame 一个 ack | 合并 ack（见契约 §不变量 #2）：50ms 或 8 条凑一次 |
| Unacked 爆 | BLE 长分区 unacked 超 256 | push 返回丢弃项 → 日志 warn + 应用层显式 error |
| Bonding 与 Session 方案融合 | 若最终与 arch-a 方案合并 | Session 方案的内部结构可包进 Bonding 的 DualConnection，反之困难 |
| 回归面大 | 改了 Node 底层 | ConnectionManager 兼容壳必须保留，所有既有 mock 测试继续跑绿 |

## 时间盘子

- Wave 1：3 天
- Wave 2 组 A：3 天
- Wave 2 组 B + C：2 天
- Wave 2 组 D：2 天
- 回归 & 修 bug：2 天
- **总计约 12 工日 / 7 人**（与 arch-a 估算 1 周持平，但并行度更高）
