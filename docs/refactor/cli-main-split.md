# mlink-cli/src/main.rs 拆分设计

> 目标文件：`/Users/zhuqingyu/project/mlink/crates/mlink-cli/src/main.rs`
> 当前总行数：**1683 行**
> 约束：拆分后每个文件不超过 **200 行**（含注释与空行）

---

## 1. 当前结构分析

### 1.1 文件头 & 主入口（1 – 140）

| 区段 | 行号 | 内容 | 说明 |
|---|---|---|---|
| use 区 | 1 – 17 | 导入（std / clap / tokio / mlink-core / mlink-daemon / libc） | |
| `fn main()` | 19 – 93 | `#[tokio::main]`：初始化 tracing → 解析 CLI → 特殊分派 daemon/dev → 外层 `select!` 对 ctrl-c / `run()`，ctrl-c 走 `libc::_exit(0)` | 75 行，含大段注释解释为什么需要外层 ctrl-c guard |
| `async fn run(cli)` | 95 – 140 | 按 `Commands` 分派到 `cmd_*` 函数；无子命令路径：自动生成或接收 6 位 room code 后调 `cmd_serve` | 46 行 |

### 1.2 辅助工具函数（142 – 164）

| 函数 | 行号 | 职责 |
|---|---|---|
| `fn host_name()` | 142 – 147 | 读 `HOSTNAME` / 兜底 `hostname_sysctl()` / 兜底 `"mlink-node"` |
| `fn hostname_sysctl()` | 149 – 154 | 读 `HOST` / `COMPUTERNAME` 环境变量 |
| `async fn build_node()` | 156 – 163 | 用 `host_name()` 构造 `NodeConfig` 并 `Node::new(...).await` |

### 1.3 长生命周期子命令（168 – 1197）

| 子命令 | 行号 | 行数 | 职责 |
|---|---|---|---|
| `cmd_serve` | 168 – 575 | **408** | 长时 host：peripheral/TCP listen → 后台 accept → scanner（TCP only，BLE host 是 peripheral-only）→ 主 `select!` 驱动 dial/accept/NodeEvent，BLE 对称拨号仲裁、重试退避、wire_id↔app_uuid 去重 |
| `cmd_chat` | 581 – 966 | **386** | 等同 `cmd_serve` 骨架 + stdin reader + per-peer `spawn_peer_reader` + 打印 `[name] text` |
| `cmd_join` | 973 – 1197 | **225** | 纯 central：只跑 scanner + dial，不广告、不 accept；`chat=true` 时叠加 stdin reader 与 per-peer reader |

这三个函数是 **main.rs 70% 的体量**，共用同一套骨架（见 §2 抽取点）。

### 1.4 短子命令（1199 – 1592）

| 子命令 | 行号 | 行数 | 职责 |
|---|---|---|---|
| `cmd_room` | 1199 – 1261 | 63 | `room new/join/leave/list/peers`——前两个委托 `cmd_serve`，后三个只操作本地 `RoomManager` |
| `fn validate_room_code` | 1263 – 1265 | 3 | 委托 `mlink_cli::validate_room_code` |
| `cmd_send_room` | 1269 – 1350 | 82 | 单次：起节点 → scan → 每个 peer `connect_peer` → 发 text 或 file（file 未实现） |
| `send_file_to_peers` | 1352 – 1360 | 9 | stub：返回 `"not yet implemented"` |
| `cmd_listen` | 1362 – 1367 | 6 | 委托 `cmd_serve(None, kind)` |
| `fn transport_label` | 1369 – 1374 | 6 | `TransportKind` → `"ble"` / `"tcp"` |
| `cmd_scan` | 1378 – 1398 | 21 | legacy：`transport.discover()` + 打印 |
| `fn print_bluetooth_permission_hint` | 1404 – 1410 | 7 | 打印 macOS 蓝牙权限提示 |
| `fn print_peer_list` | 1412 – 1425 | 14 | 格式化打印 peer 表 |
| `cmd_connect` | 1427 – 1444 | 18 | legacy：scan → 找指定 id → `connect_peer` |
| `cmd_ping` | 1446 – 1466 | 21 | legacy：connect → `send_raw(Heartbeat)` → `recv_raw` → 打印 RTT |
| `cmd_status` | 1468 – 1479 | 12 | 打印节点 state / app_uuid / peer 列表 |
| `cmd_trust` | 1481 – 1507 | 27 | `trust list/remove` 操作 `TrustStore` |
| `cmd_doctor` | 1509 – 1565 | 57 | 诊断：平台/架构、BLE discover 或 TCP loopback+mDNS probe、trust store |
| `cmd_daemon` | 1571 – 1575 | 5 | 委托 `mlink_daemon::run()` 并转错误 |
| `cmd_dev` | 1581 – 1592 | 12 | `bind_and_prepare` → `open_url` → `await_shutdown` |

