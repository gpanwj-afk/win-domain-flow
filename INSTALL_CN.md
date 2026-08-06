# 安装与部署指南（中文版）

本文档详细记录了 win-domain-flow 在 Windows 上的安装过程、遇到的问题及解决方案。

## 目录

- [环境要求](#环境要求)
- [安装步骤](#安装步骤)
- [常见问题](#常见问题)
- [验收测试记录](#验收测试记录)
- [故障排除](#故障排除)

---

## 环境要求

| 组件 | 版本要求 | 说明 |
|------|----------|------|
| Windows | 10/11 x64 | 必须 64 位系统 |
| Rust | 1.88.0 MSVC | 使用 `rustup` 安装 |
| Npcap | 1.80+ | 必须安装 WinPcap 兼容模式 |
| Npcap SDK | 最新版 | 编译时需要 |
| VS Build Tools | 2022 | C++ 工作负载 |

---

## 安装步骤

### 1. 安装 Rust 工具链

```powershell
# 安装 rustup（如果尚未安装）
Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile "$env:TEMP\rustup-init.exe"
& "$env:TEMP\rustup-init.exe"

# 安装 1.88.0 MSVC 工具链
rustup toolchain install 1.88.0-x86_64-pc-windows-msvc --profile minimal --component rustfmt clippy
rustup override set 1.88.0-x86_64-pc-windows-msvc

# 验证
rustc --version  # 应显示 1.88.0
cargo --version
```

### 2. 安装 Npcap（关键步骤）

**⚠️ 重要：不要使用 Win10Pcap 作为替代品！**

#### 2.1 下载 Npcap

访问 https://npcap.com/#download 下载最新版安装程序。

#### 2.2 运行安装程序

双击下载的 `npcap-x.xx.exe`，在安装选项页面**必须勾选**：

```
☑ Install Npcap in WinPcap API-compatible Mode
  （WinPcap 兼容模式 - 必须勾选！）

☐ Restrict Npcap driver's access to Administrators only
  （可选：限制驱动访问权限为管理员）

☐ Support raw 802.11 traffic (and monitor mode) for wireless adapters
  （可选：802.11 无线监听模式 - 一般不需要）
```

**⚠️ 警告：** 如果看到 "Install Npcap in WinPcap API-compatible Mode" 选项被禁用，说明系统中已安装 Win10Pcap。必须先卸载 Win10Pcap 再安装 Npcap。

#### 2.3 卸载 Win10Pcap（如果存在）

```powershell
# 方法 1：通过 winget 卸载
winget uninstall DaiyuuNobori.Win10Pcap

# 方法 2：通过设置卸载
# 设置 → 应用 → 搜索 "Win10Pcap" → 卸载

# 卸载后必须重启电脑！
Restart-Computer
```

#### 2.4 验证安装

```powershell
# 检查 Npcap 服务
Get-Service npcap
# 应显示 Status: Running

# 检查 DLL 文件
Get-ChildItem "C:\Windows\System32\Npcap" | Select-Object Name, Length
# 应包含 wpcap.dll, Packet.dll 等
```

### 3. 安装 Npcap SDK

```powershell
# 下载 Npcap SDK
Invoke-WebRequest -Uri "https://npcap.com/dist/npcap-sdk.zip" -OutFile "$env:TEMP\npcap-sdk.zip"

# 解压到 C:\Npcap-SDK
Expand-Archive -Path "$env:TEMP\npcap-sdk.zip" -DestinationPath "C:\" -Force

# 验证
Test-Path "C:\Npcap-SDK\Lib\x64\wpcap.lib"  # 应返回 True
Test-Path "C:\Npcap-SDK\Include\pcap.h"      # 应返回 True
```

### 4. 编译项目

```powershell
cd win-domain-flow

# 设置 Npcap SDK 路径
$env:LIB = "C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE = "C:\Npcap-SDK\Include;$env:INCLUDE"

# 编译 Release 版本
cargo +1.88.0 build --release

# 验证
Get-Item .\target\release\win-domain-flow.exe
```

### 5. 验证安装

```powershell
# 列出网卡
.\target\release\win-domain-flow.exe devices

# 查看帮助
.\target\release\win-domain-flow.exe --help
.\target\release\win-domain-flow.exe capture --help
.\target\release\win-domain-flow.exe top --help
```

---

## 常见问题

### 问题 1：Win10Pcap 驱动加载失败

**现象：**
```
程序兼容性助手
无法在此设备上加载驱动程序
驱动程序: Win10Pcap.sys
```

**原因：** Win10Pcap 驱动签名与当前 Windows 版本不兼容。

**解决方案：**
1. 卸载 Win10Pcap
2. 重启电脑
3. 安装官方 Npcap（启用 WinPcap 兼容模式）

---

### 问题 2：设备枚举返回无效 UTF-8

**现象：**
```
Error: pcap error: libpcap returned invalid UTF-8: 
invalid utf-8 sequence of 1 bytes from index 23
```

**原因：** 使用了 Win10Pcap 而非 Npcap。Win10Pcap 在中文 Windows 上返回 GBK 编码的设备名。

**解决方案：** 卸载 Win10Pcap，安装官方 Npcap。

---

### 问题 3：无法安装 Npcap（选项被禁用）

**现象：** "Install Npcap in WinPcap API-compatible Mode" 选项灰色不可选。

**原因：** 系统中已安装 Win10Pcap。

**解决方案：**
1. 先卸载 Win10Pcap
2. 重启电脑
3. 重新运行 Npcap 安装程序

---

### 问题 4：找不到 wpcap.dll

**现象：**
```
error: process didn't exit successfully (exit code: 0xc0000135, STATUS_DLL_NOT_FOUND)
```

**原因：** Npcap 未安装或 Npcap 目录不在系统 PATH 中。

**解决方案：**
1. 安装 Npcap（参考上述步骤）
2. 或将 `C:\Windows\System32\Npcap` 添加到系统 PATH

---

### 问题 5：cargo clippy 报 too_many_arguments

**现象：**
```
error: this function has too many arguments (9/7)
```

**原因：** `run_capture_loop` 函数参数较多（这是 spec 固定签名）。

**解决方案：** 在函数前添加 `#[allow(clippy::too_many_arguments)]`（已处理）。

---

### 问题 6：cargo clippy 报 collapsible_str_replace

**现象：**
```
error: used consecutive `str::replace` call
```

**解决方案：** 使用数组语法替换：
```rust
// 错误
s.replace('\t', " ").replace('\r', " ").replace('\n', " ")

// 正确
s.replace(['\t', '\r', '\n'], " ")
```

---

## 验收测试记录

### 测试环境

| 项目 | 值 |
|------|-----|
| OS | Windows 10 Pro 2009 (Build 26200) |
| Rust | 1.88.0 |
| Npcap | 1.88 |
| 测试网卡 | Intel Wi-Fi 6 AX201 160MHz |

### 自动化门禁结果

| 检查 | 结果 |
|------|------|
| cargo fmt --check | PASS |
| cargo check --all-targets --locked | PASS |
| cargo test --lib --locked | PASS (60/60) |
| cargo test --test offline_pipeline | PASS (3/3) |
| cargo clippy --all-targets --locked -D warnings | PASS |
| cargo build --release --locked | PASS |

### 实时抓包结果

```
domain                      bytes     packets
sub.callai.one              829878    506
copilot.tencent.com         159710    194
(unknown)                   15140     163
mobile.events.data...       11949     19
api.skillhub.cn             10387     22
example.com                 8303      25
```

**结论：** SNI 提取功能正常，example.com 成功识别。

---

## 故障排除

### 快速诊断命令

```powershell
# 1. 检查 Npcap 服务
Get-Service npcap

# 2. 检查 Npcap DLL
Get-ChildItem "C:\Windows\System32\Npcap" | Select-Object Name, Length

# 3. 检查 Npcap SDK
Test-Path "C:\Npcap-SDK\Lib\x64\wpcap.lib"
Test-Path "C:\Npcap-SDK\Include\pcap.h"

# 4. 测试设备枚举
.\target\release\win-domain-flow.exe devices

# 5. 检查 Rust 工具链
rustc --version
cargo --version

# 6. 检查环境变量
$env:LIB
$env:INCLUDE
```

### 完全重装步骤

如果遇到无法解决的问题，按以下步骤完全重装：

```powershell
# 1. 卸载 Win10Pcap（如果存在）
winget uninstall DaiyuuNobori.Win10Pcap

# 2. 重启电脑
Restart-Computer

# 3. 安装 Npcap
# 手动运行安装程序，勾选 WinPcap 兼容模式

# 4. 验证
Get-Service npcap

# 5. 重新编译
cargo +1.88.0 build --release
```

---

## 相关链接

- Npcap 官网：https://npcap.com/
- Npcap 下载：https://npcap.com/#download
- Npcap SDK：https://npcap.com/sdk/
- Rust 安装：https://rustup.rs/
- 项目仓库：https://github.com/gpanwj-afk/win-domain-flow
