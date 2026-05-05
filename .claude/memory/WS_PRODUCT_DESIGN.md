# mlink WebSocket 接口 —— 产品设计稿

**作者**：产品架构师（team member）
**日期**：2026-04-20
**目标**：从上层应用开发者体验出发，设计 mlink WS 接口最小可用版本

---

## 一、核心产品立场

### 1.1 谁在用它
- **主用户**：mteam agent / 知识库 / 文件同步这类"跨设备协作"应用
- **使用形态**：每个 app 在本机起一条 WS 到 `ws://127.0.0.1:<port>/ws`，订阅若干 room，收发 payload
- **不关心什么**：底下走 BLE 还是 TCP、对端具体是谁、消息怎么加密压缩 —— 这些是 mlink 的事

### 1.2 一条铁律
**mlink 不解析 payload**。payload 是什么格式、怎么路由到 agent 内部、什么算"同一条消息"，都是上层的事。
mlink 只保证："你在 room X 发的 bytes，room X 里其他机器都收到同一份 bytes。"

---

## 二、开发者体验（DX）：五行接入

目标：一个 mteam agent 作者打开 IDE，5 分钟内能双机通信。

```js
const ws = new WebSocket('ws://127.0.0.1:7421/ws')
ws.onopen = () => ws.send(JSON.stringify({ op: 'join', room: '567892' }))
ws.onmessage = (e) => {
  const msg = JSON.parse(e.data)
  if (msg.type === 'message') handle(msg.room, msg.from, msg.payload)
}
ws.send(JSON.stringify({ op: 'send', room: '567892', payload: { text: 'hi' } }))
```

**这 5 行要能跑通**。如果跑不通就是产品失败。任何设计决策都要回到这 5 行验证。

---

## 三、WS 消息协议（产品最小集）

### 3.1 原则
- **一个连接能订阅多 room**（首期就支持，不是 V2 的事）
- **所有消息都是 JSON**（不是二进制帧）—— 浏览器 devtools 能直接看到文本
- **payload 透传**（下面 3.3 展开讨论）
- **消息类型极少**（能少一个就少一个）
- **客户端上行叫 `op`，服务端下行叫 `type`** —— 语义不同，名字就不同，避免混淆

### 3.2 客户端 → 服务端（上行 `op`）

只有 4 个 op：

| op | 必填字段 | 说明 |
|---|---|---|
| `join` | `room` | 订阅 room。可重复调用订阅多个 room。`room` 是 6 位数字码 |
| `leave` | `room` | 取消订阅 |
| `send` | `room`, `payload`, `to?` | 发送消息。`to` 缺省=广播房间；`to` 是对端 `app_uuid`=点对点 |
| `ping` | — | 应用层心跳（WS 自带 ping 帧浏览器拿不到，应用层补） |

**可选字段** `req_id`：客户端生成的字符串，服务端原样带回在 `ack` / `error` 里，用于关联请求与响应。**不是 op 的一部分，是通用字段**。

### 3.3 服务端 → 客户端（下行 `type`）

只有 6 个 type：

| type | 字段 | 触发时机 |
|---|---|---|
| `welcome` | `client_id`, `app_uuid`, `version` | WS 连接建立后第一条，让客户端知道 daemon 是谁 |
| `ack` | `req_id`, `op`, `room?` | 上一条 op 成功。只在有 `req_id` 时发 |
| `error` | `req_id?`, `code`, `message` | op 失败 or 运行时错误 |
| `room_state` | `room`, `peers[]`, `joined` | join 成功 / peer 进出房间 / leave 成功，都推一条 |
| `message` | `room`, `from`, `payload`, `ts` | 收到来自 room 其他 peer 的消息 |
| `pong` | — | 响应 ping |

**就这六个**。任何第 7 个 type 都要先问"能不能拼进 `room_state` 或 `error`"。

### 3.4 payload 的格式抉择（关键决策）

