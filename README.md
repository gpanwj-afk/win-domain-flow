# win-domain-flow 0.6

Windows 本机应用、域名与浏览器活动诊断工具。

基础模式通过 Npcap 捕获 HTTPS 流量，将数据按“应用 → 域名 → 日期”持久化到 SQLite；需要追查某个浏览器域名究竟在做什么时，可以临时启用随程序打包的 Edge/Chrome 扩展，查看请求 URL、实际编码传输字节、资源类型、MIME、来源页面，以及真实下载文件记录。

## v0.6 重点变化

### 浏览器事件不再“一次失败就丢”

扩展现在使用持久化有界发送队列：

- 最多保存 1000 条待发送事件，同时将序列化队列控制在约 4 MiB 以内
- 队列保存在 `chrome.storage.local`
- Receiver 不可用时指数退避重试
- 使用 `chrome.alarms` 支持 Manifest V3 Service Worker 被挂起后的恢复补发
- Receiver 必须返回合法 JSON 且明确 `ok=true, accepted=true` 才视为送达
- Popup 可看到待补发数量、最近 Receiver 错误和丢弃计数

### 实际传输字节采用单调更新

同一个请求多次上报时，SQLite 中 `transferred_bytes` 只会保留更大的已知值。例如先记录 `8192`，之后收到延迟的 `0`，最终仍保持 `8192`。

`null` 代表“浏览器尚未测得”，真实的 0 和未知不再混在一起。

### 请求上报顺序与重定向

- 同一 request 的部分上报、最终上报先进入串行 report chain，再进入统一 FIFO 队列。
- CDP 30x 重定向复用 `requestId` 时，会先保存前一跳，再记录新 URL，避免覆盖。

### Receiver 鉴权与状态接口

扩展使用固定发行公钥，身份在重新加载后保持稳定。Receiver 只接受该发行扩展的精确 `chrome-extension://` / `edge-extension://` Origin 写入 `/events`。

缺失 Origin、错误扩展 ID 或普通网页来源均会被拒绝。

新增：

```text
GET http://127.0.0.1:38765/health
GET http://127.0.0.1:38765/status
```

`/status` 返回：

- 产品标识
- Receiver PID
- 实际端口
- 当前 SQLite 绝对路径
- 接收事件计数
- 最近事件时间
- 最近错误
- 期望的扩展 ID

### 单实例与端口冲突

GUI 启动前会探测现有 `win-domain-flow` Receiver。若已有实例运行，第二个窗口只显示已有实例的 PID、端口和数据库路径，不再启动第二套抓包或数据库写入链路。

浏览器诊断区遇到端口冲突时也会尽量显示已有产品实例信息，而不是表现成“正常启动”。

### 数据库路径不再静默分叉

默认数据库固定为：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

GUI 会明确显示实际使用的绝对路径。

若旧版使用的是程序自动选择的工作目录 `domainflow.db`，0.6 会识别这一精确旧默认路径，即使旧版已经把它保存进 `settings.conf`，仍会使用 SQLite online backup API 将旧库复制到固定数据目录。真正由用户手工指定的自定义数据库路径不会被自动迁移：

- 读取源库时不执行 WAL checkpoint
- 已提交但仍在 WAL 中的数据也会进入快照
- 原数据库文件保留不删除
- 迁移失败时明确提示，并明确继续使用哪个旧库

设置保存失败也会显示到 GUI，不再静默忽略。

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

例如筛选 `tlabel.tencent.com`，可以判断大流量主要来自视频分片、大体积接口、模型/二进制资源、静态资源，还是浏览器下载文件。

## 浏览器扩展安装

1. 以管理员身份启动 `win-domain-flow-gui.exe`。
2. 在 GUI 的“浏览器活动诊断”区域点击“打开扩展安装目录”。
3. Edge 打开 `edge://extensions`，Chrome 打开 `chrome://extensions`。
4. 开启“开发人员模式”。
5. 点击“加载解压缩的扩展”。
6. 选择发行包中的 `browser-extension` 文件夹。
7. 固定扩展图标。
8. 打开“启用深度诊断”。
9. 重新加载需要排查的网页。
10. 在 GUI 输入目标域名。

