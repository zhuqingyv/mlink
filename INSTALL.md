# 安装 mlink

## 前提
- macOS 12+
- Rust toolchain (如未安装: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)

## 安装
```
git clone https://github.com/zhuqingyv/mlink.git
cd mlink
cargo install --path crates/mlink-cli
```

## macOS 蓝牙权限
首次运行 `mlink serve` 时系统会弹出蓝牙权限请求，点击"允许"。
如果错过了，去 系统设置 → 隐私与安全性 → 蓝牙 中手动添加终端应用。

## 快速开始

### 两台 Mac 互联
Mac A:
```
mlink serve
mlink room new          # 输出: 482193
```

Mac B:
```
mlink serve
mlink room join 482193  # 自动发现 Mac A
```

### 发消息
Mac B:
```
mlink send 482193 "hello"
```

Mac A 终端显示:
```
[room=482193] MacBook: hello
```

### 传文件
Mac B:
```
mlink send 482193 --file ./report.pdf
```