**产品建议：`payload` 是一个 JSON 任意值（object / string / array / number），不做 base64**。

理由：
1. **DX 碾压优势**。base64 意味着上层每条消息都要 `btoa/atob`，devtools 里看到一坨黑乎乎，初学者立刻劝退
2. **mlink 的典型负载就是 agent 对话 JSON / 知识更新 diff / 文件同步元数据**，本身就是 JSON，二次编码纯粹是浪费
3. **真要传二进制**（图片、大文件）：
   - 小的：`payload: { kind: "binary", data: "<base64>" }`，上层约定，mlink 不关心
   - 大的：走未来的 `/stream` 通道（对应内核 `api::stream`），走 chunk 上传，不占 WS 通道
4. msgpack+zstd 的省流优势在 mlink 内核传输层已经做了（见 ARCHITECTURE.md §3），**WS 对外接口是面子，内部压缩是里子**，不要为里子的复杂性污染面子

**反例对照**：
- `POST /send {"data_base64": "..."}` 的 HTTP 方案（见 HTTP_ARCH_EVAL.md §2.2）是因为 HTTP 原本就不天然承载结构化 payload，base64 是权宜之计
- WS 是全双工 JSON 流，没有这个历史包袱，**不要背**

### 3.5 消息归属标识

一条 `message` 必含 `room` 和 `from`（app_uuid）。客户端代码就是：

```js
if (msg.room === 'my-agent-channel' && msg.from !== myUuid) { ... }
```

不需要 subscription_id / channel_ref 这类中间层。room 本身就是唯一标识。

### 3.6 错误码（极简）

```
1001  bad_request        JSON 格式错 / op 缺字段
1002  unknown_op         op 不认识
2001  not_joined         send 一个没 join 的 room
2002  room_full          保留
2003  peer_not_found     send 指定了不存在的 to
3001  daemon_not_ready   daemon 还在启动 / BLE 没 ready
3002  transport_error    发送失败（所有 transport 都发不出去）
4001  rate_limited       （未来）速率限制
```

**错误对象只有三段：code / message / req_id?**。不塞 stacktrace，不塞对象。

---

## 四、使用场景走查

### 场景 1：mteam agent 双机对话

```
Machine A: agent-a                      Machine B: agent-b
    │                                       │
    ws.send({op:'join', room:'567892'})     ws.send({op:'join', room:'567892'})
    │                                       │
    ◄── {type:'room_state', joined:true,    ◄── {type:'room_state', joined:true,
          peers:[{app_uuid:'<B>'}]}               peers:[{app_uuid:'<A>'}]}
    │                                       │
    ws.send({op:'send', room:'567892',
             payload:{role:'user',
                      text:'帮我查天气'}})
    │                                       │
    │                                       ◄── {type:'message', room:'567892',
    │                                             from:'<A>', payload:{role:'user',
    │                                             text:'帮我查天气'}, ts:...}
```

**DX 评分**：✅ agent 作者只写 3 个事件（`onopen` join、`onmessage` 分发、`send`），心智成本几乎为零。

### 场景 2：知识库广播

```
Machine A 更新知识 → ws.send({op:'send', room:'knowledge', payload:{kind:'patch', diff:[...]}})
                                          │
                                          ▼
Machine B/C/D 全员收到 {type:'message', room:'knowledge', from:'<A>', payload:{...}}
```

**广播语义**：`op:'send'` 不带 `to` 就是房间广播，不需要额外 API。

**去重**：mlink 内核已经有 SEQ 去重（见 ARCHITECTURE.md §6.3），不泄漏到 WS 层。上层看到的就是"每台机器各收到一份"。

### 场景 3：一个 agent 同时参与多 room

