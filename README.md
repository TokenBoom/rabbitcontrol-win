# RabbitControl Windows（`TokenBoom/rabbitcontrol-win`）

RabbitControl Windows 客户端与硬件适配仓库，基于 [`TokenBoom/rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core) v0.2.2 构建：原生低延迟硬件后端、XAML Islands 桌面应用、被控端服务与用户会话代理。

## 功能概览

### 主控端（控制远端设备）

- **登录**：服务器探测 + Root CA TOFU 校验 + 设备绑定 + mTLS 设备证书签发 + Token 自动轮换
- **设备管理**：设备列表/在线状态、最近连接/收藏历史、9 位码连接、LAN 直连加速
- **会话**：独立可缩放窗口（全屏/Esc 退出）、D3D11 硬件渲染 letterbox、性能 HUD（时长/传输/帧率/码率/RTT/丢包）、画质工具条（分辨率/帧率/码率/编解码/鼠标模式/多屏切换热切换）、仅观看模式
- **输入**：XAML Island 指针/键盘捕获、Set-1 扫描码 + E0 前缀注入、虚拟屏多屏映射
- **工具通道**：终端（文本视图）、文件传输（上传/下载/取消/进度）、端口映射（添加/暂停/启用/删除/吞吐统计）
- **剪贴板**：CF_UNICODETEXT 双向同步（有界重试、回环抑制）
- **音频**：WASAPI 共享模式播放（AUTOCONVERTPCM 引擎转换）
- **重连**：异常断开 3s×3 自动重连、SessionSwitching 快速接续

### 被控端（允许远端控制本机）

- **信令接入**：UPnP 端口映射、中继/LAN 双路直入、同账号 OwnerTicket 免密自控
- **审批**：结构化审批弹窗（对端地址/分辨率/密码状态），超时自动拒绝
- **采集**：DXGI Desktop Duplication（静态帧零输出、分辨率变化自动重建）
- **编码**：MF H.264 硬件编码（零拷贝 NV12 纹理入、CBR + 低延迟 + 按需 IDR）
- **注入**：SendInput（虚拟屏多屏映射、E0 前缀扫描码）
- **音频**：WASAPI 环回采集（float32→i16、20ms 块）
- **服务**：rc-daemon Windows Service（防火墙规则自动管理、会话监管、开机自启）

### 工具与诊断

- `rc-cli probe`：硬件能力探测（D3D11/DXGI/MF/WASAPI）
- `rc-cli loopback`：进程内采集→编码→解码→渲染端到端压测
- `rc-cli auth-probe`：登录联调回归
- `scripts/e2e/`：UIA 联测自动化脚本

## 架构设计要点

1. **共享 D3D11 设备（`rc-d3d11`）**：DXGI 采集输出、MF 编码输入、MF 解码输出、渲染上屏在同一设备上零拷贝。全进程设备单例，后端间传递裸纹理指针。
2. **统一 Win32 基础层（`rc-win32core`）**：COM 套间、隐藏消息窗口与消息泵、MMCSS 线程 QoS、电源请求、QPC 时钟集中管理。
3. **被控端双进程**：`rc-daemon`（LocalSystem 服务，Session 0）管提权操作（UAC/锁屏注入代理、agent 监管）；`rc-host-win`（用户会话 agent）是采集/注入/渲染执行者。两者经命名管道通信。

## 仓库结构（14 crates）

| 分层 | Crate | 职责 |
|---|---|---|
| 平台基础 | `rc-win32core`, `rc-d3d11` | COM 套间/消息泵/MMCSS/时钟；共享 D3D11 设备上下文 |
| 硬件后端 | `rc-capture-win`, `rc-encode-mf`, `rc-decode-mf`, `rc-render-win` | DXGI Duplication 采集、MF 零拷贝 H.264 编解码（`GetOutputAvailableType` 协商）、D3D11 渲染 |
| 系统后端 | `rc-input-win`, `rc-audio-win`, `rc-clipboard-win` | SendInput 注入、WASAPI 环回采集与播放、Win32 剪贴板 |
| 被控端 | `rc-host-win`, `rc-daemon` | 用户会话代理与 LocalSystem 服务 |
| 平台总成 | `rc-platform-win` | 统一注入 rabbitcontrol-core 的注册入口 |
| 应用 | `rc-ui`, `rc-cli` | XAML Islands 桌面端（登录/设备/会话/工具）；诊断命令行 |

## 构建与验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1

cargo run -p rc-ui          # 桌面端
cargo run -p rc-cli -- probe
cargo run -p rc-cli -- loopback --duration 10  # 端到端压测

# 发布打包（产出 dist/*.zip）
powershell -ExecutionPolicy Bypass -File scripts\package-win.ps1
```

`v*` tag push 触发 GitHub Actions 自动构建并上传 Release（`.github/workflows/release.yml`）。

## 本地联合调试

若需修改相邻的 `../rabbitcontrol-core`，在本地未跟踪的 `.cargo/config.toml` 中加入 `[patch."https://github.com/TokenBoom/rabbitcontrol-core.git"]` 将全部 18 个 `rc-*` crate 指向本地路径（README 旧版有完整清单）。

## 依赖

- core `v0.2.2`（git tag 锁定）
- Rust stable（MSVC），Windows 10 1903+
- VS 2022 Build Tools「使用 C++ 的桌面开发」工作负载
- rc-ui 额外依赖 windows-rs 0.37（XAML Islands 投影）+ 0.58（DXGI/WASAPI），通过 `windows58` 别名共存