### 1.5 底层工具（1597 – 1682）

| 项 | 行号 | 职责 |
|---|---|---|
| `fn open_url` | 1597 – 1625 | 平台分发（macOS `open` / Linux `xdg-open` / Windows `cmd /C start` / 其他 `Err`） |
| `async fn tcp_loopback_check` | 1627 – 1648 | `bind 127.0.0.1:0` → 自连 → 2s 超时 accept，用于 doctor |
| `const CONNECT_TIMEOUT` | 1656 | 15s，dial/accept 外层超时 |
| `fn random_backoff` | 1661 – 1669 | 用 wall-clock nanos 派生 1000–3000 ms 随机延迟 |
| `mod tests` | 1671 – 1683 | 单元测试：`random_backoff` 窗口断言 |

---

## 2. 可抽取的共享片段（`cmd_serve` / `cmd_chat` / `cmd_join` 重复骨架）

以下 9 块逻辑在三个长函数里高度重复，是拆分的最大杠杆：

| # | 片段 | 描述 | 出现位置 |
|---|---|---|---|
| A | `spawn_listen_task(kind, node, local_name, room_hash?) → mpsc<Box<dyn Connection>>` | 起 peripheral/TCP listen 的 tokio 后台任务，含 macOS `wait_for_central` 循环 + non-macos 兜底、BLE `print_bluetooth_permission_hint` | serve 203–288、chat 599–678 |
| B | `spawn_scanner_task(kind, app_uuid, hash, unsee_rx) → mpsc<DiscoveredPeer>` | 按 kind 构造 `Scanner`，设置 `room_hashes` + `unsee_channel`，`discover_loop` | serve 295–334、chat 684–713、join 992–1017 |
| C | `spawn_stdin_task() → mpsc<String>` | 从 tokio stdin 按行读，空行跳过，EOF/err 退出 | chat 719–741、join 1020–1046 |
| D | `ble_role_check(kind, local_app_uuid, peer.metadata) → enum Skip/DialAsCentral/SymmetricFallback` | BLE 对称拨号仲裁（`should_dial_as_central`），只打印信息、不分支 | serve 410–437、chat 803–830 |
| E | `dial_with_timeout(node, transport, peer) → Result<String, MlinkError>` | `tokio::time::timeout(CONNECT_TIMEOUT, node.connect_peer(...))` + 转错 | serve 458–469、chat 844–855、join 1119–1130 |
| F | `accept_with_timeout(node, conn, label, wire_id) → Result<String, MlinkError>` | 同上，`accept_incoming` 版本 | serve 519–530、chat 894–905 |
| G | `retry_via_unsee(peer_wire_id, unsee_tx)` | 随机退避后把 wire_id 回吐给 scanner | serve 488–500、chat 875–881、join 1150–1156 |
| H | `ConnectionBook` / `ConnTracker` 结构体封装：`engaged_wire_ids / attempts / connected_inbound / connected_peers / wire_to_app / MAX_RETRIES=3` 及其方法（`should_skip_dial`、`mark_engaged`、`on_dial_ok`、`on_dial_err`、`on_accept_ok`、`on_disconnect`） | serve 348–367、chat 748–762、join 1053–1060 与随后的 match 分支 | 全部 3 处 |
| I | NodeEvent 分发的 `PeerConnected / PeerDisconnected / PeerLost / MessageReceived` 打印 + 表维护（含 reader JoinHandle abort） | serve 550–569、chat 929–957、join 1160–1188 |

