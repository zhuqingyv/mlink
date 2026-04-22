# mlink 双机联通测试 Checklist

面向两台 Mac（或同机两终端）的端到端验收，覆盖 BLE 与 TCP 两条通道。本文档只关心"两边能不能连上、消息能不能互发、异常能不能自愈、房间能不能隔离"。单元/集成级用例见 `.claude/memory/TEST_CASES.md`。

## 约定

- 版本：运行前先 `cargo build --release -p mlink-cli`，两台机器使用同一 commit（`git rev-parse HEAD` 应一致）。
- 可执行路径：`./target/release/mlink`。
- 日志：main.rs 里通过 `tracing_subscriber` 设为 WARN。业务日志走 `println!`（`[mlink] ...`）和 `eprintln!`（`[mlink:conn] ...`、`[mlink:debug] ...`），用 `tee` 留档：`./target/release/mlink --transport ble chat 123456 2>&1 | tee /tmp/mlink-A.log`。
- 两台机器标记：**Mac-A**（biying6 WiFi，充当 host）、**Mac-B**（biying6-guest WiFi，充当 join）。BLE 测试跨 VLAN 可通；TCP 测试必须让两机处于同一 L2。
- 房间码：正例统一用 `567892`；反例用 `111111` / `222222`。
- 权限：首跑 BLE 时系统弹 Privacy & Security → Bluetooth，两台都需放行；之前踩过的坑见 `PITFALLS.md`。
- Ctrl+C 退出走顶层 `biased select!` → `libc::_exit(0)`，立刻返回 shell 即算下线成功。

## 通用断言

下列所有测试用例默认共享这些判据，单项只列增量判据：

- **FAIL 信号**：进程 panic / 任一边 `error:` 开头 / 等待超过 60s 仍无 `peer connected`。
- **LEAK 信号**：退出后 `lsof -p` 残留端口、`pgrep mlink` 仍有进程、mDNS daemon 线程 stuck。

---

# 一、BLE 测试（跨 WiFi，Mac-A biying6 / Mac-B biying6-guest）

BLE 是当前主力验证通道，涉及 shared adapter 修复（e1f6009）和 host/join 角色分离（af428aa）。host 端不扫描、只广告；join 端只扫描、只拨号。

## B-01　host + join 基础连接

**前置**
- 两台 Mac 蓝牙开启，距离 ≤ 5m。
- Mac-A/B 均已授予 Bluetooth 权限。

**执行**
```bash
# Mac-A (host)
./target/release/mlink --transport ble 567892 2>&1 | tee /tmp/mlink-A.log

# Mac-B (join) — 等 Mac-A 打印 "serving as <uuid>" 后再跑
./target/release/mlink --transport ble join 567892 2>&1 | tee /tmp/mlink-B.log
```

**预期**
- Mac-A：`[mlink] serving as <uuid-a> via ble — waiting for peers...`；10s 内出现 `[mlink] incoming central <wire-id> — handshaking...` → `[mlink] + <uuid-b> (incoming)`。
- Mac-B：`[mlink] joining as <uuid-b> via ble — scanning for host...` → `[mlink] discovered <name> (<wire-id>) — connecting (attempt 1/3)...` → `[mlink] + <uuid-a>`。
- 双方随后刷出 `[mlink] peer connected: <对端 uuid>`。
- `uuid-a != uuid-b`，两边互相记录对方。

**验证**
- `grep 'peer connected' /tmp/mlink-A.log /tmp/mlink-B.log` 各至少一行。
- `grep 'handshake' /tmp/mlink-*.log` 无超时 / 无 `accept .* failed`。

---

## B-02　chat 模式互发消息

**前置** B-01 已 PASS。

**执行**
```bash
# Mac-A
./target/release/mlink --transport ble chat 567892 2>&1 | tee /tmp/mlink-A-chat.log

# Mac-B
./target/release/mlink --transport ble join --chat 567892 2>&1 | tee /tmp/mlink-B-chat.log
```
连接后，Mac-A 输入 `hello-from-A` 回车；Mac-B 输入 `hello-from-B` 回车；再互发 3 条短消息。

**预期**
- Mac-A 终端出现 `[<uuid-b-or-name>] hello-from-B`。
- Mac-B 终端出现 `[<uuid-a-or-name>] hello-from-A`。
- 每条消息的收端打印滞后发端 ≤ 2s。
- 不出现 `send to <id> failed`、`(no peers yet — message dropped)`（连接建立后）。

**验证**
- `grep 'hello-from-B' /tmp/mlink-A-chat.log` ≥ 1 行。
- `grep 'hello-from-A' /tmp/mlink-B-chat.log` ≥ 1 行。
- 消息顺序在各自终端内严格按发送顺序出现。

---

## B-03　断线重连（join 端 Ctrl+C 后重开）

**前置** B-02 已 PASS，两端 chat 连接活着。

**执行**
1. Mac-B Ctrl+C。观察 Mac-A。
2. 等 Mac-A 打出 `peer disconnected` 或 `peer lost`。
3. Mac-B 重新跑 `./target/release/mlink --transport ble join --chat 567892`。
4. 连接后 Mac-A 发 `reconnect-ping`，Mac-B 发 `reconnect-pong`。

