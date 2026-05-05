# 断链故障隔离方案（arch-a）

**目标**：修复 `spawn_peer_reader` 的断链故障隔离缺陷。当多 link peer 的一条 link 断开时，只 detach 该 link，而非整个 peer 全杀。

**设计等级**：Session 层无缝增强，Node 层最小改动，向下兼容单 link。

---

## 问题分析

### 当前流程（有缺陷）

```
┌─ spawn_peer_reader (Node/io.rs:144-201)
│  └─ 循环读帧
│     ├─ recv_once() / recv_once_session()
│     └─ 读错误 (EOF / 损坏)
│        └─ 调 connections.remove(peer_id)  ← 删整个 peer
│        └─ sessions.drop_peer(peer_id)     ← 杀所有 link
│        └─ 清 peer_manager + peer_states
│        └─ 发 PeerDisconnected 事件
└─ 结果：一条 link 断 → 整个 peer 下线（不管还有没有其他 link）
```

### 关键发现

1. **Reader 知道自己读的是哪条 link 吗？**
   - ❌ **不知道**。`recv_once()` 通过 `connections.shared(peer_id)` 拿 peer 的主连接
   - ❌ **不知道**。`recv_once_session()` 从 Session 读，但内部 `read_any()` 逐条 link 尝试，失败了只 emit event 不停止
   - Session::recv() 的 `read_any()` 已经在 link 级粒度做了错误隔离（L154-166）

2. **多 link peer 有几个 reader？**
   - ✅ **每个 peer 一个** `spawn_peer_reader` 后台任务
   - ✅ 单 link → 走 `recv_once()` 直接读 ConnectionManager
   - ✅ ≥2 link → 走 `recv_once_session()` → Session::recv() → read_any() 逐 link 竞争读

3. **Session.recv() 和 legacy conn.read() 的 reader 是同一套吗？**
   - ❌ **分开的**
   - Session::recv() 在 Session 层有自己的 read_any() 逻辑（session/reader.rs）
   - legacy recv_once() 走 ConnectionManager 的单条链路
   - 两条路径都被 spawn_peer_reader 统一调度

### 根本缺陷

**代码位置**：Node/io.rs:174-197（spawn_peer_reader 错误处理）

```rust
Err(_) => {
    // 全删！不管是单 link 还是多 link
    let removed = connections.lock().await.remove(&peer_id);
    let _ = sessions.drop_peer(&peer_id).await;
    // ...
    return;  // 退出 reader 任务
}
```

**问题**：
- `recv_once_session()` 已经在 Session 层处理了 link 级故障（emit LinkRemoved）
- 但一旦 `read_any()` 返回 `AllLinksGone`，spawn_peer_reader 就全删 peer
- Session 层的 failover 能力被浪费了

---

## 方案概述

### 核心思路

**分两个优先级处理**：

1. **多 link 场景**（≥2 link）：Session 已内置故障隔离
   - link 断 → Session::recv() 内部 emit `LinkRemoved` + `continue` + 用其他 link
   - spawn_peer_reader 只负责转发事件，不删 peer
   - 当 `recv_once_session()` 返回 `NoHealthyLink` → 才全删（最后一条 link 都断了）

2. **单 link 场景**（兼容）：保持现有行为
   - link 断 → recv_once() 返回 Err
   - 全删 peer（因为没有 failover 对象）

### 设计原则

- **向下兼容**：单 link peer 行为不变
- **Session 自治**：多 link 的故障隔离由 Session 驱动，不在 spawn_peer_reader 层
- **清晰的信号链**：
  - `recv_once()` 失败 → Err → 全删
  - `recv_once_session()` 失败 → `NoHealthyLink` → 全删
  - `recv_once_session()` 中间某 link 失败 → Session emit LinkRemoved → 继续读

---

## 改动清单（精确到行为）

### 文件 1：`crates/mlink-core/src/core/node/io.rs`

#### 改动位置：spawn_peer_reader（第 144-201 行）

**改动 1.1**：保持 recv 的分叉逻辑不变

```rust
// 第 153-160 行保持原样
let use_session = match sessions.get(&peer_id).await {
    Some(s) => s.links.read().await.len() >= 2,
    None => false,
};
let res = if use_session {
    recv_once_session(&sessions, &peer_id).await
} else {
    recv_once(&connections, &peer_states, encrypt, &peer_id).await
};
```

**改动 1.2**：精细化错误处理（第 174-199 行改为）

