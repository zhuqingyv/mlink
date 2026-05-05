# 产品对 ws-tech 技术稿的对抗意见

**对抗人**：产品架构师
**对象**：`/Users/zhuqingyu/project/mlink/.claude/memory/WS_API_DESIGN.md`
**日期**：2026-04-20

---

## TL;DR

技术稿在**三件事上优于我的初稿**（信封 v/id 版本化、core 多 room_hash 改造、axum lib 选型一致），但**在四件事上重伤开发者体验**，必须挡回去：

1. 🚨 **消息类型爆炸（约 40 个）** —— 违反"能少一个就少一个"原则
2. 🚨 **payload 无脑 base64** —— agent 场景典型负载就是 JSON，硬转 base64 = 每条消息 `atob/btoa` + devtools 黑盒
3. 🚨 **首期带 RPC/PubSub/Stream 暴露到 WS** —— 产品 MVP 承诺是"最小"，这三块吃掉一半工期
4. 🚨 **lazy Node + 60s 空闲 stop** —— 首次 join 必延迟 + 权限弹窗，DX 毁灭级

此外 RPC 双向设计、`message.sent` 等确认事件、handler 单点绑死等细节也要收窄。

---

## 一、技术稿做得好的地方（产品认可）

下面几条我吸收，修订产品设计：

### 1.1 JSON 信封 `{v, id, type, payload}` —— 采纳
我的初稿只有 `op/type/payload`，没有版本位 `v`，没有通用 `id`。
- `v` 用于未来平滑升级（不同版本 daemon 能直接拒绝老/新 client）
- `id` 做 opt-in 请求关联，比我原方案的 `req_id` 命名更短、更通用
- **建议统一用 `id`**（而不是我原来的 `req_id`），简短不啰嗦

### 1.2 core 多 room_hash 改造 —— 认可，但有警告
`Node::room_hashes: HashSet<[u8;8]>` 是必需的。单值版本撑不起"一连接多 room"。
**警告**：这是首期范围内最重的改动，tech 估 0.5 人日偏乐观。建议先写多 room 的集成测试用例（两个 room，两条握手，peer 进对的 room），再动手改。已在我"场景 3"里要过这个能力，保留。

### 1.3 端口策略（动态 + daemon.json + flock）—— 采纳
我初稿里写的"默认 7421"是糊弄的。tech 的方案更稳：动态端口 + `~/.mlink/daemon.json` + flock + pid 校验。这是标准做法，没毛病。
**补充**：daemon.json 要加个 `version` 字段，客户端读到不认识的 version 直接报错，避免旧 SDK 连到新 daemon 再失败。

### 1.4 axum 选型、共享 Node + 单例约束 —— 一致
跟我原设计和 HTTP_ARCH_EVAL 一致，没异议。

### 1.5 lazy Node 的**出发点**正确
但实现方式错，见 §二.4。

---

## 二、产品必须挡回的四件事

### 2.1 🚨 消息类型爆炸

**证据**：tech §1.2 + §1.3 数出来的类型：

- 客户端→服务端：22 个（hello, room.{join,leave,list}, peers.{list,scan,disconnect}, message.send, rpc.{call,register,reply}, pubsub.{subscribe,unsubscribe,publish}, stream.{create,chunk,finish}, trust.{list,remove}, health, ping）
- 服务端→客户端：21 个（ready, room.{joined,left,list_result}, peers.{list_result,scan_result,disconnect_result}, message.sent, rpc.{result,registered,reply_ack}, pubsub.{subscribed,unsubscribed,published}, stream.{created,chunk_ack,finished}, trust.{list_result,removed}, health.result, pong, error, event.peer_connected, event.peer_disconnected, event.peer_lost, event.peer_reconnecting, event.message_received, event.rpc.request, event.pubsub.message, event.stream.chunk, event.stream.finished）

**≈ 43 个类型。** 比 Socket.IO、Phoenix Channel、MQTT 加起来还多。

**产品要求**：MVP 砍到 ≤ 15 个。砍法：

