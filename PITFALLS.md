# macOS BLE 开发踩坑记录

本文档汇总 mlink 在 macOS 上开发 BLE 功能时踩过的坑，便于后来者避开。

---

## 坑 1：终端蓝牙权限

**现象**
btleplug 执行无过滤扫描返回 0 个设备，日志无错误。

**原因**
从终端（Warp / Cursor / Terminal.app）启动的进程会继承终端自身的权限。若终端未加入系统的蓝牙授权列表，CoreBluetooth 静默拒绝扫描。

**解法**
- 系统设置 → 隐私与安全性 → 蓝牙 → 添加当前终端 app（Warp / Cursor / Terminal.app）
- 或将程序打包为 `.app` bundle 后运行（bundle 会触发系统独立的权限弹窗）

**相关文件**
- `INSTALL.md`
- `scripts/`（打包脚本目录）

---

## 坑 2：macOS service UUID overflow area

**现象**
扫描能发现设备，但 `PeripheralProperties.services` 始终是空数组。

**原因**
macOS CoreBluetooth 会把 peripheral 广播的 128-bit service UUID 放进 advertisement 的 **overflow area**。btleplug 暴露的 `props.services` 不会解析 overflow area 里的 UUID，导致上层看不到目标 service。

**解法**
使用 `ScanFilter { services: vec![TARGET_UUID] }` 进行扫描。CoreBluetooth 在底层匹配时会读取 overflow area，凡是通过 ScanFilter 的设备即为目标，上层无需再手动校验 `props.services`。

**相关文件**
- `crates/mlink-core/src/core/scanner.rs`
- `crates/mlink-core/src/transport/ble.rs`

---

## 坑 3：macOS 广播名不可自定义

**现象**
btleplug 扫描到的 `local_name` 是系统蓝牙名（例如"朱庆宇的Mac Studio"），而不是代码中设置的自定义名。

**原因**
macOS 不允许应用覆盖系统蓝牙名。`CBAdvertisementDataLocalNameKey` 只会写入 scan response 的补充数据，而 btleplug 的 `peripheral.name` 优先返回缓存的 GAP 名（即系统名）。

**影响**
无法通过广播名携带房间 hash。`name#<hex>` 之类的方案在 macOS 上不 work。

**解法**
放弃用广播名传递数据，改为连接建立后通过 GATT characteristic 读写进行信息交换。

**相关文件**
- `crates/mlink-core/src/transport/ble.rs`

---

## 坑 4：CBPeripheralManager 需要专用 dispatch queue

**现象**
Peripheral 启动后 `wait_for_powered_on` 10 秒超时，delegate 回调永远不触发。

**原因**
`CBPeripheralManager` 初始化时若 queue 参数传 `null`，表示回调派发到主线程的 dispatch queue。但 tokio 运行时占据主线程且不跑 RunLoop，回调无法执行。

**解法**
创建专用 dispatch queue 传给 `initWithDelegate:queue:`：

```c
dispatch_queue_create("com.mlink.peripheral", NULL)
```

**相关文件**
- `crates/mlink-core/src/transport/ble.rs`（Peripheral 相关代码）

---

## 坑 5：addService 必须等 didAddService 回调后才能 startAdvertising

**现象**
Peripheral 启动无报错，但对端 `discover_services` 找不到 mlink service。

**原因**
代码在 `addService:` 之后立即调用 `startAdvertising:`，未等待 `peripheralManager:didAddService:error:` 回调。此时 service 尚未注册进 GATT DB，广播出去的 service 列表为空。

**解法**
严格遵循事件顺序：

```
PoweredOn
  → addService:
  → didAddService: 回调
  → startAdvertising:
  → didStartAdvertising: 回调
```

任何一步都必须等待对应回调触发后再进行下一步。

**相关文件**
- `crates/mlink-core/src/transport/ble.rs`（Peripheral delegate 实现）

---

## 坑 6：双方同时作为 central 连接导致 BLE 冲突

**现象**
两台 Mac 都能发现对方（scan 正常），但 `transport.connect` 立刻返回 `peer disconnected`。

