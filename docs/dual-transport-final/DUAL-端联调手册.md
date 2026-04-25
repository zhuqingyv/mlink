# 双通道双端联调手册

给 agent 看的执行手册。读完直接跑，不需要额外上下文。

## 前置条件

- 两台 Mac，在同一 Wi-Fi 局域网
- 都已 clone mlink 仓库并拉到最新（`git pull`）
- Rust 工具链已安装（`cargo` 可用）

## 第一步：两边启动 daemon（双通道模式）

**机器 A（终端执行）：**
```bash
cd /path/to/mlink
git pull
MLINK_DAEMON_TRANSPORT=dual cargo run -p mlink-cli -- dev --transport tcp
```

**机器 B（终端执行）：**
```bash
cd /path/to/mlink
git pull
MLINK_DAEMON_TRANSPORT=dual cargo run -p mlink-cli -- dev --transport tcp
```

两边浏览器会自动打开 debug UI。如果没自动打开，看终端输出的端口号，手动访问 `http://127.0.0.1:<端口>`。

## 第二步：加入同一房间

两边都在 debug UI 页面上：
1. 输入房间号 `123456`
2. 点 Join

看到 room_state 事件说明加入成功。

## 第三步：基础通信测试

1. 机器 A 在 UI 输入消息发送
2. 机器 B 确认收到
3. 反向测试：B 发 A 收
4. **通过标准**：双向消息无丢失

## 第四步：Transport 面板验证

在 UI 底部找到 **Transport 面板**（如果没有说明 daemon 版本不对，重新 `git pull`）。

面板应显示：
- 每个 peer 的 link 列表（BLE/TCP）
- 每条 link 的角色（Active / Standby）
- RTT 和错误率

## 第五步：手动切换测试

1. 点 **Force TCP** → 确认 Active 变为 TCP link → 发消息确认通畅
2. 点 **Force BLE** → 确认 Active 变为 BLE link → 发消息确认通畅
3. 点 **Auto** → 取消手动覆盖，恢复自动调度
4. **通过标准**：每次切换后消息不丢、不重复、延迟无明显增大

## 第六步：断链 failover 测试

测试目标：断一个通道，另一个自动接上。

**方法 1（推荐）：通过 UI disconnect**
1. 在 Transport 面板找到 Active link 的 "drop" 按钮
2. 点击断开当前 Active link
3. 观察：Standby 是否自动升为 Active
4. 发消息确认通畅
5. **通过标准**：切换日志（Switch Log）显示 `active_failed` 事件，消息无丢失

**方法 2：物理断网**
1. 关闭 Wi-Fi（断 TCP）→ 观察 BLE 是否接管
2. 重新开 Wi-Fi → 观察 TCP 是否重新建立为 Standby
3. 发消息全程不断

## 第七步：稳定性压测

1. 在 UI 找到 **Stability Test** 面板
2. 选目标 peer
3. 输入数量 `1000`（默认值）
4. 点 **Run**
5. 等待完成，查看结果：

| 指标 | 通过标准 |
|------|---------|
| Loss rate | < 1% |
| Avg latency | < 100ms（TCP）/ < 500ms（BLE） |
| P99 latency | < 500ms（TCP）/ < 2000ms（BLE） |
| Transport switches | 记录次数即可，无固定标准 |

## 第八步：带宽差异测试

验证 BLE→TCP 加速、TCP→BLE 降速续传。

1. Force BLE → 发送大量消息（快速点发送 10 次）
2. 中途切 Force TCP → 观察发送速度是否加快
3. 反向：Force TCP → 发大量消息 → 中途切 Force BLE → 确认消息继续送达（变慢但不丢）

## 故障排查

| 问题 | 解决 |
|------|------|
| Transport 面板不显示 | 确认 `git pull` 拉到 commit `38d0988` 以上 |
| 只有一个 link | 检查 `MLINK_DAEMON_TRANSPORT=dual` 是否设了 |
| 对端看不到 peer | 确认在同一 Wi-Fi；尝试 `mlink doctor` 检查网络 |
| Force BLE/TCP 按钮无反应 | 检查浏览器控制台是否有 WS 错误 |
| 1000 ping 全 fail | 确认对端 daemon 在运行且已 join 同一房间 |

## 结果记录模板

```
日期：
机器 A：（型号/OS）
机器 B：（型号/OS）
网络：（Wi-Fi 名）

基础通信：通过 / 失败
Force TCP 切换：通过 / 失败
Force BLE 切换：通过 / 失败
断链 failover：通过 / 失败
1000 ping 结果：
  - Loss: ___%
  - Avg: ___ms
  - P99: ___ms
  - Switches: ___次
带宽切换：通过 / 失败
备注：
```