抽出这 9 块后：
- `cmd_serve` 降至约 **110 行**（控制流 + 组合）
- `cmd_chat` 降至约 **140 行**（多一条 stdin 广播分支 + peer_names + readers 管理）
- `cmd_join` 降至约 **100 行**

---

## 3. 拆分方案

### 3.1 目标目录结构

```
crates/mlink-cli/src/
├── lib.rs                         # 不动（已导出 Cli/Commands/... 147 行）
├── main.rs                        # ≈ 60 行：只保留 fn main + 外层 ctrl-c guard + mod 声明
├── run.rs                         # ≈ 70 行：async fn run(cli) 的命令分派
├── node_build.rs                  # ≈ 35 行：host_name / hostname_sysctl / build_node / transport_label / validate_room_code
├── util/
│   ├── mod.rs                     # ≈ 10 行：pub mod 声明 + re-export
│   ├── backoff.rs                 # ≈ 45 行：CONNECT_TIMEOUT + random_backoff + unit test
│   ├── open_url.rs                # ≈ 35 行：open_url 跨平台
│   └── printing.rs                # ≈ 30 行：print_peer_list + print_bluetooth_permission_hint
├── session/                       # serve/chat/join 用的共享组件（§2 的 A–I）
│   ├── mod.rs                     # ≈ 15 行：pub mod + pub use
│   ├── listen.rs                  # ≈ 120 行：spawn_listen_task（§2 A）
│   ├── scanner.rs                 # ≈ 70 行：spawn_scanner_task（§2 B）
│   ├── stdin.rs                   # ≈ 40 行：spawn_stdin_task（§2 C）
│   ├── role.rs                    # ≈ 55 行：ble_role_check + 枚举（§2 D）
│   ├── dial.rs                    # ≈ 80 行：dial_with_timeout + accept_with_timeout + retry_via_unsee（§2 E/F/G）
│   ├── tracker.rs                 # ≈ 130 行：ConnTracker 结构体 + 方法（§2 H）
│   └── events.rs                  # ≈ 70 行：handle_node_event 分发（§2 I）
└── commands/
    ├── mod.rs                     # ≈ 20 行：pub mod + pub use 每个 cmd_*
    ├── serve.rs                   # ≈ 115 行：cmd_serve（用 session::*）
    ├── chat.rs                    # ≈ 150 行：cmd_chat
    ├── join.rs                    # ≈ 110 行：cmd_join
    ├── room.rs                    # ≈ 70 行：cmd_room
    ├── send.rs                    # ≈ 100 行：cmd_send_room + send_file_to_peers（stub）
    ├── listen.rs                  # ≈ 15 行：cmd_listen（委托 serve）
    ├── scan.rs                    # ≈ 30 行：cmd_scan（legacy）
    ├── connect.rs                 # ≈ 30 行：cmd_connect（legacy）
    ├── ping.rs                    # ≈ 35 行：cmd_ping（legacy）
    ├── status.rs                  # ≈ 20 行：cmd_status
    ├── trust.rs                   # ≈ 45 行：cmd_trust
    ├── doctor.rs                  # ≈ 85 行：cmd_doctor + tcp_loopback_check（只被 doctor 用）
    ├── daemon.rs                  # ≈ 15 行：cmd_daemon
    └── dev.rs                     # ≈ 25 行：cmd_dev
```

> 每个文件都 ≤200 行，绝大多数在 30 – 130 行区间。

### 3.2 文件职责详表

#### `main.rs`（≈60 行）
- 保留 `#[tokio::main] async fn main() -> ExitCode` 外壳与 ctrl-c guard
- 保留对 `Commands::Daemon` / `Commands::Dev` 的**早分派**（这两条命令不能走外层 `libc::_exit(0)`，需各自走干净退出）
- `mod run; mod node_build; mod util; mod session; mod commands;`
- 导出：无（bin crate）

#### `run.rs`（≈70 行）
- `pub async fn run(cli: Cli) -> Result<(), MlinkError>`
- 只做 `match cli.command`，逐条调用 `commands::*`
- 导出：`pub async fn run`

#### `node_build.rs`（≈35 行）
- `pub(crate) fn host_name`、`fn hostname_sysctl`
- `pub(crate) async fn build_node() -> Result<Node, MlinkError>`
- `pub(crate) fn transport_label(kind) -> &'static str`
- `pub(crate) fn validate_room_code(code) -> Result<(), MlinkError>`（包装 `mlink_cli::validate_room_code`）
- 原因：这些是几乎所有 `cmd_*` 都要用的最小公共工具，集中放一处

