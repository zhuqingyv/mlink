# mlink Architecture

> mlink 不关心上层跑什么业务。它只做一件事：在设备之间建立可靠的多对多连接。

## Overview

mlink 是一个**连接层**。像 TCP 一样，它是哑管道——数据进来，数据出去，不做业务判断。

**它不是蓝牙工具。** 核心是连接处理协议（帧、序列化、压缩、流控、多对多），传输层可插拔，BLE 只是第一个 transport 实现。

```
┌─────────────────────────────────────────────────────┐
│                    mlink-core                        │
│                                                     │
│   ┌──────────┐ ┌──────────┐ ┌────────────────────┐ │
│   │ Protocol │ │   API    │ │   Node (p2p mesh)  │ │
│   │ 帧/压缩   │ │ 消息/流   │ │ 多对多连接管理       │ │
│   └────┬─────┘ └────┬─────┘ └──────────┬─────────┘ │
│        └────────────┴──────────────────┘           │
│                        │                             │
│              ┌─────────┴─────────┐                   │
│              │  Transport Trait  │                   │
│              └──┬─────┬─────┬───┘                   │
│                 │     │     │                        │
│              ┌──┴┐ ┌──┴┐ ┌──┴──┐                    │
│              │BLE│ │IPC│ │Mock │                    │
│              └───┘ └───┘ └─────┘                    │
└─────────────────────────────────────────────────────┘

未来 transport 可以是:
  - BLE (当前)
  - TCP/WebSocket (同网络时)
  - USB/Thunderbolt (物理直连)
  - QUIC (广域网)
  - WiFi Direct (点对点)
```

## Design Principles

1. **哑管道** — 不做业务判断，不关心 payload 内容，只负责送达
2. **协议与传输分离** — core 不知道底层是蓝牙还是 TCP
3. **数据效率优先** — 二进制序列化 + 流式压缩，榨干每一字节
4. **无队头阻塞** — 优先级调度 + 压缩降量，高优消息不等
5. **多对多对等** — 每对设备独立连接，没有中心节点

---

## 1. 发现

节点启动后自动扫描附近 mlink 设备。

```
[IDLE] ──start()──► [DISCOVERING] ──发现peer──► [DISCOVERED] ──► [CONNECTING]
                         │
                     同时进行:
                     - 主动扫描广播包
                     - 自身广播（被动被发现）
```

BLE transport 的发现机制：

```
每台设备启动 → 同时广播 + 扫描 (持续)

广播包 manufacturer data:
  - app_uuid: 持久化的节点实例 ID (首次启动生成，稳定)
  - timestamp: 启动时间戳
  - capabilities: 编码的能力信息

注意: 不用 BLE 硬件地址作为身份 (macOS 每 ~15min 轮转)，
     而是用 app_uuid 作为稳定身份标识。
```

发现结果通过回调通知应用层：

```rust
node.on_peer_discovered(|peer| async { /* peer.id, peer.rssi, ... */ });
```

---

## 2. 连接

发现后自动建立连接，并通过 app_uuid 协商角色。

### 2.1 角色协商

BLE 要求一对连接必须分出 Central / Peripheral。mlink 用 app_uuid 确定性协商：

```
双方扫到对方 → 比较 app_uuid 字典序
  小者 → Central (主动发起连接)
  大者 → Peripheral (被动接受连接)

这是确定性的：任何两台设备之间角色总是一致，不会出现同时发起冲突。
```

注意：**角色只是 BLE 的传输层概念**。上层协议对等，任何一方都能主动发消息。

### 2.2 HANDSHAKE

连接建立后立即交换 HANDSHAKE，协商能力：

```
{
  app_uuid: "...",
  version: 1,
  mtu: 512,
  compress: true,
  last_seq: 0,           // 重连时填上次 seq
  resume_streams: [],    // 重连时填未完成的流
}
```