```js
ws.send({op:'join', room:'567892'})   // 工作房
ws.send({op:'join', room:'123456'})   // 私聊房
ws.send({op:'join', room:'999999'})   // 广播房

ws.onmessage = (e) => {
  const msg = JSON.parse(e.data)
  if (msg.type === 'message') {
    switch (msg.room) {
      case '567892': onWorkMsg(msg); break
      case '123456': onPrivate(msg); break
      case '999999': onBroadcast(msg); break
    }
  }
}
```

**产品判断**：这就是上面 §3.1 "一个连接订阅多 room" 铁律的回报。如果一条 WS 只能订一个 room，agent 就要维护 3 条 WS，心智成本 ×3。

### 场景 4：掉线恢复

- **短暂掉线**：浏览器/Node 应用层自己重连，WS reconnect 后重新 `op:'join'` 之前的所有 room
- **Daemon 侧语义**：WS 断开 = 自动 leave 所有本连接 join 的 room（不影响其他连接）
- **不做**：mlink WS 不负责客户端重连重订阅，上层自己记住 join 过哪些 room

---

## 五、连接与鉴权

### 5.1 端口与地址
- **绑定**：`127.0.0.1:7421`（默认，允许 `--port` 覆盖）
- **不绑 0.0.0.0**：跨机访问走 mlink 本身，不需要另一台机器直连你的 WS。这是安全边界。

### 5.2 鉴权（MVP：无）
- **信任边界 = 能打开本机 127.0.0.1 的进程**（跟 HTTP_ARCH_EVAL.md §3.3 一致）
- **未来**：`?token=...` query 参数，token 从 `~/.mlink/token` 读。先不做，但协议留口

### 5.3 Origin 校验
- **浏览器场景**：可选允许 `localhost` / `127.0.0.1` / Electron `file://`
- **配置**：`daemon.toml` 允许 allowlist

---

## 六、文档需求

最小需要三份：

1. **Quick Start（5 分钟跑通）** — 就是 §2 那 5 行，配一个图：两台机 daemon + 两个浏览器 tab
2. **Protocol Reference** — §3 的表格
3. **Examples** — 至少 3 个可跑示例：
   - `examples/echo-room/`：两浏览器 tab，互发文字
   - `examples/mteam-agent-stub/`：两 Node 脚本，模拟 agent A 问 agent B
   - `examples/knowledge-broadcast/`：一个发多个收

**不做**：详尽 API 参考、UML 图、长篇架构。README + 三个 example 足够让人动起来。

---

## 七、竞品对比 / 借鉴

| | **Socket.IO** | **Phoenix Channel** | **MQTT** | **mlink WS (产品建议)** |
|---|---|---|---|---|
| 消息类型数 | 很多（connect/disconnect/reconnect/error/event/ack...）| 适中（join/leave/push/reply/broadcast）| 核心 3（connect/publish/subscribe）| **本设计 10 个（4 op + 6 type）**|
| 订阅单位 | room / namespace | topic / channel | topic | **room (6位数字)** |
| 多订阅 | 支持（多 room） | 支持（多 topic） | 支持（多 topic） | ✅ **支持** |
| payload | 任意 JSON | 任意 JSON | 字节 | **任意 JSON（§3.4 决策）** |
| ack 机制 | 回调参数 | `{:reply, ref}` | QoS 1/2 | **`req_id` + `ack` 下行**（可选）|
| 鉴权 | 中间件 | `join` 返回 `{:error, :unauthorized}` | username/password, TLS cert | **MVP 无，V2 加 token**（§5.2）|

**借鉴点**：
- Phoenix Channel 的 `join` / `leave` / `push` / `broadcast` 模型最干净 → **mlink 的 4 个 op 直接抄这个语义**
- MQTT 的 topic 订阅理念 → mlink room 就是 topic
- Socket.IO 的 `ack` 回调 → mlink 用 `req_id` 简化（不做回调，只做异步响应）

**不借鉴**：
- Socket.IO 的自动重连、命名空间、广播/volatile/compress 区分 —— mlink 都不要
- MQTT 的 QoS 级别 —— mlink 内核已经有 ACK + 重传（§3.2 STREAM_ACK），对上不暴露
- Phoenix 的 presence —— 替换成 `room_state` 事件里的 `peers[]`，简化