**预期**
- Mac-A 在步骤 2 内 30s 出现 `peer disconnected: <uuid-b>` 或 `peer lost`。
- 步骤 3 后 30s 内 Mac-A 再次出现 `+ <uuid-b> (incoming)`；uuid-b 可能相同（同一可执行）也可能不同（Node 新实例）——两种情况都 PASS。
- 互发的消息两边都能看到，无 `send .* failed`。

**验证**
- `/tmp/mlink-A-chat.log` 同时有 `peer disconnected`（或 `peer lost`）**和**之后的 `+ <uuid>` 两条记录。
- Mac-A 未崩溃、未重启。

---

## B-04　host 端 Ctrl+C 后重开

**前置** B-03 已 PASS。

**执行**
1. Mac-A Ctrl+C（观察是否立即退出到 shell）。
2. Mac-B 应看到消息送达失败并最终 `peer disconnected`。
3. Mac-A 重新跑 `./target/release/mlink --transport ble chat 567892`。
4. 等 Mac-B 重连后互发 `host-back` 一条消息。

**预期**
- Mac-A Ctrl+C 后 ≤ 1s 返回 shell（顶层 `_exit` 短路）。
- Mac-B 在 Mac-A 重启后 60s 内重连（join 端 scanner 持续跑，MAX_RETRIES=3，失败间隔 1–3s 随机退避）。
- 若 Mac-B 已耗尽 3 次重试（`giving up on <id> after 3 attempts`），需 Mac-B 手动 Ctrl+C 重跑——这是已知行为，记录在 checklist 里。

**验证**
- Ctrl+C 响应时间肉眼 ≤ 1s。
- 成功重连后 Mac-A 终端出现 `[<name>] host-back` 的回显（由 Mac-B 发）。

---

## B-05　多次连接：join 端反复退出重进

**前置** B-04 已 PASS，Mac-A host 保持运行。

**执行** 在 Mac-B 重复 3 轮：`join --chat 567892` → 互发一条消息 → Ctrl+C。每轮间隔 ≥ 10s。

**预期**
- 每轮 Mac-A 均打印 `peer disconnected` 后又 `peer connected`。
- 不出现"卡在 engaged_wire_ids 不释放"导致后续拒连（`skip dial .* already engaged` 不应持续出现）。
- Mac-A 进程稳定，无 OOM / 无 panic。

**验证**
- `grep -c 'peer connected' /tmp/mlink-A.log` ≥ 3（含 B-01 的首连可能更多）。
- `grep 'skip dial' /tmp/mlink-A.log` 仅在同一批扫描内去重时出现，不应跨轮残留。

---

## B-06　房间隔离：不同房间码不会互连

**前置** 关掉 B-01~B-05 的所有进程。

**执行**
```bash
# Mac-A
./target/release/mlink --transport ble chat 111111 2>&1 | tee /tmp/mlink-A-room.log

# Mac-B
./target/release/mlink --transport ble join --chat 222222 2>&1 | tee /tmp/mlink-B-room.log
```
运行 60s 后 Ctrl+C。

**预期**
- Mac-A 永远不打 `+ <uuid>`，只是一直 waiting。
- Mac-B 可能扫到 Mac-A 的广告但 scanner 的 `room_hashes` 过滤直接丢弃——即使漏过，Handshake 阶段也会触发 `RoomMismatch`，日志打印 `dropped <uuid>: different room`。
- 不出现 `peer connected`。

**验证**
- `grep 'peer connected' /tmp/mlink-*-room.log` 为空。
- 如果出现 `dropped .* different room`——说明走到握手后才隔离，也算 PASS（符合 `project_room_handshake_verify` 的设计）。

---

# 二、TCP 测试（两机同一 LAN 或单机两终端）

TCP 路径已修过 mDNS 自发现过滤（b2e92b2）和多网卡去重。若两机跨 VLAN（biying6 vs biying6-guest）mDNS 多播过不去，TCP 测试请用单机双终端，或让两台临时接到同一 WiFi。

## T-01　mDNS 发现

**前置**
- 两机（或两终端）都能绑本地 socket。
- `./target/release/mlink --transport tcp doctor` 两侧均 `doctor: all checks passed`。

**执行**
```bash
# Mac-A
./target/release/mlink --transport tcp 567892 2>&1 | tee /tmp/mlink-A-tcp.log

# Mac-B（或第二终端）
./target/release/mlink --transport tcp join 567892 2>&1 | tee /tmp/mlink-B-tcp.log
```

**预期**
- Mac-B 在 15s 内打印 `discovered <name> (<wire-id>) — connecting ...` → `+ <uuid-a>`。
- Mac-A 打印 `incoming central <wire-id> — handshaking...` → `+ <uuid-b> (incoming)`。
- 两端 `peer connected` 对齐。

**验证**
- `grep 'peer connected' /tmp/mlink-A-tcp.log /tmp/mlink-B-tcp.log` 各至少 1 行。
- `grep 'tcp accept error' /tmp/mlink-A-tcp.log` 为空。

---

## T-02　chat 互发消息