HANDSHAKE 完成后进入 CONNECTED 状态，开始正常通信。

---

## 3. 协议

所有数据收发都经过这条管道，与 transport 无关。

```
发送端:                                        接收端:
  应用数据 (bytes)                              应用数据
       │                                            ▲
       ▼                                            │
  msgpack 序列化 (rmp-serde)                     msgpack 反序列化
       │  结构化 → 紧凑二进制                         ▲
       ▼                                            │
  zstd 流式压缩 (level 3)                        zstd 流式解压
       │  边压边发，不等完整数据                       ▲  边收边解
       ▼                                            │
  帧封装 (8B header)                             帧解析
       │                                            ▲
       ▼                                            │
  优先级队列                                     接收缓冲
       │  P0 > P1 > P2 > P3                        ▲
       ▼                                            │
  Transport (BLE / IPC / TCP / ...)             Transport
```

### 3.1 消息帧格式

```
┌──────┬──────┬───────┬────────┬─────────┬──────────┐
│ MAGIC│ VER  │ FLAGS │ SEQ    │ LENGTH  │ PAYLOAD  │
│ 2B   │ 1B   │ 1B    │ 2B     │ 2B      │ 0~nB    │
└──────┴──────┴───────┴────────┴─────────┴──────────┘
Total header: 8B

MAGIC:   0x4D4C ("ML")
VER:     协议版本 (0x01)
FLAGS:   [compressed:1][type:7]
           bit7 = 压缩标志 (1=已压缩)
           bit0-6 = 消息类型 (0~127)
SEQ:     uint16, 回绕取模, HANDSHAKE 后交换 last_seq
LENGTH:  uint16, 单帧 payload 长度
PAYLOAD: msgpack 序列化 + 可选 zstd 压缩后的数据
```

### 3.2 消息类型

| TYPE | 名称 | 优先级 | 说明 |
|------|------|--------|------|
| 0x01 | HANDSHAKE | P0 | 交换能力 (MTU/压缩/版本/resume_streams) |
| 0x02 | HEARTBEAT | P0 | 心跳 (15s 间隔，3 次超时 = 45s 判定断线) |
| 0x03 | ACK | P0 | 确认收到某 SEQ |
| 0x04 | CTRL | P0 | 控制指令 (PAUSE/RESUME/MTU_UPDATE) |
| 0x10 | MESSAGE | P2 | 普通消息 |
| 0x11 | STREAM_START | P2 | 流开始: stream_id + total_chunks + total_size + checksum_algo |
| 0x12 | STREAM_CHUNK | P3 | 数据块: stream_id + chunk_index + data |
| 0x13 | STREAM_END | P2 | 流结束: stream_id + checksum |
| 0x14 | STREAM_ACK | P1 | 流确认: stream_id + received_bitmap |
| 0x20 | REQUEST | P1 | RPC 请求: request_id + method + timeout_ms |
| 0x21 | RESPONSE | P1 | RPC 响应: request_id + status + data |
| 0x22 | RESPONSE_STREAM | P1 | RPC 流式响应: request_id + stream_id |
| 0x30 | SUBSCRIBE | P2 | 订阅: topic |
| 0x31 | PUBLISH | P2 | 发布: topic + data |
| 0x32 | UNSUBSCRIBE | P2 | 取消订阅: topic |
| 0xFF | ERROR | P1 | 错误: code + message |

### 3.3 序列化: msgpack

```
为什么不用 JSON:
  - {"type":"query","content":"hello"} = 38 bytes (JSON)
  - 同样内容 msgpack = 22 bytes (省 42%)

为什么不用 protobuf:
  - 需要 schema 文件，消息结构多变时维护成本高
  - msgpack schema-free，任意结构直接序列化

crate: rmp-serde
```

### 3.4 流式压缩: zstd streaming