| 砍什么 | 怎么砍 |
|---|---|
| `peers.list` / `peers.scan` / `peers.disconnect` | **全部下掉**。peer 状态通过 `room_state` 事件推（见下），不需要主动查询。首期不允许手动断 peer（Node 自己管）。 |
| `room.list` | 下掉。客户端自己记住 join 了哪些。daemon 不做状态服务。 |
| `trust.list` / `trust.remove` | 下掉。首期 trust 管理走 CLI / 配置文件。WS 不开口。 |
| `health` | 下掉。mlink 本来就不跑健康检查产品功能，放诊断通道里（future）。 |
| `rpc.*` + `event.rpc.*` | **整组砍掉**（见 §2.3） |
| `stream.*` + `event.stream.*` | **整组砍掉**（见 §2.3） |
| `pubsub.*` + `event.pubsub.*` | **整组砍掉**（见 §2.3） |
| `message.sent` 确认 | **下掉**。改成"带 `id` 就回 `ack`，不带就静默"。不要为每个 op 造专属确认类型。 |
| `rpc.registered` / `rpc.reply_ack` / `pubsub.subscribed` / `stream.created` | 通用 `ack` 就够。 |
| `event.peer_lost` / `event.peer_reconnecting` | 合并进 `room_state` 事件的 peer 状态字段（`state: connected\|reconnecting\|lost`）。 |
| `event.peer_connected` / `event.peer_disconnected` | 合并成 `room_state` 事件：任何 peer 进/出/状态变，都推一条完整的 room_state。客户端只看最新状态，不做状态机。 |

**MVP 最终类型（12 个）**：
- 上行 4：`hello`, `join`, `leave`, `send` + 通用 `ping`
- 下行 6：`ready`, `ack`, `error`, `room_state`, `message`, `pong`

> `hello/ready` 取代我初稿的 `welcome`，跟 tech 命名兼容。
> `join/leave/send` 去掉 `room.` 前缀 —— MVP 只有 room，不需要命名空间。

**砍掉的不是功能，是首期暴露面**。RPC/PubSub/Stream 仍然在 mlink-core 里存在，只是 WS 不暴露，等真实客户有要求再开。

---

### 2.2 🚨 payload 无脑 base64

**tech §1.4 原话**：
> "默认 base64（`encoding: "base64"`），原因：JSON 原生不支持二进制；base64 编码开销固定 33%，对常见消息（<64KB）可接受"

**产品坚决反对**，原因：

1. **agent 对话 / 知识 patch 本身就是 JSON**。硬 base64 一层意味着每台机器的 agent 代码都要：
   ```js
   // tech 方案
   const data = btoa(JSON.stringify({role:'user', text:'hi'}))
   ws.send(JSON.stringify({type:'message.send', payload:{data, encoding:'base64'}}))
   // 对面
   const raw = atob(msg.payload.data)
   const obj = JSON.parse(raw)
   ```
   **对面就是每条消息 `JSON.parse(atob(...))`**。这是给初学者劝退。
2. **devtools 变黑盒**。chrome devtools WS frame 面板里看到的是 `"aGVsbG8gd29ybGQ..."`，开发者必须复制到另一个 tab 解码才能 debug。我初稿的"5 行跑通"验收就挂在这一步。
3. **33% 带宽开销不是"可接受"，是白扔的**。agent 消息密度高，一天几十万条，这个开销实打实。
4. **tech 自己说"大载荷 >256KB 强制走 stream"** —— 那就说明他也承认 base64 不适合大消息。既然小消息是 JSON 本来能表达的，大消息走别的通道，**中间那段 base64 压根没有场景**。

**产品方案**（硬挡）：

```jsonc
// 默认：payload 是任意 JSON 值（object/string/array/number/null）
{"v":1,"type":"send","payload":{"room":"482193","data":{"role":"user","text":"hi"}}}

// 要传二进制（图片 thumbnail 之类）：应用层自己约定
{"v":1,"type":"send","payload":{"room":"482193","data":{"kind":"img","b64":"iVBORw…"}}}
```

- **`data` 字段是任意 JSON**，daemon 收到就 `serde_json::Value` 直接塞进 frame payload
- daemon 内部序列化：`serde_json::to_vec(data)` → mlink frame 的 payload bytes
- 对端 daemon 反序列化：`serde_json::from_slice(payload)` → 推给 client
- **二进制是上层 convention，不是 mlink 的事**（符合"哑管道"原则）
- 真有大二进制需求，走未来的 stream 通道（对应 `api::stream`），WS 不承担

