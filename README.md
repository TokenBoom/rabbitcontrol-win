# RabbitControl for Windows

Rust 原生 Windows 客户端骨架。启动后显示未连接的桌面视图，窗口可缩放，可离线运行。

## 运行

在 Windows 10 1903 或更高版本的桌面环境中安装 Rust stable（MSVC）及 Visual Studio 2022 Build Tools 的「使用 C++ 的桌面开发」工作负载，包含 MSVC x64 工具链和 Windows SDK。

```powershell
cargo run --locked
```

发布构建使用 `cargo build --locked --release`，可执行文件位于 `target/release/rabbitcontrol-win.exe`。

## 结构

- `src/shell.rs` 用 windows-rs 封装 Win32 窗口、STA 消息循环和系统 XAML Island 的生命周期。
- `src/view.xaml` 定义主界面和桌面视口，编译时嵌入可执行文件。
- `src/graphics.rs` 将 D3D11 的 DXGI composition swap chain 绑定到 SwapChainPanel，呈现空闲帧。硬件设备不可用时使用 WARP。
- `src/desktop.rs` 使用 IronRDP 的 Display Control 类型建立初始主显示器配置。该尺寸同时用于交换链和状态栏。RDP 网络握手、解码和输入转发留给后续会话层实现。