#### `util/backoff.rs`（≈45 行）
- `pub(crate) const CONNECT_TIMEOUT: Duration`
- `pub(crate) fn random_backoff() -> Duration`
- **迁移原 `mod tests`**：`random_backoff_stays_within_window`

#### `util/open_url.rs`（≈35 行）
- `pub(crate) fn open_url(url: &str) -> std::io::Result<()>`
- 保留 `#[cfg(target_os = ...)]` 分支

#### `util/printing.rs`（≈30 行）
- `pub(crate) fn print_peer_list(peers: &[DiscoveredPeer])`
- `pub(crate) fn print_bluetooth_permission_hint()`

#### `session/listen.rs`（≈120 行）
```rust
pub(crate) struct ListenHandle {
    pub rx: mpsc::Receiver<Box<dyn Connection>>,
}
pub(crate) fn spawn_listen_task(
    kind: TransportKind,
    local_name: String,
    app_uuid: String,
    room_hash_bytes: Option<[u8; 8]>,
) -> ListenHandle;
```
- 内部分 `TransportKind::Ble`（macOS `wait_for_central` 循环 + 非 macOS 兜底）和 `TransportKind::Tcp` 两条 `tokio::spawn` 分支
- **直接搬运 serve 203–288、chat 599–678 的代码**，只改读参数来源
- 因为 BLE 分支含 `#[cfg(target_os = "macos")]`，行数偏多，接近但不超过 200

#### `session/scanner.rs`（≈70 行）
```rust
pub(crate) struct ScannerHandle {
    pub peer_rx: mpsc::Receiver<DiscoveredPeer>,
    pub unsee_tx: mpsc::Sender<String>,
}
pub(crate) fn spawn_scanner_task(
    kind: TransportKind,
    app_uuid: String,
    room_hash_bytes: [u8; 8],
) -> ScannerHandle;

// 仅保留 unsee_tx、不起任务的变体（serve 在 BLE host-only 或无 room 时用）
pub(crate) fn disabled_scanner() -> ScannerHandle;
```
- `disabled_scanner()` 返回一个 peer_rx 立刻关闭、unsee_tx 一个无接收端的兜底实例，让调用方在 `select!` 中安全使用

#### `session/stdin.rs`（≈40 行）
```rust
pub(crate) fn spawn_stdin_task() -> mpsc::Receiver<String>;
pub(crate) fn disabled_stdin() -> mpsc::Receiver<String>;
```
- `disabled_stdin()` 返回一个立刻关闭的 rx，让 `cmd_join` 的非 chat 模式下 `recv()` 永远返回 `None`（与原实现等价）

#### `session/role.rs`（≈55 行）
```rust
pub(crate) enum BleRole { DialAsCentral, SkipDial, SymmetricFallback }
pub(crate) fn decide_ble_role(
    kind: TransportKind,
    local_app_uuid: &str,
    peer: &DiscoveredPeer,
    log_prefix: &str,     // "serve" / "chat"
) -> BleRole;
```
- 封装 410–437 的 match + `eprintln!` 路径
- `log_prefix` 保留原来的日志差异（`[mlink:conn] serve: ...` vs `chat: ...`）

#### `session/dial.rs`（≈80 行）
```rust
pub(crate) async fn dial_with_timeout(
    node: &Node,
    transport: &mut dyn Transport,
    peer: &DiscoveredPeer,
) -> Result<String, MlinkError>;

pub(crate) async fn accept_with_timeout(
    node: &Node,
    conn: Box<dyn Connection>,
    label: &'static str,
    wire_id: String,
) -> Result<String, MlinkError>;

pub(crate) fn schedule_retry_unsee(
    unsee_tx: mpsc::Sender<String>,
    wire_id: String,
);  // 内部 spawn + sleep(random_backoff()) + send
```

