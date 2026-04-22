# mlink 旧架构问题清单

审计时间：2026-04-20
审计范围：`crates/mlink-core/src/transport/{trait,ble,tcp,peripheral}.rs`、`crates/mlink-core/src/core/{node,connection,scanner,peer,reconnect}.rs`、`crates/mlink-cli/src/main.rs`、`PITFALLS.md`

**严重度定义**
- P0：阻塞功能（功能不可用 / 直接 panic）
- P1：影响稳定性（运行时易出错、竞态、资源泄漏）
- P2：影响可维护性（重复代码、巨大函数、坏抽象）
- P3：代码质量（风格、debug 残留）

---

## 维度 1：Transport trait 设计 — scan/connect/listen 捆绑

### 1.1 P1 — 同一 trait 同时承担发现 / 客户端 / 服务端三种角色

**文件**：`crates/mlink-core/src/transport/trait.rs:33-40`

```rust
pub trait Transport: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> TransportCapabilities;
    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>>;
    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>>;
    async fn listen(&mut self) -> Result<Box<dyn Connection>>;
    fn mtu(&self) -> usize;
}
```

**问题**
- 一个 `&mut self` 的 trait 既扫描、又发起连接、又监听，导致上层必须为 **scan / connect / listen 各建一份 transport 实例**。见 `crates/mlink-cli/src/main.rs:274-287`（scan_transport）vs `main.rs:305-308`（connect_transport）vs `main.rs:171-176`（listen_transport），同一个 `cmd_serve` 里最多同时持有 3 份 BleTransport。
- BLE 侧通过一个静态 `SHARED_ADAPTER`（`crates/mlink-core/src/transport/ble.rs:19`）来给多个 `BleTransport` 共享同一个 CoreBluetooth adapter，这本身就是 trait 抽象漏了的补丁。

**影响范围**：transport 全部实现、CLI 全部子命令。

**建议修法**：拆成 `Discoverer` / `Dialer` / `Acceptor` 三个 trait（或拆成 `scan_stream()` + `dial` + `accept_stream()`），各自独立拥有状态。

---

### 1.2 P2 — `DiscoveredPeer.metadata: Vec<u8>` 语义模糊

**文件**：`crates/mlink-core/src/transport/trait.rs:14-20`、`crates/mlink-core/src/transport/ble.rs:251-269`、`crates/mlink-core/src/transport/tcp.rs:298-317`

- BLE 塞 8 字节 identity（`ble.rs:257-258`）、或 room_hash、或 manufacturer_data；
- TCP 塞 room_hash 的 8 字节（`tcp.rs:300-304`）；
- Scanner 对 `metadata` 再做 sliding-window 匹配（`scanner.rs:86-91`）。

同一个字段三种格式，语义靠 transport 隐式约定，CLI 还要手动 `peer.metadata.len() == 8` 再 copy 出来当 identity 用（`main.rs:377-384`、`main.rs:770-777`）。

**建议修法**：结构化 metadata（`room_hash: Option<[u8;8]>`、`identity: Option<[u8;8]>`）或拆成两个字段。

---

### 1.3 P2 — `Connection::read` 无法 cancel safe 使用

**文件**：`crates/mlink-core/src/transport/trait.rs:25`、`crates/mlink-core/src/core/node.rs:542-571`

`Connection::read` 是 `async fn read(&mut self)`，Node 在主循环用 `tokio::sync::Mutex` 包 `ConnectionManager` 去调用（`node.rs:526-531`、`node.rs:684-691`）。每次读一帧都要锁住整个 connections 表，长 read 会阻塞其它 peer 的写。

**建议修法**：把 read 单独剥出来跑后台任务（`spawn_peer_reader` 已经在做一半），write 走独立 mpsc。

---

## 维度 2：main.rs 重复代码 — 1627 行 + 三大 cmd 几乎完全重复

### 2.1 P2 — `cmd_serve` / `cmd_chat` / `cmd_join` 结构 90% 重合