```
所有消息默认压缩 (FLAGS bit7 = 1):
  - 小消息 (<256B): 不压缩，开销不值
  - 普通消息 (256B~1KB): zstd 压缩，压缩比 40-60%
  - 大数据 (>1KB): zstd 流式压缩，压缩比 80-92%

流式模式关键:
  不是: 收完 → 压全部 → 发全部 (延迟高)
  而是: 来一块 → 压一块 → 发一块 → 收一块 → 解一块 → 处理 (延迟最低)

crate: zstd (streaming encoder/decoder)
```

### 3.5 效率对比

| 数据 | 原始 JSON | msgpack | + zstd | 总压缩比 |
|------|----------|---------|--------|---------|
| RPC 请求 | 500B | 200B | 120B | **76%** |
| 小消息 | 1KB | 600B | 250B | **75%** |
| 结构化数据 | 10KB | 5KB | 1KB | **90%** |
| 大段文本 | 100KB | 55KB | 8KB | **92%** |

### 3.6 优先级调度 (无队头阻塞)

```
发送队列按优先级分 4 级:
  P0: 控制 (心跳/背压/ACK)      — 立即发，插队
  P1: 交互 (RPC/流ACK)          — 下一个时间片
  P2: 普通 (消息/订阅/流开始结束) — 正常排队
  P3: 批量 (stream chunk)       — 填充空闲带宽

调度器每个时间片:
  1. 先发所有 P0
  2. 发 1 个 P1
  3. 发 1 个 P2
  4. 发 N 个 P3 (填满剩余带宽)
  
压缩后 100KB 数据 → 8KB → 53ms (BLE)
53ms 内调度器有 ~17 个时间片，P1 消息最多等 3ms。
```

### 3.7 错误码

| code | 名称 | 说明 |
|------|------|------|
| 0x01 | TIMEOUT | RPC 超时 |
| 0x02 | PEER_GONE | 对端断开 |
| 0x03 | HANDLER_ERROR | 处理函数异常 |
| 0x04 | PAYLOAD_TOO_LARGE | 超过最大 payload |
| 0x05 | UNKNOWN_METHOD | 未注册的 RPC 方法 |
| 0x06 | STREAM_FAILED | 流传输校验失败 |
| 0x07 | BACKPRESSURE | 接收端过载 |

---

## 4. 多对多

mlink 支持多对多，但不是 Star 拓扑，也不是 Hub 转发。

```
每个节点独立维护若干连接:

  Node A ──────── Node B
    │  ╲          ╱  │
    │   ╲        ╱   │
    │    ╲      ╱    │
    │     ╲    ╱     │
    │      ╲  ╱      │
    │       ╲╱       │
    │       ╱╲       │
    │      ╱  ╲      │
    │     ╱    ╲     │
    │    ╱      ╲    │
    │   ╱        ╲   │
    │  ╱          ╲  │
  Node C ──────── Node D

  每对设备独立建立连接，没有中心节点，没有转发。
```

### 4.1 并发角色

在 BLE 上，每个节点可以**同时**扮演：
- Central（主动连出去）
- Peripheral（被动被连入）

```
Node A 的状态:
  - Central  连接 → Node B   (A 的 app_uuid < B 的 app_uuid)
  - Central  连接 → Node C   (A 的 app_uuid < C 的 app_uuid)
  - Peripheral 接受 ← Node D (A 的 app_uuid > D 的 app_uuid)

在 A 的眼里这三个都是 peer，上层 API 没区别。
```

### 4.2 每条连接独立

```
关键特性:
  - 每对 (peer_a, peer_b) 是一条独立的连接
  - 协议状态、SEQ、窗口、压缩都独立
  - 一条连接断了不影响其他连接
  - 不需要全局路由表，不需要选举

节点只知道它直接连着谁。mlink 不做跨节点转发。
如果 A 想和 C 通信，A 必须和 C 直接连上。
```

### 4.3 API 层面

peer_id 就是连接建立时拿到的句柄：