**回答 tech 的问题** "用户场景大概多大的消息？"：
- agent 对话 payload 典型 100B ~ 4KB（JSON 文本）
- 知识 patch 典型 1KB ~ 50KB（JSON diff）
- 文件元数据 < 1KB
- **> 64KB 的基本都是文件/媒体**，产品层强烈建议走 stream

**256KB 阈值**：保留，但移到 stream 接口（WS 不做）。WS `send` 硬上限 **64KB**，超了就回 `error: payload_too_large` 引导去 stream。

---

### 2.3 🚨 首期暴露 RPC / PubSub / Stream

**tech 问**："可不可以先砍掉 RPC，只留 message/pubsub？"

**产品答**：**RPC、PubSub、Stream 三个都不要首期暴露到 WS**。

理由：
1. **tech 自己估这三块吃掉 1.5 + 1.5 = 3 人日**（不含 stream 的集成测试工期）。占总工期的 25%。
2. **mteam 目标场景**（agent 互通、知识广播、多 room）**全都能用 `send` + `message` 搞定**：
   - RPC：让 agent A 调用 agent B 的 `query` 方法 → 应用层自己用 `send` + 约定 `payload.reqId` + `send` 回消息实现。mlink 不需要管 RPC 语义。
   - PubSub：topic 本身就是个 room 或 room 的子频道，约定 `payload.topic` 字段，接收方自己过滤。
   - Stream：首期没需求；真有大文件需求上层用 HTTP 或者走 mlink 以外的通道。
3. **RPC 的双向设计（tech §A3）本身过于复杂**：
   - `rpc.call` → `event.rpc.request` → `rpc.reply` → `rpc.result` 四段式
   - `rpc_handler_owner` 的 session 绑死、session 崩溃 pending 重发、duplicate_register 冲突
   - handler 只能绑一个 session 是**严重限制**：多 tab 同一 agent 代码都订阅同一 method 会冲突，tech 还打算加 `takeover` 标志 —— 这是设计味道不对的前兆
4. **产品 MVP 铁律：不为假想客户做功能**。mteam 现在的 mvp 就是对话消息，先把这个打通，RPC/PubSub/Stream 等真实用户要求再开。

**妥协方案**（如果 tech 强烈坚持留口子）：
- MVP 只留 `send` / `message`（message 里可以带 `payload.reply_to` 字段，应用层可选用）
- RPC/PubSub/Stream 在 daemon 内部代码有桩，但 WS 消息类型表里不出现
- Post-MVP v0.2 再开放

**新工期估**：砍掉这三块 + 减少 event 类型后，从 14 人日 → **约 8~9 人日**。3 周单人完成有余，或者一人 1.5 周。

---

### 2.4 🚨 lazy Node + 60s 空闲 stop

**tech §2.3 原话**：
> "daemon 启动：只起 WS server，Node 为 None；第一个客户端 room.join 触发 Node::new + Node::start；所有 client 都 leave 了所有 room → 保持 60 秒空闲后 stop"

**产品反对**，理由：

1. **首次 join 延迟 + 权限弹窗不可接受**。agent 代码：
   ```js
   ws.send({type:'join', payload:{code:'482193'}})
   // 卡 3~8 秒（BLE 初始化）+ 系统权限弹窗 "允许 mlinkd 访问蓝牙？"
   ```
   用户第一次跑 mteam 就懵。
2. **macOS BLE peripheral bring-up 有严格时序**（见 `project_peripheral_bringup_timing.md`），启动过程抢跑静默失败，从 WS 请求路径里触发风险更大。
3. **60s 空闲 stop 的意义不明**。daemon 进程都在了，省那 60s 的 BLE 资源占用不值得引入"抖动窗口"。要么跑要么不跑，不要半开。
4. **daemon 作为后台服务本来就是"我准备好了"状态**。不能把状态变化塞到应用层请求路径上。

**产品方案**：