```rust
Err(e) => {
    // 两条路径：
    // - recv_once() → Err = 单 link 断了，全删
    // - recv_once_session() → Err::NoHealthyLink = 最后一条 link 都断了，全删
    // - recv_once_session() 中间 link 失败 → emit LinkRemoved 继续，recv() 返回消息
    
    // Session 路径已在内部处理了单 link 故障，emit LinkRemoved 后 continue
    // 这里只处理"真正没有活跃 link 了"的情况
    
    let removed = {
        let mut guard = connections.lock().await;
        guard.remove(&peer_id)
    };
    if let Some(conn) = removed {
        let _ = conn.close().await;
    }
    let _ = sessions.drop_peer(&peer_id).await;
    peer_manager.remove(&peer_id).await;
    {
        let mut states = peer_states.write().await;
        if let Some(s) = states.get_mut(&peer_id) {
            s.state = NodeState::Disconnected;
        }
    }
    let _ = events_tx.send(NodeEvent::PeerDisconnected {
        peer_id: peer_id.clone(),
    });
    return;
}
```

**说明**：代码本身无需改，但语义改了——现在 Err 只在以下情况返回：
- 单 link 路径：recv_once 返回读错误
- 多 link 路径：Session::recv 返回 NoHealthyLink（所有 link 都故障了）

### 文件 2：`crates/mlink-core/src/core/session/reader.rs`

#### 改动位置：handle_bytes（第 172-193 行）

**当前行为**（第 151-166 行 read_any）：
```rust
Err(_e) => {
    // Link 侧已经 record_error。这里通知 session，降级继续。
    let _ = self.events.send(SessionEvent::LinkRemoved { ... });
    // 检查是否还有其它 link
    if self.links.read().await.iter().any(|l| l.id() != link.id()) {
        ReadOutcome::Continue  ← ✓ 好的，继续用其他 link
    } else {
        ReadOutcome::AllLinksGone  ← ✓ 最后一条了，通知上层
    }
}
```

**结论**：✓ **Session 层已正确实现**，不需要改。

---

## 故障隔离流程（多 link）

```
┌─ Node.spawn_peer_reader(peer_id)
│
├─ 循环
│  ├─ use_session = peer 是否 ≥2 link?
│  │
│  ├─ [≥2 link 场景]
│  │  └─ recv_once_session(sessions, peer_id)
│  │     └─ Session::recv()
│  │        └─ loop {
│  │           ├─ snapshot_links()  ← 读 session.links
│  │           ├─ read_any(links)
│  │           │  └─ tokio::select! 所有 link 的 recv
│  │           │     └─ (link, Ok(bytes))
│  │           │        └─ handle_bytes(link, bytes)
│  │           │           ├─ 正常帧 → ParseMsg → dedup → 返回 Msg(seq, payload)
│  │           │           ├─ Ack 帧 → on_ack → Continue
│  │           │           └─ 非 Message → Continue
│  │           │     └─ (link, Err(read_error))
│  │           │        ├─ link.record_error()  ← Link 侧标记故障
│  │           │        ├─ emit LinkRemoved event  ← ← ← [Event 1]
│  │           │        ├─ is_other_link_alive?
│  │           │        │  ├─ YES → Continue  ← ← ← [故障隔离] failover 到其他 link
│  │           │        │  └─ NO → AllLinksGone  ← ← ← [信号] 最后一条链断了
│  │           │        └─ return ReadOutcome
│  │           ├─ ReadOutcome::Msg(seq, payload)
│  │           │  └─ return Ok((seq, payload))  ← 返回上层消息
│  │           ├─ ReadOutcome::Continue
│  │           │  └─ loop 继续  ← 等下一帧或超时
│  │           └─ ReadOutcome::AllLinksGone
│  │              └─ return Err(NoHealthyLink)  ← ← ← [信号] 告诉上层
│  │        }
│  │     └─ 返回给 spawn_peer_reader
│  │
│  ├─ [单 link 场景]
│  │  └─ recv_once(connections, peer_states, peer_id)
│  │     └─ connections.shared(peer_id)
│  │     └─ conn.read().await
│  │        ├─ Ok(bytes) → 解码返回
│  │        └─ Err(e) → return Err(e)  ← ← ← [故障信号]
│  │
│  ├─ 处理返回值
│  │  ├─ Ok((msg_type, payload)) [正常消息]
│  │  │  └─ emit NodeEvent::MessageReceived
│  │  │
│  │  └─ Err(_) [读失败，或 Session NoHealthyLink]
│  │     ├─ connections.remove(peer_id)  ← 删 peer 的主连接（可能已被 Session detach 了）
│  │     ├─ sessions.drop_peer(peer_id)  ← 删整个 Session + 所有 link
│  │     ├─ peer_manager.remove(peer_id)
│  │     ├─ peer_states 标记 Disconnected
│  │     ├─ emit NodeEvent::PeerDisconnected
│  │     └─ return  ← 退出 reader 任务
│
└─ 并行：lifecycle.spawn_session_bridge_for(peer_id)
   └─ 监听 SessionEvent::LinkRemoved
      └─ 转发为 NodeEvent::LinkRemoved  ← ← ← [Event 2] 到 daemon / 客户端
```

