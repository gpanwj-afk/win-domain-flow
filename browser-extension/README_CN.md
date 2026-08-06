# 域流量管家 · 浏览器活动诊断扩展

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

## 安装

1. 启动域流量管家 GUI。
2. Edge 打开 `edge://extensions`，Chrome 打开 `chrome://extensions`。
3. 开启开发人员模式。
4. 点击“加载解压缩的扩展”。
5. 选择当前 `browser-extension` 文件夹。
6. 固定扩展图标。

## 启用

1. 点击扩展图标。
2. 打开“启用深度诊断”。
3. 确认弹窗显示本机接收器已连接。
4. 重新加载目标网页。
5. 回到 GUI 输入需要筛选的域名。

启用后浏览器会显示扩展正在调试标签页的标准提示。排查结束后关闭开关即可。

## 数据流向

扩展只向以下本机地址发送 JSON 元数据：

```text
http://127.0.0.1:38765/events
```

扩展没有 `<all_urls>` 主机权限。本地接收器不会监听局域网或公网地址。

数据保存到 GUI 当前选择的 SQLite 数据库，默认是：

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

## 已知边界

- 只覆盖安装扩展的 Edge/Chrome
- 只跟踪 `http://` 和 `https://` 标签页
- DevTools 可能与扩展争用同一标签页的调试连接
- 缓存资源的网络字节可能为零
- 流式资源会周期性更新同一请求记录
- URL 可以揭示本地浏览历史，数据库文件应妥善保护