| 场景 | 行为 |
|---|---|
| daemon 启动 | **立即 `Node::new` + `Node::start`**，BLE/TCP 都初始化好。这期间的任何 WS 请求在 `hello` 响应里带 `ready: false`，客户端等到 `ready: true` 再 join。|
| 所有 client leave | **保持 Node 运行**，不主动 stop。Node 不占 CPU，BLE advertising 就让它跑着 —— 本来 daemon 就是为了这个存在的。|
| daemon 真要停 | 用户显式 `mlinkd stop` 或进程被杀。|

**权限弹窗**：首次 daemon 启动（通常用户装 mlink 之后第一次），弹窗就弹窗，是可接受的一次性成本。比在每次 agent `join` 时弹窗好 10 倍。

**回答 tech 自己的担忧** "daemon 可能只做诊断/健康检查，不一定要打开蓝牙"：
- 首期 WS 里没有"纯诊断"入口（`health` 被我砍了）
- 真要纯诊断，加个 `--no-node` 启动参数，不是动 WS 协议

---

## 三、其他要挑的点

### 3.1 `message.sent` 确认 —— 产品不认

tech §1.3 列了 `message.sent` 作为"投递到 wire 的确认"。

**问题**：
- 跟 `ack` 功能重复（带 `id` 的请求天然有 ack）
- "投递到 wire"这个语义对 app 层没用 —— app 层只关心"对面收到了吗"，投到 wire 不代表到对面
- 每条消息一个 sent 回包，流量翻倍

**改为**：只有 `send` 带 `id` 才回 `ack`，不带 id 静默发送。没有专属 `message.sent` 类型。

### 3.2 `event.peer_connected/disconnected` 分开 —— 产品不认

用 `room_state` 事件统一表达：

```jsonc
// peer B 进房间
{"v":1,"type":"room_state","payload":{
  "room":"482193",
  "peers":[
    {"app_uuid":"B","name":"bob","state":"connected","transport":"ble"}
  ]
}}

// peer B 掉线重连中
{"v":1,"type":"room_state","payload":{
  "room":"482193",
  "peers":[
    {"app_uuid":"B","name":"bob","state":"reconnecting","attempt":3}
  ]
}}
```

**产品收益**：
- 客户端不维护状态机，每次都是"接收最新快照"
- 4 个 event 类型（connected/disconnected/lost/reconnecting）合并为 1 个
- 新增一个 peer 状态（比如 future 的 paused）不需要加消息类型

### 3.3 RPC handler 绑死单 session —— 产品反对

tech §2.4 写：
> "同一个 method 只能被一个 session 持有 handler；第二个 session 来注册同名 method → duplicate_register"

**反例**：浏览器开两个 tab 同一个 agent，都订阅了同一个 method，直接 `duplicate_register` 失败是反直觉的。

虽然整个 RPC 我已经砍了（§2.3），但如果 post-MVP 要做：
- **默认应该是"替换"语义**（第二个注册赢）
- 或者"全部 handler 都收到请求，第一个回复的赢"
- 不应该是"拒绝"语义

### 3.4 client id 和 daemon 生成 request_id 混用 —— 产品确认 tech 的做法对

tech `event.rpc.request.request_id` 用 "daemon-generated-7"。这是对的，客户端 id 和内部 RPC id 必须分层，避免冲突。我初稿没涉及这块，确认 tech 方案。

### 3.5 client side 自己管重连 —— 认可

tech §4 第 2 条 "订阅状态绑定到 WS 连接，断开全部回收，客户端重连重 join"。
**产品认可**。简单就是美。session 复活机制在 MVP 不要做。

**文档要求**：SDK 示例里必须包含重连+重订阅代码，这是 DX 的一部分：

```js
function connect() {
  const ws = new WebSocket(url)
  ws.onopen = () => { for (const room of myRooms) ws.send({type:'join', ...}) }
  ws.onclose = () => setTimeout(connect, 1000)
  // ...
}
```

### 3.6 认证 —— 认可 MVP 不做

tech §4 第 4 条和我 §5.2 一致。localhost 信任模型 OK。
**补充场景**：多用户共享机器（很少，但有，比如家庭共享 Mac）—— 不在 MVP 范围，V2 加 token 即可。

### 3.7 错误码 —— 大部分认可，有微调