**原因**
两台设备都同时运行 scanner + peripheral，各自发现对方后都尝试作为 central 去连接对方。BLE 不允许同一对设备建立两条方向相反的连接，双方的 connect 互相打断。

**解法**
用 app_uuid 比大小决定角色——UUID 小的主动连接（central），UUID 大的等待被连（peripheral only）。只有一方发起连接，打破对称。

**相关文件**
- `crates/mlink-cli/src/main.rs`（cmd_serve 的 auto-connect 逻辑）

---

## 坑 7：btleplug 扫描拿不到 peripheral 的自定义广播名

**现象**
扫描到的设备 name 为空字符串或系统蓝牙名，不是代码设置的 `hostname#room_hash`。

**原因**
与坑 3 相关。btleplug 在 macOS 上通过 `peripheral.name` 获取名字，返回的是缓存的 GAP 名。`CBAdvertisementDataLocalNameKey` 的值只在 `didDiscoverPeripheral:advertisementData:` 的字典里能拿到，但 btleplug 没有暴露这份数据。

**影响**
无法在扫描阶段通过广播名传递 room hash，`parse_room_hash_from_name` 在 macOS 上始终解析失败。

**解法**
改为连接后通过 GATT characteristic 握手交换 room code，不再依赖广播名。

**相关文件**
- `crates/mlink-core/src/transport/ble.rs`

---

## 坑 8：mDNS 自发现（TCP transport 连自己）

**现象**
两台 Mac 各自跑 `mlink --transport tcp chat 567892`，日志显示 peer_app 等于自己的 uuid，自己连了自己，没连到对方。

**原因**
mDNS listen 注册服务后，discover 能发现自己的广播。TXT 记录里没有 uuid 字段，无法在发现阶段过滤自己。

**解法**
listen 时 TXT 加 `uuid=<app_uuid>`，discover 时跳过 uuid 等于自己的服务。

**相关文件**
- `crates/mlink-core/src/transport/tcp.rs`

---

## 坑 9：多网卡产生重复 peer（TCP transport）

**现象**
同一台设备因多个网卡（127.0.0.1、192.168.21.x、192.168.2.x）在 mDNS 中广播多个地址，discover 返回同一个逻辑节点的多条 DiscoveredPeer，导致重复连接尝试和大量无效握手。

**原因**
`enable_addr_auto()` 把所有网卡 IP 都注入 mDNS，discover 按 `ip:port` 作为 peer id，不同 IP 被当成不同 peer。

**解法**
discover 时按 TXT 中的 uuid 去重，同一 uuid 只保留一个 peer（优先非 loopback 地址）；过滤掉 127.0.0.1。

**相关文件**
- `crates/mlink-core/src/transport/tcp.rs`

---

## 坑 10：GitHub Actions macOS 13 runner 排队卡死

**现象**
CI 的 `x86_64-apple-darwin` job 在 `macos-13` runner 上排队超过 12 小时不执行。

**原因**
GitHub 免费版 macOS 13 runner 资源极少，正在被淘汰。

**解法**
去掉 macOS x86 target，只保留 macOS arm64（`macos-14` runner）。x86 用户通过 Rosetta 2 运行 arm64 二进制。

**相关文件**
- `.github/workflows/release.yml`

---

## 坑 11：Linux CI 缺少 libdbus-1-dev

**现象**
Linux x86 构建 exit code 101，btleplug 编译失败。

**原因**
btleplug 在 Linux 通过 dbus crate 链接系统 libdbus，CI runner 没装。

**解法**
workflow 加 `apt-get install -y pkg-config libdbus-1-dev`。

**相关文件**
- `.github/workflows/release.yml`

---

## 坑 12：Windows 编译 ipc.rs 失败

**现象**
Windows 构建报 `unresolved imports tokio::net::UnixListener, UnixStream`。

**原因**
Unix socket 类型在 Windows 上不存在，ipc.rs 无条件导入了它们。

**解法**
ipc 模块和相关测试加 `#[cfg(unix)]` 门控。

**相关文件**
- `crates/mlink-core/src/transport/mod.rs`
- `crates/mlink-core/tests/phase3_tests.rs`