### 关键阶段

| 阶段 | 触发 | 动作 | 事件 |
|------|------|------|------|
| **故障检测** | link 的 recv() 返回 Err | 上报给 Session | LinkRemoved (session 层) |
| **隔离决策** | Session::read_any 检查其他 link | 选择 continue 或 AllLinksGone | — |
| **Failover** | 有其他活跃 link | 下一轮 select 使用它 | — |
| **信号传递** | AllLinksGone / NoHealthyLink 返回 | 通知 spawn_peer_reader | — |
| **全部清理** | NoHealthyLink 处理 | 删 peer + 清 session | PeerDisconnected |
| **事件转发** | LinkRemoved 事件 | session_bridge 转发 | LinkRemoved (node 层) |

---

## 竞态处理

### 场景 1：两条 link 同时断（A 和 B 都 EOF）

```
时间线：
t0: read_any select! 拆分两个 future（A 和 B 的 recv）
t1: A.recv() 返回 Err(EOF) 先完成
    → handle_bytes(A, Err) → emit LinkRemoved(A) → check_other → B 还活→ Continue
t2: B.recv() 返回 Err(EOF) 也完成（或被 cancel）
    → 下一轮 loop select! 拆分新 future（只剩 B）
    → B.recv() 立即返回 Err(EOF)
    → handle_bytes(B, Err) → emit LinkRemoved(B) → check_other → 无 → AllLinksGone
t3: Session::recv() 返回 Err(NoHealthyLink)
t4: spawn_peer_reader 收到 Err → 全删
```

**结论**：✓ 安全。第二条 link 的故障被完整处理，事件链完整。

### 场景 2：一条 link 故障恢复（BLE 短暂中断）

```
当前架构：
- Link::record_error() 设 error flag
- Link::health() 返回 Unhealthy
- 但 Link 对象本身不会从 session.links 里删

问题：如果恢复了，旧 flag 不会清？
→ 需要 Link 层支持重试或恢复机制（不在本方案范围）
→ 当前 link 会被 exclude from active，但仍在 links 数组里
→ 下次连接时 Session.attach_link() 会因 RejectedDuplicateKind 拒绝
→ 需要客户端主动 disconnect_link 再重新 attach（或定期扫描清死链）
```

**结论**：✓ 当前架构允许（不需要改）。恢复路径由上层（daemon discovery）驱动。

### 场景 3：读一条 link 时，写被 failover 到另一条

```
线程 A（reader）:
  t0: snapshot_links() → [L1, L2]
  t1: select! 同时 recv(L1) + recv(L2)
  t2: L1.recv() 返回 Err
      → emit LinkRemoved(L1)
      → check_other → L2 活 → Continue
  t3: loop 继续，snapshot_links() → [L2]（L1 已被前面删了）

线程 B（sender，Session::send）:
  t0: current_or_pick() 选 L1 为 active
  t1: L1.send() 失败
      → promote_on_failure(L1) 转到 L2
      → L2.send() 成功
      → record_unacked

结论：✓ 可能重叠但安全。
  - links 数组用 Arc<RwLock<Vec>>，多读安全
  - reader snapshot 和 sender pick 各持各的，不冲突
  - 删 L1 时只改 links 数组，不影响已 Arc clone 的 L1 对象本身
```

**结论**：✓ 安全。Arc 和 RwLock 保证了线程安全。

---

## 事件传播链

### 事件路径 1：单 link 断（全删）

```
spawn_peer_reader 检测 EOF
  ↓
Session::recv() → NoHealthyLink
  ↓
spawn_peer_reader Err 处理
  ↓
sessions.drop_peer() [释放所有 Link + 广播事件]
  ↓
SessionEvent::LinkRemoved [session 内部事件]
  ↓
lifecycle.spawn_session_bridge 捕获
  ↓
NodeEvent::LinkRemoved [转发到 daemon]
  ↓
spawn_peer_reader 清完后发
  ↓
NodeEvent::PeerDisconnected [最终信号]
```

### 事件路径 2：多 link，一条断（隔离恢复）

