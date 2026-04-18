# mlink

BLE-based local agent communication core.

## What

mlink 让不同设备上的 agent 通过蓝牙 BLE 直接通信，不依赖 WiFi、局域网或公网。适用于网络隔离环境下的多设备 agent 协作。

## Why

- 公司 WiFi 做了 VLAN 隔离，设备之间无法通过网络通信
- USB / Thunderbolt 物理端口被管控
- AirDrop / WiFi Direct 被 MDM 禁用
- 蓝牙 BLE 是唯一不受企业网络策略限制的本地无线通道

## Use Cases

- **Agent 间通信** — 多台设备上的 AI agent 互相发送指令、同步状态
- **知识库服务** — 一台设备部署知识库，通过 BLE 对其他设备提供查询服务
- **CLI / MCP 集成** — 作为 transport layer 集成到 CLI 工具和 MCP server

## Tech Stack

- BLE 5.0+ (2M PHY, ~1.2 Mbps)
- 跨平台: macOS / Windows / Linux
- 自动发现、自动连接、无需手动配对
- 消息分片/重组、心跳重连、压缩

## Architecture

```
Device A                          Device B
┌──────────────┐                 ┌──────────────┐
│  Agent / CLI │                 │  Agent / CLI │
│      │       │                 │      │       │
│  mlink core  │                 │  mlink core  │
│      │       │                 │      │       │
│  BLE Service │ ◄── BLE 5.0 ──►│  BLE Service │
└──────────────┘                 └──────────────┘
```

## Status

Early stage. Under active development.

## License

MIT
