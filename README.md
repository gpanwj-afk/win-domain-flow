# win-domain-flow 0.5

Windows 本机应用、域名与浏览器活动诊断工具。

基础模式通过 Npcap 捕获 HTTPS 流量，将数据按“应用 → 域名 → 日期”持久化到 SQLite；需要追查某个浏览器域名究竟在做什么时，可以临时启用随程序打包的 Edge/Chrome 扩展，查看请求 URL、实际编码传输字节、资源类型、MIME、来源页面，以及真实下载文件记录。

## v0.5 主要变化

- GUI 卡片根据窗口宽度自动调整列数与宽度
- 小窗口中应用区、域名区自动改为上下排列
- 左侧控制栏可以拖动调整宽度
- 主内容区域支持整体滚动
- 保留明亮模式、深色模式及主题记忆
- 主界面回归“应用与域名总量”，不再用上行/下行、TCP/UDP 标签冒充业务用途
- 新增可选“浏览器活动诊断”区域
- 新增随包提供的 Edge/Chrome 扩展，默认关闭
- 浏览器请求按实际编码传输字节排序，而不是只看响应头声明大小
- 浏览器下载记录可显示文件名、保存路径、最终 URL、MIME、总大小与完成状态

## 两种使用模式

### 1. 基础监控

不安装浏览器扩展也可以使用：

```text
应用
└─ 域名
   ├─ 总流量
   └─ 数据包
```

适合持续观察哪个应用、哪个域名占用流量。

### 2. 浏览器活动诊断

当某个浏览器域名流量异常时，临时启用扩展，可以进一步查看：

```text
目标域名
├─ 实际传输字节最大的 URL
├─ 页面文档、音视频、Fetch、XHR、脚本、图片、字体等资源类型
├─ MIME、HTTP 状态码、HTTP/2 或 HTTP/3 等协议
├─ 是否来自浏览器缓存
├─ 请求的来源页面或发起脚本
└─ 浏览器下载文件名、保存路径、最终 URL 与文件大小
```

例如，筛选 `tlabel.tencent.com` 后，可以判断数百 MiB 流量主要来自：

- 某个视频或音频分片 URL
- 大体积 Fetch/XHR 接口响应
- 模型、压缩包或二进制资源
- 图片、字体、脚本等静态资源
- 浏览器下载管理器中的实际文件下载

## 浏览器扩展安装

1. 以管理员身份启动 `win-domain-flow-gui.exe`。
2. 在 GUI 的“浏览器活动诊断”区域点击“打开扩展安装目录”。
3. Edge 打开 `edge://extensions`，Chrome 打开 `chrome://extensions`。
4. 开启“开发人员模式”。
5. 点击“加载解压缩的扩展”。
6. 选择程序目录中的 `browser-extension` 文件夹。
7. 固定扩展图标。
8. 点击扩展图标，打开“启用深度诊断”。
9. 重新加载需要排查的网页。
10. 在 GUI 中输入目标域名进行筛选。

启用后浏览器会显示“此扩展程序正在调试此浏览器”的标准提示，这是读取 Network 元数据和实际传输字节所需的浏览器安全提示。排查结束后可以在扩展弹窗中关闭深度诊断。

扩展详细说明见 [`browser-extension/README_CN.md`](browser-extension/README_CN.md)。

## 能否看到具体文件或内容？

### 可以精确看到

浏览器下载管理器接管的下载：

- 文件名
- 本地保存路径
- 原始 URL 与最终 URL
- MIME
- 总大小
- 下载状态
- 本地文件是否仍存在

### 可以判断用途但不能读取正文

普通网页资源可以看到 URL、实际编码传输字节、资源类型、MIME、状态码和来源页面，据此判断其更像视频分片、接口响应、脚本、图片或其他资源。

### 不会读取

- HTTPS 请求或响应正文
- 聊天消息
- 文件实际内容
- 表单、账号、Cookie 或 Authorization
- 浏览器页面 DOM

扩展没有调用 `Network.getResponseBody`，桌面程序也不会安装中间人证书或注入浏览器进程。

## 统计口径

基础 Npcap 流量使用抓包报告的线上长度，包含链路层、IP、传输层头部和重传。

浏览器诊断中的“实际传输”是浏览器调试协议报告的响应编码数据长度，用于比较哪个 URL 真正消耗了数据。它通常不包含所有链路层和协议头部，也不等同于 Npcap 总流量，所以两者不要求完全相等。

当浏览器没有提供实际传输字节时，界面会回退到响应头声明大小，并明确标注“响应声明”。

## 数据持久化

关闭 GUI 或重启电脑不会清空数据。默认数据库：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

默认显示“本月累计”。只有主动删除数据库文件或改用其他数据库，累计数据才会变化。

浏览器请求与下载记录也写入同一个 SQLite 文件。

## 隐私与安全

- 所有数据保存在本机
- 扩展只连接 `127.0.0.1:38765`
- 扩展没有 `<all_urls>` 主机权限
- 本地接收器只接受 Chrome/Edge 扩展来源的跨域请求
- 深度诊断默认关闭
- 不上传域名、URL、文件名或应用信息

完整 URL 的查询参数可能包含敏感标识符。虽然数据不会离开本机，仍应像保护浏览器历史记录一样保护 `domainflow.db`，不要随意分享数据库文件。

## 安装要求

- Windows 10/11 x64
- 官方 Npcap，建议安装时启用 `WinPcap API-compatible Mode`
- 不要使用 Win10Pcap 替代 Npcap
- 源码构建需要 Rust 1.88.0 MSVC
- 源码构建需要 Visual Studio Build Tools 2022 C++ 工作负载
- 源码构建需要 Npcap SDK 1.16

## 构建

管理员 PowerShell：

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

## 基础操作

1. 选择当前联网的物理网卡。
2. 点击“开始记录流量”。
3. 正常使用应用。
4. 在应用排行中选择应用。
5. 查看该应用访问的域名。
6. 需要追查浏览器网页用途时，再启用浏览器扩展。
7. 结束抓包时点击“停止并安全保存”。

## 命令行工具

```powershell
.\win-domain-flow.exe devices

.\win-domain-flow.exe capture `
  --interface "\Device\NPF_{...}" `
  --db "$env:LOCALAPPDATA\win-domain-flow\domainflow.db"

.\win-domain-flow.exe top `
  --db "$env:LOCALAPPDATA\win-domain-flow\domainflow.db" `
  --days 30 `
  --limit 50
```

## 已知边界

- 浏览器活动诊断只覆盖安装了扩展的 Edge/Chrome 标签页
- 微信、桌面客户端等非浏览器应用仍只能看到应用与域名总量
- DevTools 与扩展不能同时独占调试同一个标签页
- 浏览器缓存、Service Worker 和预取可能让部分请求的网络字节为零
- ECH、QUIC、漏抓 TLS 握手等情况仍可能导致基础监控显示未知域名
- 扩展无法说明加密正文的语义，只能用 URL、类型、MIME 和大小判断用途

## 质量门禁

```powershell
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

GitHub Actions 同时验证 Linux、Windows MSVC、Npcap SDK、浏览器扩展语法、安全权限和 Windows 成品包。

## 许可证

MIT。