---

## 八、非目标（产品划红线）

MVP **不做**：
1. ❌ 私信通道（`to: '<app_uuid>'` 做点对点就够，不需要独立 channel 概念）
2. ❌ 历史消息回放（mlink 不做持久化，上层自己存）
3. ❌ 服务端定义的 RPC 方法（调用方式留给内核 `api::rpc`，MVP 先不暴露到 WS）
4. ❌ 订阅过滤器（上层自己按 `from` / payload 字段过滤）
5. ❌ 压缩协商（WS permessage-deflate 交给 axum 默认行为，应用不管）
6. ❌ 客户端鉴权（§5.2）
7. ❌ 多 daemon（一台机器一个 daemon，见 HTTP_ARCH_EVAL.md §2.4）

---

## 九、对 ws-tech 技术设计的审查要点

等 ws-tech 技术稿到手后，产品视角要盯这几条：

1. **消息类型数量是否膨胀**。只要超过 10 个（op+type），就要问"能不能合并"。
2. **payload 是否还是 base64**。任何把 payload 转 base64 的设计都要顶住，除非有强技术理由（比如 serde 实现困难），否则坚持透明 JSON。
3. **多 room 是否一个 WS**。如果技术上为了省事退化成"一 WS 一 room"，产品一票否决。
4. **错误处理是否对开发者友好**。错误对象字段是否稳定、code 是否语义化、是否带 req_id 能定位到原请求。
5. **req_id 的 opt-in 设计**。不能强制每条 op 带 req_id，但带了就必须回 ack。
6. **welcome 消息是否足够**。客户端需要知道 daemon 版本、app_uuid 才能后续交互。
7. **连接级 vs room 级事件分离是否清晰**。`welcome` / `pong` 是连接级，`room_state` / `message` 是 room 级。不能混。
8. **能不能 5 行代码跑通**（§2 验收标准）。tech 设计如果让这 5 行变 15 行，产品打回。
9. **是否留了将来加鉴权的口子**（query token / first-message auth）。
10. **断线恢复的责任边界**。客户端断了 WS，服务端是否清理 room 订阅。文档必须说清楚。

---

## 十、开放问题（等 tech 确认）

1. 是否把 `pong` 也叫 `type: pong`，还是直接用 WS 协议层 ping/pong？推荐应用层，浏览器侧 WS 协议层 ping 不暴露给 JS。
2. `req_id` 是字符串还是数字？推荐字符串（兼容 UUID / nanoid）。
3. `room_state.peers` 里的 peer 对象包含哪些字段？建议至少 `{app_uuid, name?, transport?}`。transport 是 debug 用。
4. `message.ts` 是发送方时钟还是接收方时钟？推荐接收方（daemon 收到 frame 的那一刻），避免跨机时钟不同步问题。
5. 一个 app_uuid 能不能同时开多条 WS？推荐能（同一 agent 多 tab 场景）。daemon 按连接独立订阅。

---

## 附：决策速查

| 决策 | 选择 | 理由 |
|---|---|---|
| 协议格式 | JSON 文本 | devtools 可见，DX 第一 |
| 一连接多 room | ✅ 支持 | §4.3 多房间是高频场景 |
| payload 编码 | 透传 JSON，非 base64 | §3.4 避免二次编码成本 |
| op 数量 | 4 (join/leave/send/ping) | 最小可用集 |
| type 数量 | 6 (welcome/ack/error/room_state/message/pong) | 连接级 + room 级分离 |
| 鉴权 | MVP 无，V2 token | §5.2 localhost 信任模型 |
| 重连责任 | 客户端 | §4 场景 4 |
| 端口 | 127.0.0.1:7421 | 不绑 0.0.0.0，保安全边界 |