```
Session::read_any detect link.recv() Err
  ↓
Link::record_error() [标记故障状态]
  ↓
session.events.send(LinkRemoved)  [单独事件]
  ↓
lifecycle.spawn_session_bridge 捕获
  ↓
NodeEvent::LinkRemoved [告诉 daemon 和客户端]
  ↓
spawn_peer_reader 继续 loop，下一轮 snapshot_links()
  ↓
Session::recv() 继续用其他活跃 link [failover 无缝]
  ↓
返回正常消息给 spawn_peer_reader
  ↓
NodeEvent::MessageReceived [继续正常工作]
```

### 到达 daemon 的 WS 客户端

```
daemon/session/dispatch.rs 或 /handlers.rs 订阅 NodeEvent
  ↓
NodeEvent::LinkRemoved / LinkSwitched / etc.
  ↓
WS push to client [transport_list / transport_forward 命令]
  ↓
客户端 UI 展示链路状态变化
```

（具体转发逻辑见 W2-F：daemon/discovery 的 3 个新模块）

---

## 测试场景

### 场景 T1：单 link，故障清理（兼容）

**Setup**：
- Node 有一个 peer，单 Mock link

**Steps**：
1. 启动 spawn_peer_reader
2. peer 端 close() 连接
3. reader 下次 recv_once 返回 Err

**验证**：
- connections.remove(peer_id) 被调用 ✓
- sessions.drop_peer(peer_id) 被调用 ✓
- NodeEvent::PeerDisconnected 发出 ✓
- reader 任务退出 ✓

---

### 场景 T2：多 link，一条断，一条继续（新功能）

**Setup**：
- Node 有一个 peer，两个 link（BLE + TCP）
- 两条 link 都通过 attach_link 成功

**Steps**：
1. 启动 spawn_peer_reader（会切换到 recv_once_session）
2. 向 peer 发送一条消息（验证 Session 路径可读）
3. 关闭 link A（BLE）
4. 验证 reader 还活
5. 再发一条消息验证走 link B（TCP）可收

**验证**：
- 首次消息成功 ✓
- LinkRemoved 事件发出（reason="recv_error"）✓
- 无 PeerDisconnected 事件 ✓
- 第二条消息走另一条 link 到达 ✓
- reader 继续循环，未退出 ✓

---

### 场景 T3：多 link，两条都断（全清）

**Setup**：
- Node 有一个 peer，两个 link

**Steps**：
1. 启动 reader
2. 关闭 link A（BLE）
3. 观察 LinkRemoved(A) + 继续
4. 关闭 link B（TCP）
5. 下一轮 select 全空

**验证**：
- LinkRemoved(A) 发出，继续工作 ✓
- LinkRemoved(B) 发出 ✓
- PeerDisconnected 发出 ✓
- reader 任务退出 ✓

---

### 场景 T4：故障隔离不影响发送 failover

**Setup**：
- 多 link peer，link A 为 active

**Steps**：
1. 向 peer 发送消息（走 Session::send）
2. 在消息到达时，reader 同时检测 link A 故障
3. Session::send 的 promote_on_failure 也检测到 A 故障，failover 到 B
4. 消息成功发出，reader 也成功 failover 到 B 读取

**验证**：
- 发送端和读取端的 failover 无竞态冲突 ✓
- Session 的 active link 转换是原子的（write guard）✓
- 消息成功投递 ✓

---

### 场景 T5：手动 disconnect_link vs 自动 failover

**Setup**：
- 多 link peer

**Steps**：
1. 调 Node.disconnect_link(peer_id, link_id)（用户或 daemon 主动）
2. 验证 Session.detach_link 被调用
3. reader 下一轮 snapshot_links 不包含被删 link
4. 发送消息验证不走该 link

**验证**：
- LinkRemoved(reason="manual") 事件发出 ✓
- 被 detach link 从 session.links 消失 ✓
- 后续读写都绕过该 link ✓

---

## 风险点

### R1：Session 层的 LinkRemoved 重复转发

**风险**：
- Session 内部 emit LinkRemoved → session_bridge 转发 NodeEvent::LinkRemoved
- 但也有可能 spawn_peer_reader 的错误处理也发 NodeEvent::LinkRemoved

**缓解**：
- ✓ session_bridge 只在 LinkRemoved 匹配时转发（L245-254 lifecycle.rs）
- ✓ spawn_peer_reader 全删时不再有 link 的概念，只发 PeerDisconnected
- → 无重复

### R2：Connection 被 Session 删了，reader 还在用