tech §1.5 的 10 个枚举基本够用。微调：
- `duplicate_register` 删（没有 rpc.register 就不需要）
- `payload_too_large` 保留（64KB 阈值）
- 加 `rate_limited`（留位）
- 错误 code 建议用**字符串 snake_case**（tech 已这样做），而不是 HTTP-style 数字。更好读。

产品初稿里我用的 `1001/2001/...` 数字码是沿袭习惯，**采纳 tech 的 snake_case**。

### 3.8 WS 协议层 ping vs 应用层 ping

tech §2.4 写用协议层 ping/pong（tokio-tungstenite 自带）。
**产品补充**：浏览器里协议层 ping 不暴露给 JS，所以应用层 `ping/pong` 必须也保留（客户端触发）。两层都要有。

---

## 四、修订后的协议总览（产品视角）

### 上行（4 个 + 1 ping）
```
hello   { client_name, client_version }
join    { code, encrypt?, name? }
leave   { code }
send    { room, data (任意 JSON), to?: app_uuid }
ping    {}
```

### 下行（6 个）
```
ready        { app_uuid, version, caps: [] }  // 回应 hello
ack          {}                               // 回应带 id 的上行
error        { code, message }                // snake_case code
room_state   { room, peers: [{app_uuid, name, state, transport, attempt?}] }
message      { room, from, data, ts }
pong         {}
```

### 信封（沿用 tech 的 v/id）
```
{ v: 1, id?: "c-42", type: "...", payload: {...} }
```

**总计 11 个消息类型**（5 上 + 6 下），比 tech 的 ~43 个砍掉 74%。

---

## 五、对 tech 提问的直接回答

| tech 问 | 产品答 |
|---|---|
| 1. 消息类型是否太多或太少？RPC 双向是否必要？ | **太多**。MVP 砍 74% 到 11 个。**RPC 首期全砍**，应用层自己在 payload 里约定。 |
| 2. base64 产品接受吗？256KB 阈值合理？ | **不接受 base64**，payload 改为透传 JSON。stream 阈值保留 64KB（send 上限）。|
| 3. 多 room "并存 vs 切换"？ | **并存是真实场景**（§我初稿场景 3）。多 room_hash 改造必要。|
| 4. lazy Node 启动？ | **改为 daemon 启动即 start**。避免首次 join 延迟+权限弹窗。60s 空闲 stop 删除。|
| 5. 断线上层自己管是否太简陋？ | **不简陋，就该这样**。SDK 示例里配重连代码即可。|
| 6. 错误码 10 个够用？ | 基本够。删 `duplicate_register`（没 rpc.register），加 `rate_limited` 占位。snake_case 采纳。|
| 7. 认证假设 OK 吗？ | OK。多用户共享机器场景不在 MVP，V2 加 token。|

---

## 六、给 team-lead 的综合建议

**MVP 范围（我和 tech 达成一致后）**：
- 11 个 WS 消息类型
- payload 透传 JSON，不 base64
- daemon 启动即 Node start
- core 多 room_hash 改造（tech 识别到的必要技术债）
- 端口策略采纳 tech 方案
- 无 RPC / PubSub / Stream 暴露（core 里仍保留）
- 无鉴权（localhost 信任）

**预计工期**：8~9 人日（比 tech 的 14 人日少 40%，砍掉的是暴露面不是内核）

**验收标准（产品硬线）**：
1. `examples/echo-room/` 5 行 JS 双机通信跑通
2. 同一 WS 订阅 3 个 room，消息正确路由到 3 个 `message.room` 回调
3. 浏览器 devtools WS 面板里 payload 直接可读 JSON，无 base64
4. daemon 启动 3 秒内 `ready: true`（冷启动 5 秒内）
5. 客户端 WS 断连重连 + 重 join 全流程无丢消息（对端发送期间只在重连窗口内丢）

**剩余待定**（需 team-lead 仲裁）：
1. RPC 是否真全砍，还是保留 `send.reply_to` 半吊子约定
2. daemon 启动即 `Node::start` 会不会让非 mteam 用户（比如只想测 mlink 本身的用户）觉得权限弹窗突兀

---

**我方态度**：产品线坚持上述四大反对。技术线在信封/端口/多 hash 改造上认可采纳。11 个消息类型的最终集是产品底线，不再退。