#### `session/tracker.rs`（≈130 行）
```rust
pub(crate) struct ConnTracker {
    engaged_wire_ids: HashSet<String>,
    connected_inbound: HashSet<String>,
    connected_peers: HashSet<String>,
    wire_to_app: HashMap<String, String>,
    attempts: HashMap<String, u8>,
}
impl ConnTracker {
    pub(crate) const MAX_RETRIES: u8 = 3;
    pub(crate) fn new() -> Self;

    // 返回 Skip 原因的枚举，调用方负责打日志（日志文本在 serve/chat/join 略有差异）
    pub(crate) fn should_skip_dial(&mut self, peer: &DiscoveredPeer) -> SkipReason;
    pub(crate) fn bump_attempt(&mut self, wire_id: &str) -> Option<u8>; // None = exceeded
    pub(crate) fn mark_engaged(&mut self, wire_id: &str);
    pub(crate) fn on_dial_ok(&mut self, wire_id: &str, app_uuid: &str);
    pub(crate) fn on_dial_err(&mut self, wire_id: &str);
    pub(crate) fn on_room_mismatch(&mut self, wire_id: &str);
    pub(crate) fn on_accept_ok(&mut self, wire_id: &str, app_uuid: &str);
    pub(crate) fn on_accept_err(&mut self, wire_id: &str);
    pub(crate) fn on_peer_connected(&mut self, app_uuid: String);
    pub(crate) fn on_peer_disconnected(&mut self, app_uuid: &str);
}

pub(crate) enum SkipReason {
    Proceed,
    AlreadyInbound,
    AlreadyEngaged,
    AppUuidAlreadyConnected(String),
}
```
- 把三个函数里散落的 HashSet/HashMap 操作统一起来；Trait impl 轻量，方法名映射清楚

#### `session/events.rs`（≈70 行）
```rust
pub(crate) struct EventCtx<'a> {
    pub tracker: &'a mut ConnTracker,
    pub readers: Option<&'a mut HashMap<String, JoinHandle<()>>>,  // chat/join 用；serve 传 None
    pub peer_names: Option<&'a mut HashMap<String, String>>,
    pub print_messages: bool,
}
pub(crate) fn handle_node_event(ev: NodeEvent, ctx: &mut EventCtx);
```
- 统一处理 4 种 `NodeEvent`：connected/disconnected/lost/message
- `serve` 传 `readers=None, print_messages=false`，`chat` / `join(chat=true)` 全开

#### `commands/serve.rs`（≈115 行）
- `pub async fn cmd_serve(room_code: Option<String>, kind: TransportKind) -> Result<(), MlinkError>`
- 组合 `spawn_listen_task` + `spawn_scanner_task`（BLE host 用 `disabled_scanner`）+ `ConnTracker` + 主 select 循环，每个 arm 调 session::* 的工具函数
- 仅保留控制流（顺序、select!、`node.start/stop`）

#### `commands/chat.rs`（≈150 行）
- `pub async fn cmd_chat(code: String, kind: TransportKind) -> Result<(), MlinkError>`
- 组合 listen + scanner + **stdin** + `ConnTracker` + `readers`/`peer_names` + events
- 只保留控制流 + stdin 广播分支（`node.peers().await` + `send_raw`）

#### `commands/join.rs`（≈110 行）
- `pub async fn cmd_join(code: String, chat: bool, kind: TransportKind) -> Result<(), MlinkError>`
- 组合 scanner（join 必有）+ `stdin`（可选 `spawn_stdin_task` / `disabled_stdin`）+ `ConnTracker` + events
- **无 listen / 无 BLE 角色仲裁**（join 永远 dial）

#### `commands/room.rs`（≈70 行）
- `pub async fn cmd_room(action: RoomAction, kind: TransportKind) -> Result<(), MlinkError>`
- `RoomAction::New / Join` 调用 `commands::serve::cmd_serve(Some(code), kind)`
- `Leave / List / Peers` 操作本地 `RoomManager`

#### `commands/send.rs`（≈100 行）
- `pub async fn cmd_send_room(code, file, message, kind)`
- `async fn send_file_to_peers(...)`（私有 stub）

#### `commands/listen.rs`（≈15 行）
- `pub async fn cmd_listen(kind)` → `cmd_serve(None, kind)`

#### `commands/scan.rs`（≈30 行）
- legacy：起 transport → `discover()` → `print_peer_list`

#### `commands/connect.rs`（≈30 行）
- legacy：discover → find by peer_id → `build_node` → `connect_peer`

