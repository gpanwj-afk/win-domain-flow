# 域流量管家 · 浏览器活动诊断扩展 0.6

## 用途

这个可选扩展用于回答：

> 某个浏览器域名为什么产生了大量流量？

它可以记录：

- 每个请求的 URL
- 浏览器报告的实际编码传输字节
- 资源类型
- MIME
- HTTP 状态码
- HTTP/2、HTTP/3 等协议
- 是否来自缓存
- 来源页面或发起脚本
- 浏览器下载文件名、保存路径、最终 URL、总大小与状态

## 不会记录

- HTTPS 请求或响应正文
- Cookie
- Authorization
- 表单内容
- 聊天消息
- 文件实际内容
- 页面 DOM

扩展代码没有调用 `Network.getResponseBody`。

## v0.6 可靠性变化

- 扩展清单内包含固定公钥，解压加载后的扩展 ID 可稳定复现。
- 验证工具仍会从 Service Worker URL **动态发现实际扩展 ID**，不会写死某次安装产生的 ID。
- Receiver 只接受当前发行扩展的 `chrome-extension://` / `edge-extension://` Origin；缺失 Origin 或其他扩展 ID 的 `/events` 写入会被拒绝。
- `transferredBytes = null` 表示浏览器尚未测得实际网络字节；真实的 0 与“未知”不再混淆。
- 部分上报和最终上报按单个请求串行进入统一 FIFO 发送队列。
- 发送失败不会直接丢事件：队列持久化在 `chrome.storage.local`，最多保留 1000 条，使用指数退避和 `chrome.alarms` 自动补发。
- Receiver 必须返回合法 JSON，并明确返回 `ok=true, accepted=true` 才算事件送达。
- CDP 重定向复用 `requestId` 时，会先保存 30x 前一跳，再开始记录新 URL，不再互相覆盖。
- 弹窗会显示 Receiver 错误、待补发队列、丢弃计数以及标签页附加失败状态。

## 安装

1. 启动域流量管家 GUI。
2. Edge 打开 `edge://extensions`，Chrome 打开 `chrome://extensions`。
3. 开启开发人员模式。
4. 点击“加载解压缩的扩展”。
5. 选择当前 `browser-extension` 文件夹。
6. 固定扩展图标。

从旧版升级到 0.6 后，如果浏览器仍加载的是旧扩展目录，请在扩展管理页重新加载当前发行包中的 `browser-extension`。固定公钥确保今后的 0.6+ 包保持稳定身份。

## 启用

1. 点击扩展图标。
2. 打开“启用深度诊断”。
3. 确认弹窗显示本机接收器已连接，且没有 Receiver 错误或待补发异常。
4. 重新加载目标网页。
5. 回到 GUI 输入需要筛选的域名。

启用后浏览器会显示扩展正在调试标签页的标准提示。排查结束后关闭开关即可。

## 数据流向与鉴权

扩展只向本机回环地址发送 JSON 元数据：

```text
http://127.0.0.1:38765/events
```

扩展没有 `<all_urls>` 主机权限。Receiver 不监听局域网或公网地址。

`/health` 和 `/status` 仅用于本机状态观测；写入 `/events` 必须来自发行扩展的精确 Origin。

`/status` 可用于确认：

- Receiver PID
- 实际监听端口
- 当前 SQLite 绝对路径
- 已接受事件数量
- 最近事件时间
- 最近错误
- Receiver 期望的扩展 ID

数据保存到 GUI 当前明确显示的 SQLite 数据库。默认位置：

```text
%LOCALAPPDATA%\win-domain-flow\domainflow.db
```

## 使用示例

筛选：

```text
tlabel.tencent.com
```

然后按实际传输字节查看最大的 URL：

- `media` 常见于音视频资源
- `fetch`、`xhr` 常见于接口或大块数据
- `script` 常见于 JavaScript
- `image` 常见于图片
- `font` 常见于字体
- `document` 常见于页面文档

若浏览器下载管理器接管了下载，会在“真实下载文件”中直接显示文件名和保存位置。

## 隔离验证

发行包包含：

```text
tools\validate-windows.ps1
tools\browser_fixture.py
tools\query_e2e_db.py
```

验证器使用：

- 独立临时 Edge/Chrome Profile
- OS 分配的动态浏览器调试端口
- OS 分配的动态 Receiver 端口
- 独立临时 SQLite 数据库
- 仅监听 127.0.0.1 的本地 fixture 页面
- CDP `Target.createTarget` 创建测试页
- 动态发现扩展 ID
- SQLite `mode=ro` 只读查询 WAL 数据

它不会关闭用户默认浏览器，不会写入真实 `domainflow.db`，也不会通过数据库文件修改时间判断是否写入。

测试结束时只停止命令行中带本次临时 Profile 的浏览器进程，并恢复该临时 Profile 的诊断开关。

## 已知边界

- 只覆盖安装扩展的 Edge/Chrome
- 只跟踪 `http://` 和 `https://` 标签页
- DevTools 可能与扩展争用同一标签页的调试连接
- 缓存资源的实际网络字节可能为 0
- 流式资源会周期性更新同一请求记录
- URL 可以揭示本地浏览历史，数据库文件应妥善保护