```rust
// 直接连接 + 获取 peer_id
let peer = node.connect(discovered_peer).await?;
node.send(&peer.id, b"hello").await?;

// 列出所有当前连着的 peer
for peer in node.peers().await {
    node.send(&peer.id, b"ping").await?;
}
```

---

## 5. 流式传输

大数据拆成 chunk，走滑动窗口 + bitmap ACK。

### 5.1 流程

```
Sender                              Receiver
  │                                     │
  │  STREAM_START {id, total, size}     │
  │ ──────────────────────────────────► │
  │                                     │
  │  STREAM_CHUNK {id, idx=0, data}     │  已 msgpack + zstd 流式压缩
  │ ──────────────────────────────────► │
  │  STREAM_CHUNK {id, idx=1, data}     │
  │ ──────────────────────────────────► │
  │  ...连续发 W 个                      │
  │                                     │
  │  STREAM_ACK {id, bitmap}            │  每 W 块 ACK
  │ ◄────────────────────────────────── │
  │                                     │
  │  (重传缺失 chunks)                   │
  │ ──────────────────────────────────► │
  │                                     │
  │  STREAM_END {id, checksum}          │
  │ ──────────────────────────────────► │
  │                                     │
  │  ACK                                │
  │ ◄────────────────────────────────── │
```

### 5.2 自适应滑动窗口

```
初始 W = 8
  连续 3 轮无丢包 → W = min(W × 2, 32)
  出现丢包 → W = max(W / 2, 4)

chunk_size = MTU - ATT_HEADER - MLINK_HEADER (动态)
```

### 5.3 stream_id 分配

```
双方并发开流，为避免 id 冲突，按 BLE 角色分配奇偶：
  Central   偶数: 0, 2, 4, ...
  Peripheral 奇数: 1, 3, 5, ...
```

### 5.4 超时分级

```
timeout = max(60s, total_size / (50KB) × 60s)
每收到 chunk 重置计时器
```

---

## 6. 可靠性

### 6.1 断线检测 (双层)

```
两层检测，取先触发的:
  1. Transport 层事件 (BLE supervision timeout ~6s / TCP RST)
  2. 应用层心跳 (HEARTBEAT 15s 间隔，3 次超时 = 45s)
```

### 6.2 重连策略

```
前台 (指数退避):
  立即 → 1s → 2s → 4s → 8s → ... → 60s (上限)
  5 分钟失败 → 降级后台

后台 (低频探测):
  每 120s 尝试一次
  持续 30 分钟
  仍失败 → IDLE，触发 peer:lost
```

### 6.3 断点续传

```
重连 HANDSHAKE 携带:
  {
    resume_streams: [
      { stream_id: 4, received_bitmap: "0xFF3F..." }
    ],
    last_seq: 42180
  }

发送端:
  有缓存 → 从 bitmap 缺失处续传
  无缓存 → STREAM_FAILED，应用层决定重发

SEQ 回绕:
  新连接从 last_seq + 1 开始
  16bit 滑动窗口去重
```

### 6.4 生命周期状态机

```
[IDLE] → [DISCOVERING] → [DISCOVERED] → [CONNECTING] → [CONNECTED] → [DISCONNECTED]
  │            │               │              │              │  ▲            │
  │       启动后自动        发现 peer      连接建立中     正常通信 │          断线检测
  │                                                │         ┌──┘            │
  │                                                ▼         │               ▼
  │                                          [STREAMING] ────┘        [RECONNECTING]
  │                                                                         │
  │                                                                 成功 → CONNECTED
  │                                                                         │
  └──────────────────── 前台 5min / 后台 30min 失败 ──────────────── [IDLE]
```

状态转换：