**前置** T-01 PASS。

**执行** 同 B-02，把 `--transport ble` 换成 `--transport tcp`。依次互发 `tcp-ping-A`、`tcp-pong-B`、`bigger-message-<N-bytes>` 等 5 条。

**预期**
- 双向消息 ≤ 500ms 送达（LAN 环境）。
- 大消息（例：`python3 -c "print('x'*8000)"` 粘贴后回车）通过 mpsc(16) 的 stdin 通道背压但不丢。
- 不出现 `send .* failed` / 连接断开。

**验证**
- `grep 'tcp-ping-A' /tmp/mlink-B-tcp-chat.log` ≥ 1。
- `grep 'tcp-pong-B' /tmp/mlink-A-tcp-chat.log` ≥ 1。

---

## T-03　自发现过滤（不会连自己）

**前置** 无。

**执行**
```bash
# 单机一个终端
./target/release/mlink --transport tcp chat 567892 2>&1 | tee /tmp/mlink-self.log
```
运行 30s。

**预期**
- 终端持续 `scanning for host...` 或 waiting；**不得**出现 `+ <自己的 uuid>` 或 `incoming central ... (自己)`。
- 自发现过滤在 `transport/tcp.rs::discover` 内以 `app_uuid` 比对，commit b2e92b2 已修。

**验证**
- `grep 'peer connected' /tmp/mlink-self.log` 为空。
- `grep 'skip .* self' /tmp/mlink-self.log`（若存在自过滤调试日志）能看到。否则仅凭"无 peer connected"即可判 PASS。

---

## T-04　多网卡去重（一机多 IP 不会重复连）

**前置**
- Mac-A 同时接 WiFi + 以太网（或 WiFi + USB 网卡），`ifconfig | grep 'inet '` 显示 ≥ 2 个非回环 IPv4。
- Mac-B 同一 LAN。

**执行**
```bash
# Mac-A
./target/release/mlink --transport tcp chat 567892 2>&1 | tee /tmp/mlink-A-multi.log

# Mac-B
./target/release/mlink --transport tcp join --chat 567892 2>&1 | tee /tmp/mlink-B-multi.log
```

**预期**
- Mac-B scanner 可能看到 Mac-A 的两条 mDNS 记录（每个网卡一条），但 `discover` 里的多 IP 去重（commit b2e92b2）只保留一条，且 `node.connect_peer` 后续以 `app_uuid` 对齐，**最终 `peers` 只有 1 条**。
- Mac-A 的 `+ <uuid-b> (incoming)` 只出现 1 次（首连），不会出现"一边 2 条链接、另一边 1 条"。

**验证**
- Mac-A `grep -c '+ .*incoming' /tmp/mlink-A-multi.log` == 1。
- Mac-B `grep -c '^\[mlink\] +' /tmp/mlink-B-multi.log` == 1。
- 互发一条消息，两端各只回显一次（没有因为重连而双打）。

---

## T-05　房间隔离：不同房间码不互连

**前置** 清理进程。

**执行**
```bash
# Mac-A
./target/release/mlink --transport tcp chat 111111 2>&1 | tee /tmp/mlink-A-room-tcp.log

# Mac-B
./target/release/mlink --transport tcp join --chat 222222 2>&1 | tee /tmp/mlink-B-room-tcp.log
```
60s 后 Ctrl+C。

**预期**
- 不出现 `peer connected`。
- mDNS 的 `rh=<hex>` TXT 字段在 scanner 阶段就过滤掉异房间；即使穿透，Handshake 也会判 `RoomMismatch` → 日志 `dropped <uuid>: different room`。

**验证**
- `grep 'peer connected' /tmp/mlink-*-room-tcp.log` 为空。
- `grep 'different room' /tmp/mlink-*-room-tcp.log` 若命中即走到了握手——和 BLE 一样都算 PASS。

---

# 三、执行顺序 & 退出判据

建议按 B-01 → B-02 → B-03 → B-04 → B-05 → B-06 → T-01 → T-02 → T-03 → T-04 → T-05 依次跑。

任一用例 FAIL：

1. 立即收集两侧 `/tmp/mlink-*.log`、`mlink --transport <x> doctor` 输出、`sw_vers`、`git rev-parse HEAD`。
2. 优先对照 `PITFALLS.md`（macOS BLE local_name、Peripheral bring-up、scanner 软放行等都在里面）。
3. 再跑对照组：同一用例切换 transport、或切换 host/join 角色互换，确认失败是否对称。

全部 PASS 才算"双机联通"验证通过，可向上游（mteam 等）开放集成。

---

# 四、未覆盖 & 未来扩展

- IPC transport：尚未双机验证（语义上也不适用）——本 checklist 不覆盖。
- QUIC：Roadmap 里的跨网络通道，架构未落地，不纳入。
- 压力测试（>2 节点 mesh、高频小消息、>1MB 文件传输）：需要单独写 `benches/` 或 `tests/scenarios/`，本 checklist 聚焦功能联通。
- `send --file` 路径：`main.rs::unsafe_clone_node` 里明确 `unimplemented!()`，daemon 落地前此路径不测。