**文件**：`crates/mlink-cli/src/main.rs`
- `cmd_serve`：`main.rs:137-542`（约 405 行）
- `cmd_chat`：`main.rs:548-933`（约 385 行）
- `cmd_chat` 与 `cmd_serve` 共用的代码段：
  - 建 node + room_hash + start：见 `main.rs:138-146` vs `main.rs:551-555`
  - Peripheral/TCP accept spawn 循环：`main.rs:170-255` vs `main.rs:566-645`（几乎逐行一致，仅 `set_room_hash` 非可选）
  - Scanner 构建 + room filter：`main.rs:262-294` vs `main.rs:651-676`
  - Main select 循环：`main.rs:336-538` vs `main.rs:731-926`（peer_rx / accepted_rx / events 三个 arm 逻辑一致）
  - Per-peer 状态表（`engaged_wire_ids` / `attempts` / `connected_inbound` / `connected_peers` / `wire_to_app`）：`main.rs:316-334` vs `main.rs:716-729`
- `cmd_join` 又复制了一次（`main.rs:940-1164`，约 225 行），只是少了 peripheral accept 分支。

**统计**
- 三个大命令共计 ~1015 行，其中约 **700 行是跨命令的粘贴复制**。
- `BleTransport::new()` / `TcpTransport::new()` 在 `main.rs` 中被实例化的次数：多处（connect_transport、listen_transport、scan_transport）。
- `engaged_wire_ids.contains` / `connected_inbound.contains` / `wire_to_app` 的 dedup 逻辑在三个函数里各抄了一遍。

**建议修法**：提炼 `struct SessionRuntime { node, accepted_rx, peer_rx, unsee_tx, engaged, attempts, ... }`；把 select 循环抽成 `run_session_loop(mut runtime, handlers)`，三个命令只注入不同 handler（chat 有 stdin+reader，serve 没有）。

---

### 2.2 P1 — `unsafe_clone_node` 是一个永远 panic 的函数

**文件**：`crates/mlink-cli/src/main.rs:1342-1357`

```rust
fn unsafe_clone_node(_n: &Node) -> Node {
    unimplemented!(
        "file transfer over a short-lived CLI requires the daemon path (not yet wired); \
         use `mlink send <code> <text>` for now"
    )
}
```

被 `send_file_to_peers` 在 `main.rs:1326` 调用；只要用户 `mlink send <code> --file <path>` 就会 panic。

**建议修法**：要么彻底删掉 `--file` CLI 选项，要么把 Node 改成 `Arc<Node>` 一次到位。

---

### 2.3 P3 — `_reserved_mutex` 死代码

**文件**：`crates/mlink-cli/src/main.rs:1589-1593` 带 `#[allow(dead_code)]`，仅为保留 `tokio::sync::Mutex` import。

---

## 维度 3：日志系统 — tracing 空跑，eprintln 满天飞

### 3.1 P2 — `tracing_subscriber` 初始化了但无人 emit

**文件**：`crates/mlink-cli/src/main.rs:23-25`

```rust
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::WARN)
    .init();
```

全仓搜 `tracing::(info|warn|error|debug)` → **0 命中**（仅 `main.rs:24` 引用 `Level::WARN`）。

### 3.2 P2 — `eprintln!` 数量统计（纯生产代码，不含 tests/examples）

| 文件 | `eprintln!` 次数 |
|------|------------------|
| `crates/mlink-cli/src/main.rs` | 42 |
| `crates/mlink-core/src/core/node.rs` | 17 |
| `crates/mlink-core/src/transport/peripheral.rs` | 17 |
| `crates/mlink-core/src/transport/ble.rs` | 16 |
| `crates/mlink-core/src/transport/tcp.rs` | 1 |
| `crates/mlink-core/src/lib.rs` | — |

**问题**
- `[mlink:conn]` / `[mlink:periph]` / `[mlink:debug]` 是 printf-style 调试输出，用户运行 CLI 时全都打到 stderr。见 `node.rs:205-221`、`peripheral.rs:253-267`、`ble.rs:241-341` 等。
- 无级别、无过滤、无时间戳；`--verbose` / `--quiet` 不存在。