从旧版升级到 0.6 时建议在扩展管理页重新加载当前发行包中的扩展。0.6 起使用固定公钥，因此后续发行包的扩展身份稳定。

启用后浏览器会显示“此扩展程序正在调试此浏览器”的标准提示，这是浏览器自己的安全提示。

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

## 数据持久化

关闭 GUI 或重启电脑不会清空数据。默认显示“本月累计”。

浏览器请求和下载记录与基础流量记录写入同一个当前明确选择的 SQLite 数据库。

## 隔离验证工具

发行包包含：

```text
tools\validate-windows.ps1
tools\browser_fixture.py
tools\query_e2e_db.py
```

源码构建完成后可运行：

```powershell
.\tools\validate-windows.ps1 `
  -BinaryRoot .\target\release `
  -OutputDirectory .\validation-report
```

验证器的设计原则：

- 独立临时 Edge/Chrome Profile，不操作用户默认 Profile
- 浏览器远程调试端口由 OS 动态分配
- Receiver 测试端口由 OS 动态分配
- 独立临时 SQLite，不写用户真实数据库
- 本机 fixture 只绑定 `127.0.0.1`，不依赖公网
- 使用 CDP `Target.createTarget` 创建测试页，不用 `Start-Process msedge.exe URL`
- 从 Service Worker URL 动态发现实际扩展 ID，并与 Receiver 期望的发行 ID 严格核对
- CDP connect/send/receive 均有硬超时，浏览器异常不会无限挂住验证器
- 只停止命令行包含本次临时 Profile 的浏览器 PID
- Receiver 停止失败时不会启动第二个 Receiver
- 使用 SQLite `mode=ro` 查询，不看 DB 文件时间，不执行 checkpoint
- 会故意在 Receiver 停机期间产生浏览器事件，并验证队列在恢复后自动补发
- 测试结束恢复扩展诊断开关
- 输出 JSON、JUnit XML 和测试 manifest

CI 使用官方 Chrome for Testing 执行自动化扩展 E2E；用户正常使用仍按上文在 Edge/Chrome 扩展管理页手工加载发行包中的扩展。

manifest 包含 commit、二进制 SHA-256、Receiver PID/端口、浏览器 PID、临时 Profile、动态扩展 ID 和临时数据库路径。

## CLI 的隔离 Receiver 模式

用于 CI / 自动化测试，不启动 Npcap：

```powershell
.\win-domain-flow.exe browser-receiver `
  --db C:\Temp\domainflow-e2e.db `
  --port 0
```

`--port 0` 表示让 Windows 自动分配可用端口。启动后 CLI 会输出实际 PID、端口和数据库路径。

## 隐私与安全

- 所有数据保存在本机
- 扩展仅有 `http://127.0.0.1/*` 回环主机权限
- 没有 `<all_urls>`
- `/events` 只接受发行扩展的精确 Origin
- 深度诊断默认关闭
- 不上传域名、URL、文件名或应用信息

完整 URL 的查询参数可能包含敏感标识符，应像保护浏览器历史记录一样保护 `domainflow.db`。

## 安装要求

- Windows 10/11 x64
- 官方 Npcap，建议安装时启用 `WinPcap API-compatible Mode`
- 不要使用 Win10Pcap 替代 Npcap
- 源码构建需要 Rust 1.88.0 MSVC
- 源码构建需要 Visual Studio Build Tools C++ 工作负载
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
- DevTools 与扩展可能争用同一个标签页的调试连接
- 浏览器缓存、Service Worker 和预取可能让实际网络字节为 0
- ECH、QUIC、漏抓 TLS 握手等情况仍可能让基础监控显示未知域名
- 扩展无法说明加密正文的语义，只能结合 URL、类型、MIME 和大小判断用途

## 质量门禁

```powershell
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

GitHub Actions 额外执行：

- 扩展公钥动态推导 ID 与 Rust Receiver ID 一致性检查
- 浏览器扩展权限/隐私静态门禁
- Receiver 真 TCP 集成测试
- SQLite WAL online-backup 回归测试
- Windows MSVC / Npcap SDK 构建
- Windows Chrome for Testing dedicated-profile 浏览器端到端测试
- Receiver 停机队列自动补发测试
- Windows 成品包验收与敏感/临时文件扫描

## 许可证

MIT。
