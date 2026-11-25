# RabbitControl for Windows

Rust 原生 Windows 客户端。启动后显示 RabbitControl 邮箱登录页。透明标题区域与页面共享背景，可拖动窗口、双击切换最大化，也可使用右上角的窗口按钮。

## 运行

在 Windows 10 1903 或更高版本的桌面环境中安装 Rust stable（MSVC）及 Visual Studio 2022 Build Tools 的「使用 C++ 的桌面开发」工作负载，包含 MSVC x64 工具链和 Windows SDK。

```powershell
cargo run --locked
```

发布构建使用 `cargo build --locked --release`，可执行文件位于 `target/release/rabbitcontrol-win.exe`。

## 登录与服务器设置

在登录页选择「服务器设置」并填写 RabbitControl 服务端地址。只填写主机名时默认使用 HTTPS 和端口 21118，也可以指定完整地址，例如 `https://remote.example.com`。本机回环地址支持 HTTP 调试。首次启动使用 `https://127.0.0.1:21118`。保存后新地址立即生效，并写入当前用户注册表 `HKCU\Software\RabbitControl\Windows` 的 `ServerOrigin` 值，重启后可继续使用。

使用该服务器的账号邮箱和密码登录。客户端在后台请求 `POST /api/v1/auth/login`，令牌仅保存在进程内存中。登录成功后显示账号和 1280 × 720 的待连接桌面。选择「退出登录」会调用 `POST /api/v1/auth/logout` 并回到登录页。

## 结构

- `src/shell.rs` 用 windows-rs 封装 Win32 窗口、STA 消息循环和系统 XAML Island 的生命周期。
- `src/view.xaml` 定义主界面和桌面视口，编译时嵌入可执行文件。
- `src/app.rs` 管理页面和异步结果，`src/auth.rs` 处理认证请求，`src/settings.rs` 解析和持久化服务器地址。
- `src/graphics.rs` 将 D3D11 的 DXGI composition swap chain 绑定到 SwapChainPanel，呈现空闲帧。硬件设备不可用时使用 WARP。
- `src/desktop.rs` 使用 IronRDP 的 Display Control 类型建立初始主显示器配置。该尺寸同时用于交换链和状态栏。RDP 网络握手、解码和输入转发留给后续会话层实现。