| 当前状态 | 事件 | 下一状态 |
|---------|------|---------|
| IDLE | start() | DISCOVERING |
| DISCOVERING | 发现 peer | DISCOVERED |
| DISCOVERING | 30s 无结果 | IDLE (timeout) |
| DISCOVERED | 角色协商完成 | CONNECTING |
| CONNECTING | 连接成功 + HANDSHAKE 完成 | CONNECTED |
| CONNECTING | 10s 失败 | DISCOVERING (重试) |
| CONNECTED | 收发消息 | CONNECTED |
| CONNECTED | 开始流式传输 | STREAMING |
| CONNECTED | 断线 | DISCONNECTED |
| STREAMING | 传输完成 | CONNECTED |
| STREAMING | 断线 | DISCONNECTED (记录 stream 进度) |
| DISCONNECTED | 触发重连 | RECONNECTING |
| RECONNECTING | 成功 | CONNECTED (恢复未完成 stream) |
| RECONNECTING | 超时放弃 | IDLE |

注意：每条连接（每个 peer）独立维护自己的状态机。

---

## 7. Transport 可插拔

### 7.1 Transport Trait

```rust
#[async_trait]
pub trait Transport: Send + Sync {
    /// 传输标识
    fn id(&self) -> &str;
    
    /// 传输能力
    fn capabilities(&self) -> TransportCapabilities;
    
    /// 发现附近设备
    async fn discover(&self) -> Result<Vec<DiscoveredPeer>>;
    
    /// 主动连接
    async fn connect(&self, peer: &DiscoveredPeer) -> Result<Connection>;
    
    /// 监听连接 (被动)
    async fn listen(&self) -> Result<ConnectionStream>;
    
    /// 最大传输单元
    fn mtu(&self) -> usize;
}

pub struct TransportCapabilities {
    pub max_peers: usize,         // 最大并发连接
    pub throughput_bps: u64,      // 理论吞吐 (bits/s)
    pub latency_ms: u32,          // 典型延迟
    pub reliable: bool,           // 是否可靠传输
    pub bidirectional: bool,      // 是否双向
}
```

### 7.2 已规划 Transport

| Transport | 场景 | 吞吐 | 延迟 | 状态 |
|-----------|------|------|------|------|
| **BLE** | 网络隔离跨设备 | ~1.2 Mbps | 5-30ms | Phase 1 |
| **IPC** | 本机进程间 (Unix socket / named pipe) | 无限 | <1ms | Phase 1 |
| **Mock** | 单测 | — | — | Phase 1 |
| TCP | 同网络跨设备 | ~1 Gbps | <1ms | Phase 2 |
| USB | 物理直连 | ~5 Gbps | <1ms | Future |
| QUIC | 广域网 | ~100 Mbps | 10-50ms | Future |

### 7.3 BLE 实现 (btleplug)

Service 定义：

```
Service UUID: 0000FFAA-0000-1000-8000-00805F9B34FB (mlink)

Characteristics:
  ├── TX (write):    0000FFAB-...  → Central 写，Peripheral 读
  ├── RX (notify):   0000FFAC-...  → Peripheral 写，Central 订阅通知
  └── CTRL (r/w):    0000FFAD-...  → 控制通道
```

MTU 与分片：

```
连接后请求最大 MTU:
  macOS: ~512B | Linux: ~517B | Windows: ~527B
  
chunk_size = MTU - 3B (ATT) - 8B (mlink header) = 动态计算

MTU 协商失败 → 回落到 ATT 默认 23B → 可用 12B/chunk (极慢，日志警告)
```

---

## 8. 安全 (协议层内置)

安全是协议层的事，不是应用层的事。上层不需要关心加密和认证。

### 8.1 首次连接：验证码确认

```
Device A 扫到 Device B:

1. 双方通过 ECDH (X25519) 协商临时密钥
2. 从共享密钥派生 6 位验证码

   Device A 屏幕: 确认连接? 验证码 [847291]
   Device B 屏幕: 确认连接? 验证码 [847291]

3. 双方用户确认数字一致 → 连接建立
4. 验证码不一致 → 中间人攻击，拒绝连接

类似蓝牙配对的 Numeric Comparison，但在 mlink 协议层实现，
不依赖 BLE 底层配对（跨 transport 通用）。
```

