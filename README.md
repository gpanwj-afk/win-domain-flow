# win-domain-flow 0.4

Windows 本机应用与域名流量监控工具。通过 Npcap 捕获 HTTPS 流量，识别 TLS ClientHello 中的域名，并将流量按“应用 → 域名 → 日期”持久化到 SQLite。

## 主要能力

- 中文原生桌面界面，无需日常输入命令
- 明亮模式与深色模式一键切换，默认使用高对比度明亮模式
- 主题、网卡、数据库路径、统计周期和刷新设置均会记忆
- 按应用查看流量，再下钻到该应用访问的域名
- v0.4 新增上行、下行、TLS/TCP、QUIC/UDP 字节与数据包细分
- 今日、本月、近 7 天、近 30 天、全部历史等统计周期
- 默认显示“本月累计”
- 关闭窗口或重启电脑后继续保留并累加数据
- 自动识别常用物理网卡
- 安全停止抓包，退出前写入最后一批数据
- 保留原有命令行工具，兼容自动化脚本

## v0.4 可见的流量颗粒度

新版 GUI 可以逐层查看：

```text
应用
└─ 域名
   ├─ 上行字节与数据包
   ├─ 下行字节与数据包
   ├─ TLS over TCP 字节与数据包
   └─ QUIC over UDP 字节与数据包
```

例如，可以判断某个浏览器主要向哪个域名传输了多少数据、流量以下载还是上传为主，以及主要走 TLS/TCP 还是 QUIC/UDP。

### 为什么不能直接显示“传输了什么内容”？

HTTPS 正文经过 TLS 加密。仅靠被动抓包无法可靠看到：

- HTTP URL 路径与查询参数
- 请求或响应正文
- 聊天消息与账号信息
- 上传或下载文件的实际内容
- Content-Type、文件名或业务字段

本项目不会安装中间人证书、注入应用进程或绕过 TLS，因此不会把加密内容伪装成可见数据。需要查看自己的 HTTP 请求正文时，应使用应用自身的开发者工具、服务端日志或明确配置的调试代理，并遵守相关授权与隐私要求。

## 数据会不会在关闭后清空？

不会。流量持续写入 SQLite，不依赖内存中的界面状态。

默认数据库位置：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

例如：

```text
C:\Users\你的用户名\AppData\Local\win-domain-flow\domainflow.db
```

重新打开程序时会自动加载同一数据库，因此“本月累计”会从月初延续到当前时间。只有主动删除数据库文件或在界面中改用其他数据库，累计数据才会发生变化。

### 旧版数据迁移

如果程序启动目录中已经存在旧版 `domainflow.db`，首次升级会继续使用该数据库，不会抛弃旧记录。升级前的数据没有应用字段，会显示在：

```text
历史数据（升级前未记录应用）
```

v0.3 及更早版本没有记录上行、下行与协议细分，因此旧记录仍保留总流量和数据包，但细分区域会标注“旧版记录无方向与协议细分”。v0.4 新采集的数据会同时写入总量表和细分表。

## 安装要求

- Windows 10/11 x64
- 官方 Npcap，建议安装时启用 `WinPcap API-compatible Mode`
- 构建源码时需要 Rust 1.88.0 MSVC
- 构建源码时需要 Visual Studio Build Tools 2022 C++ 工作负载
- 构建源码时需要 Npcap SDK 1.16

不要使用 Win10Pcap 替代 Npcap。

## 构建

在管理员 PowerShell 中：

```powershell
$env:LIB="C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE="C:\Npcap-SDK\Include;$env:INCLUDE"

cargo build --release --locked
```

生成：

```text
target\release\win-domain-flow-gui.exe
target\release\win-domain-flow.exe
```

## 使用中文 GUI

以管理员身份运行：

```powershell
.\target\release\win-domain-flow-gui.exe
```

操作顺序：

1. 选择当前联网的物理网卡。
2. 点击“开始记录流量”。
3. 正常使用浏览器、微信、腾讯会议等应用。
4. 在左侧“应用流量排行”选择应用。
5. 在右侧查看该应用访问的域名、上下行与协议细分。
6. 点击窗口右上角可切换明亮模式或深色模式。
7. 结束时点击“停止并安全保存”。

界面会记住：

- 上次选择的网卡
- 数据库路径
- 统计周期
- 显示条数
- 自动刷新设置
- 明亮或深色主题

设置文件位于：

```text
%LOCALAPPDATA%\win-domain-flow\settings.conf
```

## 应用归因原理与边界

Windows 版本通过系统 TCP/UDP 连接表读取 PID，再读取进程可执行文件名。归因属于尽力而为，以下情况可能显示“未知应用”：

- 连接极短，在系统连接表刷新前已经关闭
- 权限不足，无法读取部分系统服务或其他用户进程
- 多个 UDP 进程使用相同本地端口，无法唯一判断
- 抓包从连接中途开始，进程连接状态已经变化
- VPN、代理或安全软件代替原应用建立外部连接

“应用归因率”与“域名识别率”是两项独立指标：

- 应用归因率：多少字节成功关联到 Windows 进程
- 域名识别率：多少字节成功从 TLS ClientHello 识别域名

## 域名识别边界

- 支持明文 TLS ClientHello 中的 SNI
- ECH 加密的 ClientHello 无法读取域名
- UDP/443 和 QUIC 当前归入“未知域名”，但仍记录为 QUIC/UDP 流量
- 漏抓握手、乱序或缺失 TCP 分段时可能显示“未知域名”
- 已建立连接在启动抓包后可能不会再次发送 ClientHello

流量字节数使用 pcap 报告的线上长度，包含链路层、IP 和传输层头部；重传包会计入流量，因为它们确实占用了网络带宽。

## 命令行工具

### 枚举网卡

```powershell
.\target\release\win-domain-flow.exe devices
```

### 抓包

```powershell
.\target\release\win-domain-flow.exe capture `
  --interface "\Device\NPF_{...}" `
  --db "$env:LOCALAPPDATA\win-domain-flow\domainflow.db"
```

### 查询域名排行

```powershell
.\target\release\win-domain-flow.exe top `
  --db "$env:LOCALAPPDATA\win-domain-flow\domainflow.db" `
  --days 30 `
  --limit 50
```

命令行 `top` 继续读取兼容的域名汇总表。应用下钻与方向细分目前以 GUI 为主。

## 质量门禁

```powershell
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

GitHub Actions 同时验证：

- Linux 完整测试、Clippy 和 Release 构建
- Windows MSVC 编译
- Npcap SDK 链接
- Windows 进程归因代码
- CLI 与 GUI 两个可执行文件
- 扁平 Windows x64 成品包

## 隐私

所有数据保存在本机 SQLite 文件中。工具不上传抓包内容、域名列表或应用信息，也不解密 HTTPS 正文。

## 许可证

MIT。