**建议修法**：`Cargo.toml` 已经引入 `tracing = "0.1"`（`crates/mlink-core/Cargo.toml:15`），把 `eprintln!("[mlink:conn] ...")` 全部改成 `tracing::debug!`/`info!`，并加 `RUST_LOG` 或 `--verbose` 控制。

---

## 维度 4：Node 并发安全 — &mut self 方法一堆

### 4.1 P1 — `connect_peer` / `accept_incoming` / `disconnect_peer` / `stop` 都要 `&mut Node`

**文件**：`crates/mlink-core/src/core/node.rs`

| 方法 | 行号 | 签名 |
|------|------|------|
| `start` | `node.rs:156` | `&mut self` |
| `stop` | `node.rs:167` | `&mut self` |
| `connect_peer` | `node.rs:200-204` | `&mut self, transport: &mut dyn Transport, …` |
| `accept_incoming` | `node.rs:332-337` | `&mut self, conn: Box<dyn Connection>, …` |
| `disconnect_peer` | `node.rs:460` | `&mut self` |
| `attach_connection` | `node.rs:445` | `&mut self` |
| `set_room_hash` | `node.rs:132` | `&mut self` |

内部所有状态都已经 `Arc<RwLock>` / `Arc<Mutex>`（`node.rs:96-103`），**`&mut self` 完全是历史包袱**。CLI 侧只能把 Node 按可变借用传来传去，导致：
- `send_file_to_peers` 拿到的是 `&Node`（不可变），然后被迫写了 `unsafe_clone_node`（见 §2.2）。
- select 循环里 `node.connect_peer` 拿到 `&mut node` 时就不能同时 `node.peers().await` 做别的事，流程被串行化。

**建议修法**：把这些方法全部改成 `&self`（内部已经有 Arc 锁），Node 直接 `Arc<Node>` 多处共享。

### 4.2 P1 — `Node::state` 是 `Copy` 枚举但没有意义的状态机

**文件**：`crates/mlink-core/src/core/node.rs:100`、`node.rs:23-33`

`self.state` 是 **全局单值** `NodeState`，但 Node 同时支持 N 个 peer，每个 peer 有自己的 `PeerState`（`node.rs:52-58`）。顶层 `NodeState` 一会儿被 `start` 改成 `Discovering`（`node.rs:163`），一会儿被任一 `connect_peer` 改成 `Connected`（`node.rs:320`），不反映任何聚合真相。`cmd_status` 打印的 `node.state()` 因此不可信（`main.rs:1468`）。

**建议修法**：删除顶层 `state`，保留 `PeerState` 即可，或把它改成聚合视图（has_any_connected / discovering / idle）。

---

## 维度 5：多 room 支持 — RoomManager 存在但没人接进 Node

### 5.1 P1 — `RoomManager` 在 CLI 里每次新建、不持久化

**文件**：`crates/mlink-cli/src/main.rs:1185-1213`

```rust
RoomAction::Leave { code } => {
    let mut manager = RoomManager::new();
    manager.join(&code);
    manager.leave(&code);
    println!("[mlink] left room {code}");
    // ...
}
RoomAction::List => {
    let manager = RoomManager::new();
    let rooms = manager.list();  // 永远空
    // ...
}
RoomAction::Peers { code } => {
    let manager = RoomManager::new();
    let peers = manager.peers(&code);  // 永远空
    // ...
}
```

每次 `RoomManager::new()` 都是全新实例，既没 IPC 到 daemon 也没落地文件。`room list` 永远输出空，`room peers <code>` 永远输出空。

### 5.2 P1 — Node 只支持一个 room_hash（单 room 模式）

**文件**：`crates/mlink-core/src/core/node.rs:102`、`node.rs:132-138`

```rust
pub struct Node {
    // ...
    room_hash: Option<[u8; 8]>,   // <-- 单值
}
pub fn set_room_hash(&mut self, hash: Option<[u8; 8]>) { ... }
```

