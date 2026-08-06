# win-domain-flow 0.5 Windows 安装指南

## 一、运行环境

- Windows 10/11 x64
- 官方 Npcap
- 建议安装 Npcap 时启用 `WinPcap API-compatible Mode`
- 不要安装或使用 Win10Pcap

直接使用成品包时不需要安装 Rust、Visual Studio 或 Npcap SDK。

## 二、升级前保护数据

正常点击旧版 GUI 中的“停止并安全保存”，等待状态变为已停止。

数据库默认位于：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

建议升级前复制以下文件到备份目录：

```text
domainflow.db
domainflow.db-wal
domainflow.db-shm
settings.conf
```

不要在 GUI 仍然运行时复制或删除数据库。

## 三、安装桌面程序

1. 解压 Windows x64 成品包到固定目录，例如：

```text
C:\Tools\win-domain-flow
```

2. 双击：

```text
Start-GUI-As-Administrator.cmd
```

3. 接受 Windows 管理员权限提示。
4. 在 GUI 中选择当前联网的物理网卡。
5. 点击“开始记录流量”。

GUI 默认使用明亮模式，可以在右上角切换深色模式。

## 四、响应式界面

- 宽窗口：应用排行与域名排行左右显示
- 窄窗口：应用排行与域名排行上下显示
- 顶部指标卡会根据宽度自动显示为 5 列、3 列或 2 列
- 左侧控制栏可以拖动改变宽度
- 主内容可以纵向滚动

## 五、安装 Edge/Chrome 浏览器活动诊断扩展

该扩展是可选功能，默认不启用。只有需要追查某个网页域名为什么产生大量流量时才安装并打开。

### Edge

1. 启动域流量管家 GUI。
2. 在“浏览器活动诊断”区域点击“打开扩展安装目录”。
3. 打开：

```text
edge://extensions
```

4. 开启“开发人员模式”。
5. 点击“加载解压缩的扩展”。
6. 选择成品包中的：

```text
browser-extension
```

7. 固定扩展图标。
8. 点击扩展图标。
9. 打开“启用深度诊断”。
10. 重新加载需要排查的网页。

### Chrome

步骤相同，扩展管理地址为：

```text
chrome://extensions
```

## 六、追查大流量域名

以 `tlabel.tencent.com` 为例：

1. 保持 GUI 运行。
2. 启用浏览器扩展的深度诊断。
3. 重新加载或重新执行会产生流量的网页操作。
4. 在 GUI 的“域名筛选”中输入：

```text
tlabel.tencent.com
```

5. 查看“最近网页请求”。请求默认按实际传输字节从大到小排列。
6. 重点观察：
   - URL 路径
   - 实际传输大小
   - 资源类型
   - MIME
   - HTTP 状态码
   - HTTP/2、HTTP/3 等协议
   - 是否来自缓存
   - 来源页面
7. 查看“网页资源用途”汇总，判断流量主要来自音视频、Fetch/XHR、脚本、图片或其他资源。
8. 查看“真实下载文件”，确认是否存在浏览器下载管理器接管的文件。

## 七、浏览器提示“正在调试此标签页”

这是正常现象。扩展使用浏览器公开的调试协议读取 Network 元数据和实际编码传输字节。

扩展不会读取响应正文，也不会安装 HTTPS 中间人证书。

排查完成后，在扩展弹窗中关闭“启用深度诊断”，提示会随调试会话结束而消失。

## 八、数据保存

桌面流量、浏览器请求元数据和下载记录都保存在：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

关闭 GUI或重启电脑不会清空数据。默认查看“本月累计”。

完整 URL 可能含有敏感查询参数。数据库只保存在本机，但仍不应随意发送给他人。

## 九、停止程序

结束抓包时必须点击：

```text
停止并安全保存
```

等待状态变为：

```text
已停止并保存
```

再关闭窗口。

## 十、源码构建

只有需要从源码构建时才需要：

- Rust 1.88.0 MSVC
- Visual Studio Build Tools 2022 C++ 工作负载
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

## 十一、常见问题

### GUI 无法枚举网卡

确认官方 Npcap 服务正在运行：

```powershell
Get-Service npcap, npf -ErrorAction SilentlyContinue
```

### 扩展显示未连接

确认 GUI 正在运行。本地接收器只监听：

```text
127.0.0.1:38765
```

### 扩展已启用但没有请求

- 重新加载目标网页
- 确认页面是 `http://` 或 `https://`
- 关闭该标签页的 DevTools 后重试
- 查看扩展弹窗是否显示已跟踪标签页

### 没有真实下载文件

网页视频、接口响应、缓存资源并不一定进入浏览器下载管理器。此时应查看“最近网页请求”和“网页资源用途”，而不是“真实下载文件”。

### Npcap 总流量与浏览器实际传输不一致

属于正常现象。Npcap 包含网络头部和重传；浏览器诊断统计响应编码数据长度，两者用途不同。