### 8.2 信任持久化

```
首次验证通过后:
  - 双方交换各自的 app_uuid + 公钥
  - 存入本地信任列表: ~/.mlink/trusted_peers.json
  
后续重连:
  - HANDSHAKE 阶段校验 app_uuid + 公钥签名
  - 匹配信任列表 → 自动连接，不再弹验证码
  - 不匹配 → 当作新设备，重新走验证码流程

信任可撤销:
  mlink untrust <peer-id>    # CLI 移除信任
```

### 8.3 通信加密 (默认开启)

```
HANDSHAKE 完成后所有 payload 默认 AES-256-GCM 加密:

  发送: plaintext → AES-256-GCM encrypt → 密文 (FLAGS bit6 = 1)
  接收: 密文 → AES-256-GCM decrypt → plaintext

密钥: ECDH 共享密钥经 HKDF 派生
每条消息独立 nonce (SEQ 作为 nonce 的一部分，防重放)

可选关闭 (调试/测试场景):
  Node::new(NodeConfig { encrypt: false, .. })

crate: ring (X25519 + AES-256-GCM + HKDF)
```

### 8.4 FLAGS 更新

```
FLAGS byte:
  bit7 = 压缩标志 (zstd)
  bit6 = 加密标志 (AES-256-GCM)
  bit0-5 = 消息类型 (0~63)
```

---

## 9. API 设计

### 9.1 Rust Core API

```rust
use mlink::{Node, NodeConfig, Peer, MlinkError};

// ─── 创建节点 ───
let node = Node::new(NodeConfig {
    name: "node-a".into(),
    // encrypt: true,          // 默认开启，可选关闭
    // trust_store: None,      // 默认 ~/.mlink/trusted_peers.json
}).await?;

node.start().await?;   // 开始扫描 + 广播

// ─── 生命周期回调 ───
node.on_peer_discovered(|peer: &Peer| async { });
node.on_peer_connected(|peer: &Peer| async { });
node.on_peer_disconnected(|peer: &Peer| async { });
node.on_peer_lost(|peer: &Peer| async { });
node.on_reconnecting(|peer: &Peer, attempt: u32| async { });

// ─── Peer 管理 ───
let peers = node.peers().await;             // 当前连着的所有 peer
let peer = node.get_peer(&peer_id).await?;

// ─── 消息 ───
node.send(&peer.id, b"hello").await?;
node.broadcast(b"data").await?;             // 广播到所有已连接 peer
node.on_message(|msg, from| async { });

// ─── 流式传输 ───
let mut tx = node.create_stream(&peer.id).await?;
tx.write(chunk).await?;
tx.finish().await?;

node.on_stream(|mut rx, from| async {
    while let Some((chunk, progress)) = rx.next().await {
        // progress.percent, progress.received, progress.total
    }
});

// ─── RPC ───
node.handle("query", |data: &[u8], ctx| async {
    Ok(process(data).await?)
});

node.handle_stream("export", |data: &[u8], mut tx| async {
    for chunk in produce(data) {
        tx.send(chunk).await?;
    }
    Ok(())
});

let result = node.request(&peer.id, "query", b"hello", RequestOpts {
    timeout_ms: 5000,
    cancel: token.clone(),
}).await?;

// ─── 订阅/发布 ───
node.subscribe(&peer.id, "topic", |data, from| async { }).await?;
node.publish("topic", b"data").await?;
node.unsubscribe(&peer.id, "topic").await?;

// ─── 关闭 ───
node.close().await?;
```

### 9.2 Node.js Binding (napi-rs)