#### `commands/ping.rs`（≈35 行）
- legacy：discover → connect → `send_raw(Heartbeat)` → `recv_raw` → 打印 RTT

#### `commands/status.rs`（≈20 行）
- `build_node` → 打印 state/app_uuid/peers

#### `commands/trust.rs`（≈45 行）
- `TrustStore` 的 list / remove

#### `commands/doctor.rs`（≈85 行）
- `cmd_doctor` + **私有** `async fn tcp_loopback_check`（只此一家调用，跟着走不额外暴露）

#### `commands/daemon.rs`（≈15 行）
- `pub async fn cmd_daemon()` → `mlink_daemon::run()`

#### `commands/dev.rs`（≈25 行）
- `pub async fn cmd_dev() -> Result<(), mlink_daemon::DaemonError>`
- 用 `util::open_url::open_url`

### 3.3 各 `mod.rs` 的 re-export

- `util/mod.rs`：`pub mod backoff; pub mod open_url; pub mod printing;`
- `session/mod.rs`：`pub mod listen; pub mod scanner; pub mod stdin; pub mod role; pub mod dial; pub mod tracker; pub mod events;`
- `commands/mod.rs`：每个子模块 `pub mod X;`，并 `pub use X::cmd_X;`，`run.rs` 只需 `use crate::commands::*;`

---

## 4. 模块间依赖

### 4.1 共享类型

| 类型 | 来源 | 被谁依赖 |
|---|---|---|
| `Cli / Commands / RoomAction / TrustAction / TransportKind` | `mlink_cli::*`（`lib.rs`，不动） | `run.rs`、几乎所有 `commands/*` |
| `Node / NodeConfig / NodeEvent` | `mlink_core::core::node` | `node_build.rs`、多数 `commands/*`、`session/events.rs` |
| `DiscoveredPeer / Connection / Transport` | `mlink_core::transport` | `session/*`、`commands/*` |
| `MlinkError` | `mlink_core::protocol::errors` | 到处都用（返回类型） |
| `MessageType` | `mlink_core::protocol::types` | `commands/{chat,join,send,ping}.rs` |
| `BleTransport / TcpTransport` | `mlink_core::transport::{ble,tcp}` | `session/listen.rs`、`session/scanner.rs`、`commands/*`（legacy） |
| `should_dial_as_central` | `mlink_core::transport::ble` | `session/role.rs` |
| `MacPeripheralConnection` | `mlink_core::transport::peripheral`（macOS） | `session/listen.rs` |

### 4.2 内部依赖（crate 内）

```
main.rs
 └─→ run.rs
      └─→ commands/*
           ├─→ node_build.rs
           ├─→ util/{backoff,open_url,printing}.rs
           └─→ session/{listen,scanner,stdin,role,dial,tracker,events}.rs
                └─→ util/backoff.rs          (CONNECT_TIMEOUT / random_backoff)
                └─→ util/printing.rs         (print_bluetooth_permission_hint, 仅 listen.rs 用)
                └─→ node_build.rs            (transport_label, 仅 events/dial 的日志)
```

无循环依赖。`session/*` 完全不依赖 `commands/*`。

### 4.3 `main.rs` 中必须保留

- `#[tokio::main] async fn main()` 本体
- **对 `Commands::Daemon` 与 `Commands::Dev` 的早分派**（ctrl-c guard 之前）——原因见 main.rs 27–51 的注释：这两条命令自带优雅退出与清理 `~/.mlink/daemon.json`，不能走 `libc::_exit(0)` 路径
- 外层 `tokio::select!` 对 `signal::ctrl_c()` + `run(cli)` 的竞速 + `libc::_exit(0)`
- `mod` 声明链（`mod run; mod node_build; mod util; mod session; mod commands;`）

---

## 5. 迁移步骤（按顺序执行，逐步降低出错风险）

**总原则**：每步末尾跑 `cargo check -p mlink-cli` + `cargo test -p mlink-cli`，确认绿再进下一步。每步只动一个 mod，单步 diff 可控。