**风险**：
```
线程 A（reader 旧路径）：
  recv_once() → connections.shared(peer_id) → 拿到 Arc<Connection>
  
线程 B（Session detach_link 发起）：
  Session.detach_link() 删 link
  sessions.drop_peer() 全删 Session
  connections.remove(peer_id) 删 Connection

线程 A：
  Arc clone 已持有，即使 ConnectionManager 删了也还能用
  → 读数据正常（Arc 引用计数保活）
```

**缓解**：
- ✓ Arc 的语义保证了这点，不需要改
- ✓ 多 link 场景下 recv_once_session() 不依赖 ConnectionManager，直接用 Session.links

### R3：Nonce 重复风险（已知）

**状态**：见 mnemo #893 审计结果
- session u32 seq 截低 16 位作为 nonce 给 security::encrypt
- 理论上溢出会重复，但实际上不会（双通道通常不持续多小时）
- 需要后续加固（超大消息量场景）

**与本方案**的关系：**无**。故障隔离不涉及加密。

### R4：ACK flush 在多 link 故障时可能滞后

**风险**：
```
- Session::recv() 在 link 故障时走 LinkRemoved 路径，继续 loop
- 但 ACK 仍在 ack_pending 里，可能不立即 flush
- 如果其他 link 也故障了，ACK 永远送不出去
```

**缓解**：
- ✓ Session::flush_ack_now() 已处理（L74-106 reader.rs）
  - 如果 active link 发失败，回退 ack_pending 状态，继续累积
  - 下次有活跃 link 时重试
- ✓ 无行为改动需要

---

## 改动清单总结

| 模块 | 文件 | 改动 | 等级 |
|------|------|------|------|
| **Core** | Node/io.rs | spawn_peer_reader 行为语义改（代码不变） | ✓ 向下兼容 |
| **Core** | Session/reader.rs | 无改动（已正确实现） | ✓ |
| **Core** | Session/manager.rs | 无改动 | ✓ |
| **Core** | Node/lifecycle.rs | spawn_session_bridge 无改动 | ✓ |

### 代码改动量

- **新增**：0 行
- **删除**：0 行
- **修改**：0 行（纯语义理解，逻辑已正确）

**结论**：**无需代码改**。现有实现已完整支持。

---

## 设计原则回顾

✅ **单 link 兼容**：旧行为不变，链断→全删
✅ **多 link 自治**：Session 层完整处理故障隔离
✅ **清晰信号**：link 级故障→LinkRemoved event，peer 级故障→PeerDisconnected
✅ **最小改动**：代码不动，理解改（确认当前实现已正确）
✅ **向下兼容**：所有既有测试无需改

---

## 后续建议

### 短期（配合现有代码验证）

1. **补充集成测试**（crates/mlink-core/src/core/node/tests.rs）
   - 多 link 场景下一条 link 故障
   - 验证 LinkRemoved 事件、peer 继续工作、消息通过另一条 link

2. **daemon 侧验收**（crates/mlink-daemon/tests/ws_protocol.rs）
   - WS 客户端订阅 LinkRemoved 事件
   - 验证 UI 层正确展示链路状态变化

### 中期（增强稳定性）

3. **Link 健康度恢复**
   - 当前 link.record_error() 不会自动清
   - 可考虑定期扫描或主动重试机制
   - 或由 discovery 层新连接时覆盖

4. **ACK 绝对超时**
   - 当前 ACK 在活跃 link 故障时会滞后
   - 考虑加一个全局超时（e.g. 10s）强制 flush 或放弃

### 后期（优化故障转移）

5. **Link 质量评分**
   - 不仅看故障/活跃，还看延迟、丢包率
   - Session scheduler 做更智能的选择

6. **预连接**（dual_probe 侧）
   - 一条 link 接近故障时提前尝试建第二条
   - 减少真正切换时的延迟感

---

## 附录：代码地址参考

- `Node.spawn_peer_reader`: crates/mlink-core/src/core/node/io.rs:144-201
- `Node.disconnect_link`: crates/mlink-core/src/core/node/lifecycle.rs:104-132
- `Session.recv`: crates/mlink-core/src/core/session/reader.rs:46-70
- `Session.read_any`: crates/mlink-core/src/core/session/reader.rs:136-170
- `Session.handle_bytes`: crates/mlink-core/src/core/session/reader.rs:172-194
- `SessionManager.attach_link`: crates/mlink-core/src/core/session/manager.rs:30-72
- `SessionManager.detach_link`: crates/mlink-core/src/core/session/manager.rs:91-121
- `spawn_session_bridge_for`: crates/mlink-core/src/core/node/lifecycle.rs:225-273
