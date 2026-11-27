# RabbitControl Windows（`TokenBoom/rabbitcontrol-win`）

RabbitControl Windows 客户端与硬件适配仓库，基于 [`TokenBoom/rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core) 构建：原生低延迟硬件后端、XAML 桌面应用、被控端服务与用户会话代理。

## 架构设计要点

Windows 平台特性决定的三个核心设计：

1. **共享 D3D11 设备（`rc-d3d11`）**：DXGI 采集输出、Media Foundation 编码输入、MF 解码输出、渲染上屏必须在同一个 D3D11 设备上才能零拷贝（D3D11 纹理默认只在创建它的设备上可用）。全进程设备单例 + TDR 统一重建，各后端传递裸纹理指针，避免共享句柄与键控互斥的开销。
2. **统一 Win32 基础层（`rc-win32core`）**：剪贴板监听、托盘、显示变更、会话锁定在 Windows 上都是窗口消息；WASAPI/MF/WGC 都要求正确的 COM 套间。COM 初始化、隐藏消息窗口与消息泵、MMCSS 线程 QoS、电源请求、QPC 主机时钟集中管理，各后端订阅接入。
3. **被控端双进程**：Windows 服务运行在 Session 0，摸不到用户桌面。`rc-daemon`（LocalSystem 服务）只管提权操作——UAC/锁屏输入注入代理、显示模式切换、agent 监管；`rc-host-win`（用户会话 agent）是采集/注入/渲染的实际执行者，并提供托盘与连接审批 UI。两者经命名管道通信。

## 仓库结构（14 crates）

| 分层 | Crate | 职责 |
|---|---|---|
| 平台基础 | `rc-win32core`, `rc-d3d11` | COM 套间/消息泵/MMCSS/电源请求/QPC 时钟；共享 D3D11 设备上下文与 TDR 重建 |
| 硬件后端 | `rc-capture-win`, `rc-encode-mf`, `rc-decode-mf`, `rc-render-win` | DXGI Desktop Duplication / WGC 采集与显示器控制；Media Foundation 零拷贝 H.264/HEVC 硬件编解码；D3D11 渲染 |
| 系统后端 | `rc-input-win`, `rc-audio-win`, `rc-clipboard-win` | SendInput 注入与低级钩子捕获；WASAPI 环回采集与播放；Win32 剪贴板 |
| 被控端 | `rc-host-win`, `rc-daemon` | 用户会话代理（托盘/审批/媒体执行）与 LocalSystem 服务（提权代理/进程监管） |
| 平台总成 | `rc-platform-win` | 将全部后端统一注入 rabbitcontrol-core 的注册入口 |
| 应用 | `rc-ui`, `rc-cli` | 原生 XAML Islands 桌面端（登录/设备列表/D3D11 桌面视口）；诊断命令行 |

## 平台接入方式

`rc-platform-win::register_all()` 幂等注册：

1. `rc_pipeline::register_platform_provider(...)` — DXGI 采集 / MF 编解码 / D3D11 渲染与主机单调时钟
2. `rc_audio::register_audio_provider(...)` — WASAPI 环回采集与播放
3. `rc_session::register_clipboard_provider(...)` — Win32 剪贴板
4. `rc_client_core::platform::register_platform_hooks(...)` — 显示器查询、分辨率切换、锁屏、输入注入与渲染表面挂接

## 本地联合调试 `rabbitcontrol-core`

默认 `Cargo.toml` 从 GitHub `TokenBoom/rabbitcontrol-core` 拉取 `tag = "v0.1.14"`。若本地同时修改了相邻的 `../rabbitcontrol-core`，在本地未跟踪的 `.cargo/config.toml` 中加入：

```toml
[patch."https://github.com/TokenBoom/rabbitcontrol-core.git"]
rc-types = { path = "../rabbitcontrol-core/crates/rc-types" }
rc-diff = { path = "../rabbitcontrol-core/crates/rc-diff" }
rc-sched = { path = "../rabbitcontrol-core/crates/rc-sched" }
rc-fec = { path = "../rabbitcontrol-core/crates/rc-fec" }
rc-proto = { path = "../rabbitcontrol-core/crates/rc-proto" }
rc-trace = { path = "../rabbitcontrol-core/crates/rc-trace" }
rc-loopback = { path = "../rabbitcontrol-core/crates/rc-loopback" }
rc-capture = { path = "../rabbitcontrol-core/crates/rc-capture" }
rc-encode = { path = "../rabbitcontrol-core/crates/rc-encode" }
rc-decode = { path = "../rabbitcontrol-core/crates/rc-decode" }
rc-render = { path = "../rabbitcontrol-core/crates/rc-render" }
rc-input = { path = "../rabbitcontrol-core/crates/rc-input" }
rc-audio = { path = "../rabbitcontrol-core/crates/rc-audio" }
rc-pipeline = { path = "../rabbitcontrol-core/crates/rc-pipeline" }
rc-net = { path = "../rabbitcontrol-core/crates/rc-net" }
rc-session = { path = "../rabbitcontrol-core/crates/rc-session" }
rc-tools = { path = "../rabbitcontrol-core/crates/rc-tools" }
rc-client-core = { path = "../rabbitcontrol-core/crates/rc-client-core" }
```

## 构建与验证

环境：Windows 10 1903+，Rust stable（MSVC），Visual Studio 2022 Build Tools 的「使用 C++ 的桌面开发」工作负载（MSVC x64 工具链 + Windows SDK）。

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1

# 运行桌面端
cargo run -p rc-ui

# 诊断命令行
cargo run -p rc-cli -- probe
```

发布构建使用 `cargo build --locked --release`，可执行文件位于 `target/release/rabbitcontrol-win.exe`。

## 当前状态

骨架阶段：全部 crate 接口已与 core 契约对齐并编译通过，硬件后端（DXGI/MF/D3D11/WASAPI/SendInput）为占位实现，后续逐个落地。

## 登录与服务器设置

在登录页选择「服务器设置」并填写 RabbitControl 服务端地址。只填写主机名时默认使用 HTTPS 和端口 21118，也可以指定完整地址，例如 `https://remote.example.com`。本机回环地址支持 HTTP 调试。保存地址时客户端会通过 `rc-client-core` 探测 `/api/v1/health` 与 `/api/v1/pki/ca`，完成 Root CA 的 TOFU 校验后将服务器配置原子持久化至 `%USERPROFILE%\.config\rabbitcontrol\server.json`。

使用该服务器的账号邮箱和密码登录。客户端通过 `rc-client-core` 的认证与证书管理器完成 `/api/v1/auth/login` 登录、设备绑定（`POST /api/v1/me/devices/bind`）与 mTLS 设备证书签发，并在后台线程自动轮换临期 Token 与续签 `< 48h` 设备证书。登录凭据保存在 `%USERPROFILE%\.config\rabbitcontrol\auth.json`，设备密钥与证书保存在 `%USERPROFILE%\.config\rabbitcontrol\certs\`，下次启动可静默恢复会话。登录成功后显示账号、本机 9 位设备码、已绑定设备摘要和 1280 × 720 的待连接桌面。选择「退出登录」会清理本地凭据与证书并通知服务端注销。

## rc-ui 结构

- `src/shell.rs` 用 windows-rs 封装 Win32 窗口、STA 消息循环和系统 XAML Island 的生命周期。
- `src/view.xaml` 定义主界面和桌面视口，编译时嵌入可执行文件。
- `src/app.rs` 管理页面和异步结果，`src/auth.rs` 接入 `rc-client-core` 的认证会话与设备证书管理，`src/settings.rs` 封装服务器地址解析、健康探测与 `server.json` 持久化。
- `src/graphics.rs` 将 D3D11 的 DXGI composition swap chain 绑定到 SwapChainPanel，呈现空闲帧。硬件设备不可用时使用 WARP。
- `src/desktop.rs` 使用 `rc-types::Resolution` 提供待连接桌面的初始尺寸配置。