1. **step 1 — util 层（叶子，零依赖）**
   - 新建 `src/util/{mod.rs, backoff.rs, open_url.rs, printing.rs}`
   - 从 main.rs 搬 `CONNECT_TIMEOUT / random_backoff / mod tests / open_url / print_peer_list / print_bluetooth_permission_hint`
   - 在 main.rs 加 `mod util;` 并把原函数替换为 `util::...` 调用
   - 验收：`cargo test -p mlink-cli` 过（`random_backoff_stays_within_window` 覆盖）

2. **step 2 — node_build**
   - 新建 `src/node_build.rs`，搬 `host_name / hostname_sysctl / build_node / transport_label / validate_room_code`
   - main.rs 加 `mod node_build;` 并替换调用
   - 验收：`cargo check`

3. **step 3 — commands/ 短子命令先行**（无共享骨架依赖）
   - 顺序：`daemon.rs → dev.rs → status.rs → trust.rs → scan.rs → connect.rs → ping.rs → room.rs（仅 Leave/List/Peers 部分，New/Join 先留 TODO）→ send.rs → listen.rs → doctor.rs`
   - 每搬一个就从 main.rs 删掉对应函数，在 main.rs 的 `run()` 改为调用 `commands::X::cmd_X`
   - 这一步不触碰 `cmd_serve/chat/join`
   - 验收：每搬一个跑 `cargo check`；全部搬完跑一遍手动冒烟（`mlink status`、`mlink doctor`、`mlink trust list`）

4. **step 4 — session/ 骨架抽取**（最高风险，逐个抽，不急）
   - 4.1 `session/scanner.rs`：抽 `spawn_scanner_task` + `disabled_scanner`。先在 `cmd_serve` 里替换，验证；再改 `cmd_chat` / `cmd_join`
   - 4.2 `session/listen.rs`：抽 `spawn_listen_task`（含 `#[cfg(target_os = "macos")]` 分支）。先 serve 后 chat
   - 4.3 `session/stdin.rs`：抽 `spawn_stdin_task / disabled_stdin`。先 chat 后 join
   - 4.4 `session/role.rs`：抽 `decide_ble_role`
   - 4.5 `session/dial.rs`：抽 `dial_with_timeout / accept_with_timeout / schedule_retry_unsee`
   - 4.6 `session/tracker.rs`：抽 `ConnTracker`（最大头、动静最大，放最后）
   - 4.7 `session/events.rs`：抽 `handle_node_event`
   - 每一小步完成后**都要做一次双机手动联调**（BLE + TCP 各一次），因为这些路径没有单测覆盖
   - 验收：serve/chat/join 三条命令行为与拆分前完全一致（参考 TEST_PLAN.md）