```typescript
import { createNode } from '@mlink/node'
import type { Peer, MlinkError } from '@mlink/node'

const node = await createNode({ name: 'node-a' })
await node.start()

node.on('peer:connected', (peer: Peer) => {})
await node.send(peer.id, Buffer.from('hello'))
const result = await node.request(peer.id, 'query', data, { timeout: 5000 })
await node.close()
```

### 9.3 错误类型

```rust
#[derive(Debug, thiserror::Error)]
pub enum MlinkError {
    Timeout,
    PeerGone { peer_id: String },
    HandlerError(String),
    PayloadTooLarge { size: usize, max: usize },
    UnknownMethod(String),
    StreamFailed { stream_id: u16 },
    Backpressure,
    Ble(#[from] btleplug::Error),
    Io(#[from] std::io::Error),
}
```

---

## 10. CLI

```bash
# 设备发现
mlink scan                        # 扫描附近 mlink 设备

# 连接管理
mlink connect <peer-id>           # 连接 (首次弹验证码)
mlink status                      # 查看当前连接状态

# 信任管理
mlink trust list                  # 查看已信任设备
mlink trust remove <peer-id>     # 撤销信任

# 诊断
mlink ping <peer-id>              # 测试延迟
mlink doctor                      # 诊断蓝牙/权限/环境

# 消息
mlink send <peer-id> <msg>        # 发送消息
```

---

## 11. 目录结构

```
mlink/
├── crates/
│   ├── mlink-core/                — 核心协议库
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── core/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── node.rs        — 节点主体 + 生命周期状态机
│   │   │   │   ├── scanner.rs     — 设备发现
│   │   │   │   ├── connection.rs  — 连接管理 + 角色协商
│   │   │   │   ├── reconnect.rs   — 断线重连 + 断点续传
│   │   │   │   └── peer.rs        — Peer 管理
│   │   │   ├── protocol/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── frame.rs       — 8B 帧编解码
│   │   │   │   ├── stream.rs      — 流式传输 (分片/窗口/bitmap)
│   │   │   │   ├── codec.rs       — msgpack 序列化/反序列化
│   │   │   │   ├── compress.rs    — zstd 流式压缩/解压
│   │   │   │   ├── errors.rs      — MlinkError
│   │   │   │   └── types.rs       — 常量 + 类型
│   │   │   ├── transport/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── trait.rs       — Transport trait
│   │   │   │   ├── ble.rs         — BLE (btleplug)
│   │   │   │   ├── ipc.rs         — Unix socket / named pipe
│   │   │   │   └── mock.rs        — Mock (单测)
│   │   │   └── api/
│   │   │       ├── mod.rs
│   │   │       ├── message.rs     — 消息收发
│   │   │       ├── stream.rs      — 流式 API
│   │   │       ├── rpc.rs         — RPC (含流式响应)
│   │   │       └── pubsub.rs      — 订阅/发布
│   │   ├── tests/
│   │   └── Cargo.toml
│   │
│   └── mlink-cli/                 — CLI binary
│       ├── src/
│       │   └── main.rs            — clap CLI
│       └── Cargo.toml
│
├── bindings/
│   └── node/                      — Node.js binding
│       ├── src/lib.rs             — napi-rs 导出
│       ├── index.d.ts
│       ├── package.json
│       └── Cargo.toml
│
├── examples/
│   ├── two_nodes.rs               — 两节点点对点通信
│   ├── many_to_many.rs            — 多节点多对多连接
│   ├── stream_transfer.rs         — 大数据流式传输
│   └── rpc_demo.rs                — RPC 调用示例
│
├── Cargo.toml                     — workspace
├── ARCHITECTURE.md
└── README.md
```

---

## 12. Tech Stack

```
语言:       Rust
异步运行时:  tokio
BLE:        btleplug
序列化:      rmp-serde (msgpack)
压缩:       zstd (streaming)
加密:       ring (AES-256-GCM)
CLI:        clap
Node 绑定:  napi-rs
错误处理:   thiserror
日志:       tracing
```