Scanner 侧已经支持 `Vec<[u8; 8]>`（`scanner.rs:17`、`scanner.rs:61-67`），但 Node 的握手校验只比较自己的单个 `room_hash`（`node.rs:259-277`、`node.rs:374-392`），完全无法承载多 room 场景。

**建议修法**：Node 内部改 `HashMap<RoomHash, RoomState>`，或先明确声明 mlink 只支持一个 active room、把 RoomManager 全删掉。

---

## 维度 6：重连机制 — ReconnectPolicy 写了没启用

### 6.1 P1 — `ReconnectPolicy::next_delay` 在生产代码无任何调用方

**文件**：`crates/mlink-core/src/core/reconnect.rs:47-84`

全仓搜 `.next_delay(`：
- `crates/mlink-core/src/core/reconnect.rs:47` — 定义处
- `crates/mlink-core/tests/phase5_tests.rs` — 共 12 处单测
- 生产代码 0 处

`PeerState::new()` 虽然塞了一个 `reconnect_policy: ReconnectPolicy::new()`（`node.rs:64`），但 **`PeerState.reconnect_policy` 字段之后再也没被读或写过**。

### 6.2 P1 — `Node::mark_reconnecting` 无调用方

**文件**：`crates/mlink-core/src/core/node.rs:655-661`

```rust
pub async fn mark_reconnecting(&self, peer_id: &str, attempt: u32) {
    self.set_peer_state(peer_id, NodeState::Reconnecting).await;
    let _ = self.events_tx.send(NodeEvent::Reconnecting {
        peer_id: peer_id.to_string(),
        attempt,
    });
}
```

生产代码 `.mark_reconnecting(` 命中数：0。`NodeEvent::Reconnecting` 事件永远不会被 emit。

### 6.3 P1 — CLI 侧用 per-wire_id 的土制重试代替了 Policy

**文件**：`crates/mlink-cli/src/main.rs:319-320, 405-469, 453-467`、`main.rs:717-723`、`main.rs:1022-1025`

CLI 在每个 cmd 里用 `const MAX_RETRIES: u8 = 3;` + `HashMap<String, u8>` + `random_backoff()`（`main.rs:1605-1613`）做了自己的重试。**`ReconnectPolicy` 和 `ReconnectManager` 两套轮子互不相通**。

**建议修法**：把 CLI 里的 retry state 搬进 Node（或删掉 ReconnectPolicy），二选一。

---

## 维度 7：加密 — AES/TrustStore 真没接入

### 7.1 P0 — `aes_key` 在生产代码永远是 `None`

**文件**：`crates/mlink-core/src/core/node.rs:629-635`（`set_peer_aes_key`）、`node.rs:503-507`、`node.rs:699-707`

```rust
pub async fn set_peer_aes_key(&self, peer_id: &str, key: Vec<u8>) { ... }
```

全仓搜 `.set_peer_aes_key(`：
- `crates/mlink-core/src/core/node.rs:629` — 定义处
- `crates/mlink-core/tests/phase9_tests.rs:687,688` — 测试手动注入
- 生产代码：**0 处**

握手 `Handshake.encrypt = true` （`node.rs:227-232`），但因为 `st.aes_key` 永远是 `None`（`node.rs:504`），加密分支直接落到 no-op：`body` 原文被当成加密 body 打 flag 送出（`node.rs:511, 519`），接收端 `frame.flags` 的 `encrypted` 位是 true、但 `payload` 是明文。

**实际行为：默认配置 `encrypt: true` 但零加密，且 `encrypt` flag 乱挂。**

### 7.2 P1 — TrustStore 只 CLI 可查、生产代码不写入

**文件**：`crates/mlink-cli/src/main.rs:1479-1505`、`crates/mlink-core/src/core/security.rs`