5. **step 5 — 把 cmd_serve / cmd_chat / cmd_join 搬进 commands/**
   - 此时函数体已经瘦到 100–150 行，直接搬即可
   - main.rs 删掉这三个函数
   - 验收：`cargo check` + 手动跑一遍典型流（`mlink` 生成房号 / 另一端 `mlink join <code>` / `mlink chat`）

6. **step 6 — 提取 run.rs**
   - 新建 `src/run.rs`，搬 `async fn run(cli)`
   - main.rs 只 `mod run;` 并 `run::run(cli).await`
   - 验收：`cargo check`

7. **step 7 — 清理 & 验证**
   - main.rs 应只剩 `fn main` + mod 声明，≤60 行
   - 跑 `cargo test -p mlink-cli`、`cargo clippy -p mlink-cli -- -D warnings`
   - 完整冒烟：`mlink`、`mlink <code>`、`mlink join <code>`、`mlink join --chat <code>`、`mlink chat <code>`、`mlink send <code> hi`、`mlink room list`、`mlink status`、`mlink doctor`、`mlink trust list`、`mlink daemon`、`mlink dev`

---

## 6. 风险点

| # | 风险 | 说明 | 缓解 |
|---|---|---|---|
| R1 | **`cmd_serve` / `cmd_chat` / `cmd_join` 没有单元测试** | 这是拆分最大风险面。三个函数里的微妙状态机（engaged/inbound/connected_peers/wire_to_app、attempts 重置时机、MAX_RETRIES 固定位、unsee 回吐顺序）非常容易在抽函数时行为漂移 | **迁移步骤 §5 step 4** 每小步都做双机手动联调；优先不改 `println!` / `eprintln!` 的**文本内容与顺序**（TEST_PLAN.md 的验收很可能匹配这些字符串） |
| R2 | **`println!` / `eprintln!` 的前缀在三条命令里略有不同**（`"[mlink:conn] serve: ..."` vs `chat:` vs `join:`） | `decide_ble_role` / `ConnTracker::should_skip_dial` 若把日志吸进去，前缀需要参数化 | 把日志**留在调用方**（`session/*` 只返回枚举/结果，日志由 `commands/*` 打），避免 prefix 耦合 |
| R3 | **`#[cfg(target_os = "macos")]` 分支** | `session/listen.rs` 的 BLE 分支含 macOS-only 代码，非 macOS 走兜底 | 保持和原文件一模一样的 cfg gate，不做"聪明改写"；linux/windows CI 如果有，要确认都能编过 |
| R4 | **`engaged_wire_ids` 的清理时机** | 拨号失败时要先 `remove`、再 schedule_retry；拨号成功则不 remove（由 disconnect 事件才移除）——这是握手去重的关键不变量 | `ConnTracker::on_dial_err` 内必须先 remove；`on_dial_ok` 不动 engaged（让 `on_peer_disconnected` 清理） |
| R5 | **`cmd_serve` 里 `ble_host_only` 时 `unsee_rx` 的显式 drop** | 原代码特意注释 "No scanner task → nothing will ever read `unsee_rx`. Drop it explicitly so sends fail-fast instead of filling the buffer"（332–334 行） | `disabled_scanner()` 内部必须 **drop(unsee_rx) 或 close() 以让 send 立即失败**；不要用 `let _ = unsee_rx;`（那样还活着） |
| R6 | **`cmd_join` 的 `stdin_tx` drop 时机**（chat=false 时 `drop(stdin_tx)`） | 让 `stdin_rx.recv()` 永远 None、arm 静默；不能漏 | `disabled_stdin()` 实现时构造完立即 drop 发送端 |
| R7 | **`main.rs` 特殊分派顺序** | `Commands::Daemon` / `Commands::Dev` 必须在外层 ctrl-c guard **之前**（否则会被 `_exit(0)` 吃掉它们的 `daemon.json` 清理） | 保留原顺序原注释；不要把它们移进 `run()` |
| R8 | **`CONNECT_TIMEOUT` 常量从 main.rs 底部迁移** | `const CONNECT_TIMEOUT` 声明在文件末尾（1656 行），但在 `cmd_serve` 458 行就被引用——Rust 允许这种向前引用，但迁走后多个文件都要引用 | 放 `util/backoff.rs`，`pub(crate) const CONNECT_TIMEOUT`，调用方 `use crate::util::backoff::CONNECT_TIMEOUT;` |
| R9 | **`ConnTracker` 封装带来的 borrow-checker 成本** | 原代码在单一作用域里同时读写 5 个 `HashMap/HashSet`，match 里也会连续 `insert/remove`。封进 struct 后方法内可能出现借用冲突 | 方法全取 `&mut self`，内部顺序化操作；必要时把 `HashSet` 的查改拆成 `contains` + 后续 `remove/insert` 两步，避免"读一个 set 同时改另一个"这类情况一上来就踩坑 |
| R10 | **`handle_node_event` 需要条件打印消息**（`cmd_join` 仅在 `chat=true` 时打印 `MessageReceived`） | 原代码用 `Ok(NodeEvent::MessageReceived { .. }) if chat` 的 match guard | `EventCtx.print_messages: bool` 控制；不要把 chat 判断渗透进 events.rs 本身 |

---

## 7. 预期收益

| 指标 | 拆分前 | 拆分后 |
|---|---|---|
| main.rs 行数 | 1683 | ≈60 |
| 单文件最大行数 | 1683 (main.rs) | ≈150 (commands/chat.rs 或 session/listen.rs) |
| `cmd_serve / cmd_chat / cmd_join` 骨架重复 | 3× 近乎同构 ≈ 1000 行 | 3× 控制流 ≈ 375 行 + 共享 session/ ≈ 600 行 |
| 单测可达面 | 只有 `random_backoff` | + `ConnTracker` 状态机、`decide_ble_role`、`validate_room_code`（已有） |

---

**文档完**
