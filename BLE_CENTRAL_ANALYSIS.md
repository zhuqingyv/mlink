# BLE Central 侧锁竞争调查报告

## 锁分析

### BleConnection 层的两个独立锁

**write 路径（ble.rs:455-473）：**
- 获取 `self.peripheral` 的 Mutex
- 立即 `.cloned()` 释放锁（O(1) 操作）
- 实际 `peripheral.write().await` 在锁**外**运行
- ✅ **无阻塞风险**

**read 路径（ble.rs:440-452）：**
- 获取 `self.notifications` 的 Mutex
- 在 `stream.next().await` 上长时间持有锁
- ⚠️ **可能长时间占用，但使用不同的 Mutex，不会阻塞 write**

**结论：** 两个锁**完全独立**，read 的长锁时间不会竞争 write 的锁。

---

### ConnectionManager 层的 Mutex<HashMap>

**io.rs:123-128 的 send_legacy 路径：**
```rust
let conn = {
    let guard = self.connections.lock().await;
    guard.shared(peer_id).ok_or_else(...)?
};
conn.write(&bytes).await  // ← 锁已释放
```

- HashMap Mutex 只在 `.shared()` 克隆时持有
- 实际写在锁外
- 代码注释明确提到过去踩过这个坑：
  > "Otherwise a parked reader holding the map lock during `read().await` would block every `send_raw` in the process — that was the exact BLE chat deadlock."

✅ **已正确修复**

---

### 上层 peer_states 写锁（io.rs:77-103）

**send_legacy 的 peer_states 竞争：**
```rust
async fn send_legacy(...) -> Result<()> {
    let (seq, compressed_flag, encrypted_flag, body) = {
        let mut states = self.peer_states.write().await;  // ← 获取写锁
        let st = states.get_mut(peer_id)?;
        let seq = st.next_seq();
        
        let mut body = payload.to_vec();
        let compressed_flag = if should_compress(&body) {
            body = compress(&body)?;  // ← 在锁内压缩
            true
        } else { false };
        
        let encrypted_flag = if self.config.encrypt {
            if let Some(key) = &st.aes_key {
                body = crate::core::security::encrypt(&body, key, seq)?;  // ← 在锁内加密
                true
            } else { false }
        } else { false };
        (seq, compressed_flag, encrypted_flag, body)
    };  // ← 锁在这里释放
```

**问题：**
- `compress()` 和 `encrypt()` 可能是 CPU 密集操作（毫秒级）
- 期间 `peer_states` 的写锁被持有
- 任何其他任务调用 `spawn_peer_reader` → `recv_once` → `peer_states.read()` 会排队等待
- 在高并发或 Force 模式下，多个 send 任务可能在写锁上排队

⚠️ **有竞争风险，但不是根本原因**

---

### Session 层的可能序列化

**Force 模式下的路由（io.rs:26-27）：**
```rust
if self.session_is_multilink(peer_id).await {
    return self.send_via_session(peer_id, msg_type, payload).await;
}
```

**send_via_session 的实现（io.rs:39-68）：**
```rust
async fn send_via_session(...) -> Result<()> {
    let session = self.sessions.get(peer_id).await?;
    session.send(msg_type, payload.to_vec(), encrypt_key.as_deref(), compress_on).await?;
}
```

**未知的 Session::send() 行为：**
- 代码不在本文件内，需要查看 `crates/mlink-daemon/src/session/` 或 `crates/mlink-core/src/core/session/`
- 可能在 Session 层有全局写序列化
- 可能要等对端的 ack 才能发第二条
- Force 模式强制走这条路，症状"第二条卡"就会显现

🔴 **最可能是根本原因**

---

## 阻塞点

### 现象
"发一条消息后第二条发不出去，直到对方回一条才能继续"

### 可能的根源（按概率排序）

| 位置 | 概率 | 症状 |
|------|------|------|
| **Session::send() 全局锁** | 🔴 高 | 第 2、3 条 hang，需要对端回复 |
| **peer_states 写锁竞争** | 🟡 中 | 发送序列化，但不依赖对端回复 |
| **btleplug 的 GCD 队列** | 🟢 低 | 消息最终都发出，但有序列化 |

---

## 修复建议

### 1. 追踪 Session 层（优先调查）
```
调查文件：
  - crates/mlink-daemon/src/session/dispatch.rs
  - crates/mlink-core/src/core/session/manager.rs (Session::send 实现)
  
核心问题：
  - Session::send() 是否对所有 link 有全局 Mutex？
  - 是否在等待对端 ack 才允许下一条发送？
  - send_via_session → Session::send → dispatch 的路径是什么？
```

### 2. 确认 Force 模式的触发条件
```
问题：
  - Force 模式是否强制所有 BLE 连接走 multi-link session？
  - session_is_multilink() 返回 true 的确切条件是什么？
  
验证：
  - grep "session_is_multilink" 的所有调用点
  - 追踪 Force 模式如何初始化 session
```

### 3. 添加日志追踪
```
建议埋点：
  1. ble.rs write() 的入口和出口（记时）
  2. io.rs send_legacy/send_via_session 的入出口
  3. Session::send() 的前后（如果可见）
  
观察指标：
  - 第一条 write 耗时 vs 第二条 write 耗时
  - 第二条在哪个 await 上 pending：
    * peer_states.write()
    * session.send()
    * peripheral.write()
  - 对端收到的消息顺序和完整性
```

### 4. 微观测试
```
场景：BLE 链接不等对端回复，快速发 3 条消息

预期行为（如果没有问题）：
  - 3 条都最终发出
  - 可能有底层序列化延迟，但不依赖对端回复

实际观察（如果是 Session 全局锁）：
  - send(msg1) 成功返回
  - send(msg2) hang，直到对端回复 msg1
  - send(msg3) hang

测试代码位置：
  - crates/mlink-cli/tests/ 或 crates/mlink-daemon/tests/
```

---

## 关键发现

1. ✅ **ble.rs 层的锁设计完全正确** — read/write 使用不同的 Mutex，互不竞争
2. ✅ **io.rs 的 ConnectionManager 锁已修复** — 注释明确记录过去踩过的坑
3. 🔴 **问题不在 BLE 连接本身，而在上层（Session 或 peer_states）**
4. ⚠️ **WithoutResponse 不代表真正异步** — btleplug 底层可能有单线程队列
5. 🔴 **Force 模式触发了不同代码路径** — send_legacy vs send_via_session

---

## 总结

**BLE central 侧的 ble.rs 代码没有锁竞争问题。**

症状"发一条卡，对端回复才能继续"的根本原因更可能是：

1. **Session::send() 有全局序列化机制**（最可能）
2. 或 Node 层的 peer_states 写锁在压缩/加密上的竞争（中等可能）
3. 或 btleplug 底层的 GCD 单线程队列（低可能）

**下一步建议：** 在 `send_via_session()` 和 Session::send() 路径加日志，追踪第二条消息具体卡在哪个 await。特别是查看 Session 层是否有"等待 ack 后再发送下一条"的逻辑。

---

## 相关代码位置

- **ble.rs write():** 455-473 行
- **ble.rs read():** 440-452 行  
- **io.rs send_legacy():** 70-130 行
- **io.rs send_via_session():** 39-68 行
- **connection.rs shared():** 83-85 行
- **Node::send_raw():** io.rs 16-30 行