- `TrustStore::add` 在 `crates/mlink-core/src/core/security.rs` 里存在；
- CLI `trust list` / `trust remove` 可以读 / 删；
- **全代码无任何地方调用 `add`**（除 tests），也无 pairing 流程。
- `cmd_connect` 写着 `println!("TODO: verification-code prompt for first-time pairing");`（`main.rs:1440`）。

### 7.3 P1 — `derive_verification_code` / `derive_aes_key` 有实现无调用

**文件**：`crates/mlink-core/src/core/security.rs:39`、`crates/mlink-core/src/lib.rs:22`

导出到公共 API，但 handshake 里没有任何 DH / shared-secret 交换（`protocol/types.rs` 的 `Handshake` 没有 pubkey 字段），这两个函数没有上游输入。

**建议修法**
- 要么砍 `encrypt` 字段、把 `NodeConfig::encrypt: true` 默认值改 false 并公开声明「mlink 目前明文传输」；
- 要么补完 DH 交换 + 首次 pairing 流程，再把 TrustStore 接上。

---

## 维度 8：BLE 特有问题

### 8.1 P1 — peripheral.rs 里有永不退出的 keep-alive task

**文件**：`crates/mlink-core/src/transport/peripheral.rs:284-287`

```rust
tokio::spawn(async move {
    let _keep_alive = tx;
    tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
});
```

每个 `MacPeripheral::start` 都会 spawn 一个睡 ~2900 亿年的 task，只为留一份 Sender。每进一次 `cmd_serve` → `Drop` 流程再 stop，task 不会被 abort，runtime drop 才回收。CLI 内部 `cmd_serve` 在重新进入（如 room_new → cmd_serve）时会累积。

**建议修法**：改成 `std::mem::forget(tx)` 或把 tx 直接塞到 `MacPeripheral` 字段里，别开 task。

### 8.2 P2 — `wait_for_central` 死循环在同一个 `ctrl_events` 上 loop

**文件**：`crates/mlink-core/src/transport/peripheral.rs:330-352`

```rust
pub async fn wait_for_central(&self) -> Result<(String, BytesReceiver)> {
    loop {
        match self.next_event().await {
            Some(PeripheralEvent::CentralSubscribed { central_id }) => { ... }
            Some(_) => continue,
            None => { return Err(...); }
        }
    }
}
```

- 控制事件和 subscribe 事件共用一个 receiver（`ctrl_events`，`peripheral.rs:133`、`peripheral.rs:322`），**如果谁在 `next_event()` 上抢先等了，后来者会错过事件**。
- 并发多个 central 订阅时，bring-up 阶段的 `StateChanged` / `ServiceAdded` / `AdvertisingStarted` 事件已经在 `wait_for_powered_on` 等三个函数里消费掉了，这里只会看到 `CentralSubscribed`，但这个串行 select 仍然是潜在竞态点。

### 8.3 P2 — BleTransport::connect 的日志量巨大

**文件**：`crates/mlink-core/src/transport/ble.rs:288-351`

一次 connect 打 ~11 行 `[mlink:conn]` 和 `[mlink:debug]`。见 §3.2。

### 8.4 P2 — `IDENTITY_UUID_MARKER` 与 MLINK_SERVICE_UUID 耦合在 ble.rs

**文件**：`crates/mlink-core/src/transport/ble.rs:40-63`、`crates/mlink-core/src/transport/ble.rs:86-94`（decode_identity_uuid）

magic bytes `[0x00, 0x00, 0xFF, 0xBB]` 只在 ble 侧出现，但 scanner 侧又做了跟 length 相关的 room 匹配（`scanner.rs:80-91`），导致 BLE 和 TCP 的 "metadata" 格式走两套不同的判别逻辑。见 §1.2。

### 8.5 P1 — btleplug 的 `local_name` / 自定义广播名在 macOS 不可用（已知坑，未闭环）

**文件**：`PITFALLS.md:42-58`、`crates/mlink-core/src/transport/peripheral.rs:158-161`

```rust
let advertised_name = match room_hash {
    Some(h) => format!("{local_name}#{}", hex_encode(&h)),
    None => local_name.clone(),
};
```

