# mlink

多对多本地设备连接层，transport 可插拔。

## What

mlink 在设备之间建立可靠的多对多连接，不依赖 WiFi / 局域网 / 公网。它是一层哑管道 — 像 TCP 一样只负责把字节可靠送达，不做业务判断。

## Why

网络隔离环境下（VLAN 隔离、端口管控、MDM 限制），设备间仍然需要可靠的通信通道。mlink 把"连接"这件事抽象出来，让上层应用不必关心底层走的是 BLE 还是其他链路。

## Features

- 自动发现 — 无需手动配对或输入地址
- 多对多连接 — 一台设备可同时与多台设备通信
- 二进制序列化 + 流式压缩 — msgpack + zstd
- 流式传输 + 断点续传 — 大消息分片，中断后可续
- 断线自动重连 — 链路抖动对上层透明
- Transport 可插拔 — BLE / IPC / 未来更多

## Architecture

```
┌─────────────┐     ┌──────────────────────┐     ┌────────────────┐
│ Application │ ◄─► │ mlink (Protocol+API) │ ◄─► │ Transport      │
│             │     │                      │     │ BLE / IPC /... │
└─────────────┘     └──────────────────────┘     └────────────────┘
```

## Tech Stack

- Rust
- btleplug (BLE)
- tokio (async runtime)
- msgpack (序列化)
- zstd (流式压缩)

## Status

Early stage.

## License

MIT
