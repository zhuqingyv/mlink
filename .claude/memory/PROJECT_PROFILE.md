# mlink — 项目档案

- **项目名**: mlink
- **路径**: /Users/zhuqingyu/project/mlink
- **Git Remote**: git@github.com:zhuqingyv/mlink.git
- **定位**: 支持多对多的本地设备连接层
- **技术栈**: Rust + tokio + btleplug
- **目标平台**: macOS / Windows / Linux
- **核心能力**: 自动发现、多对多连接、可靠传输（分片/重组/断点续传/重连）、transport 可插拔（BLE / IPC / ...）
- **设计原则**: 哑管道，不做业务判断，像 TCP 一样只负责把字节可靠送达