CBPeripheralManager 会忽略自定义 name（见 PITFALLS 坑 3），但代码仍然把 room_hash 拼到 name 后面（`peripheral.rs:158-161`），这段名字只有非 macOS 平台能用，在 macOS 上完全浪费。目前线上方案已经改走 identity UUID（`ble.rs:166-171`、`peripheral.rs:863-905`）—— 拼名字的逻辑是死代码。

**建议修法**：删掉 `advertised_name` 里的 `#<hex>` 逻辑。

---

## 维度 9：TCP 特有问题

### 9.1 P1 — `discover_connect_roundtrip` 测试本身就承认 mDNS 可能 flaky

**文件**：`crates/mlink-core/src/transport/tcp.rs:671-710`

```rust
let Some(peer) = peers.into_iter().find(...) else {
    eprintln!("[tcp-test] mDNS browse returned no match; skipping roundtrip assertion");
    listener_task.abort();
    return;
};
```

CI 环境或本地 multicast 被屏蔽时，该测试 **悄悄跳过断言而返回 pass**。没有 `ignored` 标记，也没有条件 skip metric，无法区分"通过"和"跳过"。

### 9.2 P2 — `browse_once` 依赖 `spawn_blocking`，SIGINT 被挡 3s

**文件**：`crates/mlink-core/src/transport/tcp.rs:126-133`、`crates/mlink-cli/src/main.rs:39-60`

`discover()` 全程跑在 `tokio::task::spawn_blocking(move || browse_once(...))`，browse 循环用 `receiver.recv_timeout(remaining)` 一次最多 3s（`tcp.rs:275-349`）。runtime drop 前无法 cancel，这正是 `main.rs:37-59` 要写 `libc::_exit(0)` 的原因。

**建议修法**：改用 `mdns-sd` 的异步 stream API，或在 `discover_duration` 很小时走 `tokio::time::timeout` 包裹 + task abort。

### 9.3 P2 — `TcpConnection` read/write 没有帧边界保护

**文件**：`crates/mlink-core/src/transport/tcp.rs:188-216`

`write` 发 4-byte big-endian 长度 + payload；单个 peer 不校验 `len` 是否合理。对端发 `u32::MAX` 的 len 直接 `vec![0u8; len]` 会 OOM。

**建议修法**：加 `const MAX_FRAME: usize = 1 << 20;` 之类的上限。

### 9.4 P2 — `listen()` 首次 bind 成功后 `self.port` 才被回填

**文件**：`crates/mlink-core/src/transport/tcp.rs:147-165`

```rust
if self.listener.is_none() {
    let listener = TcpListener::bind(&bind_addr).await?;
    let local = listener.local_addr()?;
    self.port = local.port();
    // ...
}
```

调用者在 `listen()` 之前读 `.port()` 会拿到 0；语义混乱。

---

## 维度 10：半成品 / 占位符功能

### 10.1 P0 — `send <code> --file <path>` 一定 panic

见 §2.2：`unsafe_clone_node` → `unimplemented!`。

### 10.2 P1 — `room list` / `room peers` 永远输出空列表

见 §5.1。

### 10.3 P1 — `cmd_status` 不反映真实状态

**文件**：`crates/mlink-cli/src/main.rs:1466-1477`

```rust
async fn cmd_status() -> Result<(), MlinkError> {
    let node = build_node().await?;
    println!("state: {:?}", node.state());
    // 新建的 Node state 永远是 Idle
    // peers() 永远是空
    // ...
    println!("note: CLI invocations are stateless — run `mlink serve` in a long-lived process to see peers here");
}
```

build_node 每次都新建 Node，没 IPC 连接到长跑 daemon（`ipc.rs` 虽有但未接入 `cmd_status`）。整个命令只打印一段 note 说"结果不准"。

### 10.4 P1 — `cmd_connect` 有 TODO 留在用户界面

**文件**：`crates/mlink-cli/src/main.rs:1440`

