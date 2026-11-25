# RabbitControl Windows（`TokenBoom/rabbitcontrol-win`）

RabbitControl Windows 客户端与硬件适配仓库，基于 [`TokenBoom/rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core) 构建，包含 Windows 原生低延迟硬件后端、XAML 桌面应用与被控驻留托盘。架构对齐 [`TokenBoom/rabbitcontrol-macos`](https://github.com/TokenBoom/rabbitcontrol-macos)。

## 仓库结构（11 crates）

| 分层 | Crate | 职责 |
|---|---|---|
| Windows 硬件与系统后端 | `rc-capture-win`, `rc-encode-mf`, `rc-decode-mf`, `rc-render-win`, `rc-input-win`, `rc-audio-win`, `rc-clipboard-win` | DXGI Desktop Duplication / WGC 采集、Media Foundation 零拷贝 H.264/HEVC 硬件编解码、D3D11 零拷贝渲染、SendInput 输入注入与低级钩子捕获、WASAPI 环回采集与低延迟播放、Win32 剪贴板 |
| 被控驻留与平台总成 | `rc-host-win`, `rc-platform-win` | 系统托盘/审批窗，以及将全部 Windows 提供者统一注入 `rabbitcontrol-core` 的注册总入口 |
| 桌面端与诊断 | `rc-ui`, `rc-cli` | 原生 XAML Islands 桌面端（登录/设备列表/D3D11 桌面视口）、硬件探测与回环压测命令行 |

## 平台客户端接入方式

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

在登录页选择「服务器设置」并填写 RabbitControl 服务端地址。只填写主机名时默认使用 HTTPS 和端口 21118，也可以指定完整地址，例如 `https://remote.example.com`。本机回环地址支持 HTTP 调试。首次启动使用 `https://127.0.0.1:21118`。保存后新地址立即生效，并写入当前用户注册表 `HKCU\Software\RabbitControl\Windows` 的 `ServerOrigin` 值，重启后可继续使用。

使用该服务器的账号邮箱和密码登录。客户端在后台请求 `POST /api/v1/auth/login`，令牌仅保存在进程内存中。登录成功后显示账号和 1280 × 720 的待连接桌面。选择「退出登录」会调用 `POST /api/v1/auth/logout` 并回到登录页。

## rc-ui 结构

- `src/shell.rs` 用 windows-rs 封装 Win32 窗口、STA 消息循环和系统 XAML Island 的生命周期。
- `src/view.xaml` 定义主界面和桌面视口，编译时嵌入可执行文件。
- `src/app.rs` 管理页面和异步结果，`src/auth.rs` 处理认证请求，`src/settings.rs` 解析和持久化服务器地址。
- `src/graphics.rs` 将 D3D11 的 DXGI composition swap chain 绑定到 SwapChainPanel，呈现空闲帧。硬件设备不可用时使用 WARP。
- `src/desktop.rs` 使用 IronRDP 的 Display Control 类型建立初始主显示器配置。该尺寸同时用于交换链和状态栏。RDP 网络握手、解码和输入转发留给后续会话层实现。
