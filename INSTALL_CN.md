# win-domain-flow 0.6 Windows 安装与验证指南

## 一、运行环境

- Windows 10/11 x64
- 官方 Npcap
- 建议 Npcap 启用 `WinPcap API-compatible Mode`
- 不要使用 Win10Pcap

直接使用 Windows 成品包不需要安装 Rust、Visual Studio 或 Npcap SDK。

## 二、升级前保护数据

正常点击旧版 GUI 的“停止并安全保存”，等待状态变为已停止。

默认数据目录：

```text
%LOCALAPPDATA%\win-domain-flow\
```

主要文件：

```text
domainflow.db
domainflow.db-wal
domainflow.db-shm
settings.conf
```

不要在旧 GUI 仍写库时删除这些文件。

## 三、0.6 数据库路径变化

0.6 默认数据库固定为：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

GUI 会直接显示当前实际数据库的绝对路径，不再在工作目录数据库和 LOCALAPPDATA 数据库之间静默选择。

如果此前没有保存过明确数据库路径，而程序工作目录存在旧 `domainflow.db`，0.6 会通过 SQLite online backup API 创建一致性快照到固定目录：

- 包含已提交但仍在 WAL 中的数据
- 不执行 `wal_checkpoint(TRUNCATE)`
- 不修改或删除原数据库
- 迁移失败会明确显示继续使用的旧库绝对路径

## 四、安装桌面程序

1. 解压成品包，例如：

```text
C:\Tools\win-domain-flow
```

2. 双击：

```text
Start-GUI-As-Administrator.cmd
```

3. 接受管理员权限提示。
4. 选择当前联网物理网卡。
5. 点击“开始记录流量”。

### 单实例保护

如果已经有域流量管家实例占用 Receiver，第二次启动不会再启动第二套抓包或数据库写入链路，而会显示已有实例的：

- PID
- Receiver 端口
- SQLite 路径

这用于避免“旧 GUI 没停干净，却又启动第二个 GUI”的假验证状态。

## 五、浏览器扩展升级与安装

浏览器活动诊断是可选功能。

0.6 扩展使用固定发行公钥。若从 0.5 或更早版本升级，建议在 Edge/Chrome 扩展管理页重新加载当前发行包中的 `browser-extension` 文件夹一次。

### Edge

1. 打开 `edge://extensions`。
2. 开启开发人员模式。
3. 点击“加载解压缩的扩展”。
4. 选择：

```text
browser-extension
```

5. 固定扩展图标。
6. 点击扩展图标并打开“启用深度诊断”。

Chrome 对应地址：

```text
chrome://extensions
```

## 六、Receiver 安全与状态

默认 Receiver：

```text
127.0.0.1:38765
```

本机只读状态接口：

```text
http://127.0.0.1:38765/health
http://127.0.0.1:38765/status
```

`/status` 可核对 Receiver PID、数据库绝对路径、事件计数和最近错误。

浏览器事件写入 `/events` 时必须来自 0.6 发行扩展的精确 Origin。缺失 Origin、其他扩展 ID 和普通网页来源会被拒绝。

## 七、Receiver 暂时不可用时

扩展不会再立即丢弃事件。

0.6 使用：

- 持久化有界队列
- JSON ACK
- 指数退避
- `chrome.alarms` 自动恢复补发

弹窗会显示：

- 待补发事件数
- 最近 Receiver 错误
- 标签页附加失败数
- 队列是否发生过溢出丢弃

Receiver 恢复后队列会自行继续发送。

## 八、追查大流量域名

例如：

```text
tlabel.tencent.com
```

1. 保持 GUI 运行。
2. 打开扩展深度诊断。
3. 重新执行产生流量的网页操作。
4. 在 GUI 域名筛选中输入目标域名。
5. 查看按实际传输字节排序的请求。
6. 观察 URL、资源类型、MIME、状态码、协议、缓存状态和来源页面。
7. 查看“真实下载文件”确认是否为浏览器下载管理器接管的文件。

## 九、停止程序

结束抓包时点击：

```text
停止并安全保存
```

等待状态变为：

```text
已停止并保存
```

再关闭窗口。

## 十、源码构建

需要：

- Rust 1.88.0 MSVC
- Visual Studio Build Tools C++ 工作负载
- Npcap SDK 1.16

管理员 PowerShell：

```powershell
$env:LIB="C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE="C:\Npcap-SDK\Include;$env:INCLUDE"

cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

## 十一、隔离式 Windows 验证

0.6 成品包附带验证工具：

```text
tools\validate-windows.ps1
tools\browser_fixture.py
tools\query_e2e_db.py
```

运行：

```powershell
.\tools\validate-windows.ps1 `
  -BinaryRoot . `
  -OutputDirectory .\validation-report
```

若从源码目录运行：

```powershell
.\tools\validate-windows.ps1 `
  -BinaryRoot .\target\release `
  -OutputDirectory .\validation-report
```

### 验证器不会做的事

- 不使用用户默认 Edge/Chrome Profile
- 不执行 `Stop-Process -Name msedge`
- 不写真实 `domainflow.db`
- 不依赖公网测试站点
- 不硬编码某次解压扩展的 ID
- 不用 SQLite 文件修改时间判断 WAL 写入
- 不执行数据库 checkpoint

### 验证器会做的事

- 新建临时浏览器 Profile
- 使用动态 CDP 端口
- 使用动态 Receiver 端口
- 使用独立临时 SQLite
- 启动仅监听 `127.0.0.1` 的 fixture
- 动态发现扩展 Service Worker 与实际 ID
- 用 `/status` 核对 Receiver PID 与数据库路径
- 验证 request、实际传输字节和 download 记录
- 故意停止 Receiver，在停机期间产生真实浏览器事件
- 确认队列积压、错误可见、Receiver 恢复后自动补发
- 使用 SQLite `mode=ro` 读取 WAL 中已提交数据
- 最后只关闭本次临时 Profile 对应的浏览器 PID
- 恢复本次临时 Profile 的诊断开关

输出：

```text
validation-report.json
validation-report.xml
validation-manifest.json
```

manifest 记录 commit、二进制 SHA-256、Receiver PID/端口、浏览器 PID、Profile、扩展 ID 和测试数据库路径。

## 十二、常见问题

### 扩展显示 Receiver 未连接

打开：

```text
http://127.0.0.1:38765/status
```

确认 PID 和数据库路径是否对应当前 GUI。若存在第二个旧实例，先回到旧窗口执行正常停止，而不是强杀全部进程。

### 弹窗显示待补发事件

说明 Receiver 曾暂时不可达。保持 GUI 正常运行，队列会自动重试。若持续不清零，查看弹窗中的 Receiver 错误及 GUI 的实际数据库路径。

### 没有真实下载文件

网页视频、接口响应、缓存资源不一定进入浏览器下载管理器。此时查看“最近网页请求”和“网页资源用途”。

### Npcap 总流量与浏览器实际传输不一致

正常。Npcap 包含网络头部与重传；浏览器诊断统计浏览器报告的编码传输长度，两者用途不同。