```rust
println!("TODO: verification-code prompt for first-time pairing");
```

用户会看到这行 `TODO:`。

### 10.5 P1 — `cmd_listen` 只是 `cmd_serve(None, kind)` 的别名

**文件**：`crates/mlink-cli/src/main.rs:1359-1364`

```rust
async fn cmd_listen(kind: TransportKind) -> Result<(), MlinkError> {
    println!("[mlink] listen: running serve loop, Ctrl+C to quit");
    cmd_serve(None, kind).await
}
```

没有独立语义 —— 可能可以直接删。

### 10.6 P2 — `RoomAction::Leave` 走假流程

**文件**：`crates/mlink-cli/src/main.rs:1180-1193`

```rust
RoomAction::Leave { code } => {
    let mut manager = RoomManager::new();
    manager.join(&code);          // 先假装加入
    manager.leave(&code);         // 再假装离开
    println!("[mlink] left room {code}");
}
```

用户总是看到 "left room"，不管之前是否 join 过。

---

## 维度 11：依赖健康度

**文件**：`crates/mlink-core/Cargo.toml`

| crate | 声明版本 | 说明 |
|-------|----------|------|
| `tokio` | `"1"` | full features，OK |
| `btleplug` | `"0.11"` | 活跃维护，0.11 是 2024 版本；目前无已知公开 CVE |
| `rmp-serde` | `"1"` | OK |
| `zstd` | `"0.13"` | OK |
| `ring` | `"0.17"` | OK |
| `mdns-sd` | `"0.19"` | 相对活跃，SIGINT 阻塞问题见 §9.2 |
| `thiserror` | `"2"` | 最新主版本 |
| `objc2` | `"0.5"` | `objc2` 已出 `0.6.x`；当前 0.5 仍维护 |
| `objc2-foundation` / `objc2-core-bluetooth` | `"0.2"` | 跟 objc2 0.5 配套 |

**问题**
- 所有版本都写成主版本通配（如 `"1"`、`"0.11"`），没有 `Cargo.lock` 外加 minimum-version 下限约束；
- `objc2 = "0.5"` 升 0.6 是一次 breaking change，需要评估；
- **依赖本身没有看到 CVE**（作为离线扫描，需正式 `cargo audit` 核实）。

**建议修法**：跑一次 `cargo audit`；把关键 crate 固到 patch 版本；评估 objc2 0.5 → 0.6 升级。

---

## 附录 A：问题按严重度聚合

### P0（3 条）
1. §7.1 加密默认 on，实际零加密，flag 乱挂。
2. §10.1 `send --file` 必 panic。
3. §1.1 / §2.1 严格讲不算功能阻塞，但"三命令重复 + trait 捆绑"把整个架构卡死在不可演进状态，视读者判断。

### P1（15 条）
§1.1、§1.3、§2.2、§4.1、§4.2、§5.1、§5.2、§6.1、§6.2、§6.3、§7.2、§7.3、§8.1、§8.5、§9.1、§10.2、§10.3、§10.4、§10.5

### P2（10 条）
§1.2、§2.1（重复代码本身归 P2）、§3.1、§3.2、§8.2、§8.3、§8.4、§9.2、§9.3、§9.4、§10.6

### P3（1 条）
§2.3

---

## 附录 B：下一步建议优先级（非本审计要求，仅参考）

1. **先修 P0 加密假跑** —— 把 `NodeConfig::encrypt` 默认值改 false，并把 `encrypted` flag 的编码修好，防止对端误判。
2. **砍死代码** —— ReconnectPolicy / RoomManager / ipc.rs / unsafe_clone_node / _reserved_mutex 一次性清理。
3. **Node &mut → &self** —— 解除 CLI 被 `&mut node` 串行化的锁，顺带把 status 改成 IPC 真实查询。
4. **三大 cmd 提炼 SessionRuntime** —— 把 ~700 行重复降到 ~200。
5. **日志切 tracing** —— eprintln → tracing::debug，默认 `RUST_LOG=warn`。
