# 双通道融合方案（下版本迭代）

## 目标
同一设备同时通过 TCP 和 BLE 两个通道连接，TCP 优先，断线自动切换。用户无感。

## 理想状态
- 两个通道都连接
- 数据走 TCP 优先（高带宽低延迟）
- TCP 断了自动切 BLE，BLE 断了自动切 TCP
- 两个通道都恢复后自动回到 TCP

## 当前阻塞
- Node 的 DEDUP 逻辑会关掉第二条通道（只保留先到的）
- 断线后没有自动重连，也没有切换通道
- ReconnectPolicy 结构已写但未启用

## 改造要点
1. ConnectionManager 改为支持同一 app_uuid 多条连接（按 transport_id 区分）
2. Peer.transport_id 改为 `Vec<String>`
3. send_raw 加选路策略：TCP 优先，BLE 兜底
4. PeerDisconnected 事件触发另一通道重连
5. Scanner 同时跑 BLE + TCP，发现记录存入 Peer.last_seen_via

## 预估工作量
- 方案 A（断线切换）：2-3 天
- 方案 B（双通道并存+选路）：1-2 周

## 相关文件
- `crates/mlink-core/src/core/node.rs` — DEDUP 逻辑
- `crates/mlink-core/src/core/peer.rs` — Peer 结构
- `crates/mlink-core/src/core/connection.rs` — ConnectionManager
- `crates/mlink-core/src/core/reconnect.rs` — 已写未用
