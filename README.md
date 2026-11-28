# RabbitControl Windows（`TokenBoom/rabbitcontrol-win`）

RabbitControl Windows 客户端与硬件适配仓库，基于 [`TokenBoom/rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core) v0.2.7 构建：原生低延迟硬件后端、XAML Islands 桌面应用、被控端服务与用户会话代理。

## 功能概览

### 主控端（控制远端设备）

- **登录**：服务器探测 + Root CA TOFU 校验 + 设备绑定 + mTLS 设备证书签发 + Token 自动轮换
- **设备管理**：设备列表/在线状态（按名称或设备码搜索，不分大小写）、最近连接/收藏历史、9 位码连接、LAN 直连加速；信令重新连上、本机设备码变化、主窗口回到前台（30 秒内不重复）时自动静默刷新
- **会话**：独立可缩放窗口（全屏/Esc 退出）、D3D11 硬件渲染 letterbox、性能 HUD（时长/传输/帧率/码率/RTT/丢包）、画质级联菜单（分辨率/帧率/码率/编解码/鼠标模式/多屏切换热切换，当前项打勾）、仅观看模式
- **输入**：XAML Island 指针/键盘捕获、Set-1 扫描码 + E0 前缀注入、虚拟屏多屏映射
- **工具通道**：
  - 终端：真终端（自研 VT100/xterm 屏幕模型）：16/256/真彩色、粗体/斜体/下划线/删除线/反显、全角字符按列对齐、备用屏幕（vim/less 这类全屏程序）、滚动历史（滚轮翻看、翻看时来了新输出有提示）、拖动 / 双击 / 三击选区与复制、括号粘贴、输入法直接可用、按键编码与 macOS 端的 xterm.js 一致；被控端锁屏时先验证系统密码（只校验，不解锁屏幕）；对方不允许用终端时给出说明
  - 文件传输（上传/下载/取消/进度）、端口映射（添加/暂停/启用/删除/吞吐统计）
  - 工具面板宽度可拖右侧分隔条调整
- **剪贴板**：CF_UNICODETEXT 双向同步（有界重试、回环抑制）
- **音频**：WASAPI 共享模式播放（AUTOCONVERTPCM 引擎转换）
- **重连**：异常断开 3s×3 自动重连、SessionSwitching 快速接续
- **标题栏**（对齐 UU 远程）：右上角「更多 / 账号 / 新版本提醒 | 最小化 / 关闭」。「更多」里是安全锁定 / 提交反馈 / 检查更新 / 关于；账号弹窗显示当前登录、修改密码、退出登录；有新版本时右上角出现绿色提醒，更新弹窗里有版本、更新说明、「立即更新」（打开安装包的下载地址）和「自动检查更新」开关
- **客户端安全锁定**（对齐 UU 远程「安全锁定」）：在「更多 → 安全锁定」里设置 8–16 位安全密钥后，可以一键锁定客户端。锁定期间本地不能修改任何客户端设置、不能退出登录、也不能关闭客户端；发起远程控制不受影响。可选「远程协助本设备期间自动锁定」「启动时自动锁定」（设置 → 安全）

### 被控端（允许远端控制本机）

- **信令接入**：UPnP 端口映射、中继/LAN 双路直入、同账号 OwnerTicket 免密自控
- **审批**：结构化审批弹窗（对端地址/分辨率/密码状态），超时自动拒绝
- **采集**：DXGI Desktop Duplication（静态帧零输出、分辨率变化自动重建）
- **编码**：MF H.264 硬件编码（零拷贝 NV12 纹理入、CBR + 低延迟 + 按需 IDR）
- **注入**：SendInput（虚拟屏多屏映射、E0 前缀扫描码）
- **音频**：WASAPI 环回采集（float32→i16、20ms 块）
- **后台被控进程 `rc-host-agent`**：主界面关掉以后仍保持在线、可被控；托盘图标显示状态，菜单提供打开主界面 / 开关被控 / 断开连接 / 退出；有人请求控制时弹出独立的审批小窗（主界面没开也能审批）
- **开关持久化**：「允许被控」开一次之后，关窗、重启都保持（默认关闭，必须本人显式开启）
- **服务**：rc-daemon Windows Service（登录时拉起后台进程、防火墙规则自动管理、UAC/锁屏注入代理、开机自启）

### 工具与诊断

- `rc-cli probe`：硬件能力探测（D3D11/DXGI/MF/WASAPI）
- `rc-cli loopback`：进程内全链路回环（采集→编码→打包→解包→解码→呈现，Windows 真实后端）；`--source dxgi` 采真实桌面、`--source synthetic` 用合成动画帧（锁屏 / 熄屏时也能跑）、默认 auto
- `rc-cli hw-loop`：合成帧→编码→解码→读回校验，单独验证编解码链路
- `rc-cli capture-probe`：DXGI 桌面复制能否出帧（不经过会话管线）
- `rc-cli power-probe`：显示器熄屏通知、熄屏唤醒、锁屏查询
- `rc-cli relay-check`：桌面采集的跨设备中转链路自检（不依赖桌面内容）
- `rc-cli conv-bench`：BGRA→NV12 转换的 GPU 耗时
- `rc-cli auth-probe`：登录联调回归
- `rabbitcontrol-win --connect-probe <设备码>`：用另一套配置目录（`RC_CONFIG_DIR`）作「第二台设备」，无界面地把协商结果和画面统计打出来
- `rabbitcontrol-win --demo-terminal`（仅调试构建，建议同时带 `--no-agent-handshake --no-activate`）：不连远端，直接开会话窗口演示——按节奏放一段有代表性的远端输出（颜色 / 全角字符 / 全屏程序 / 滚动历史），键入由本地回显；画质菜单给了一组样例值可以点选，终端能关能开。Windows 被控端没有终端服务，终端面板的实机核对靠它
- `--no-activate`（仅调试构建）：窗口显示时不抢前台。自动化核对界面时带上它，再用 `PrintWindow` 截图、UIA 的 Invoke / Toggle 触发控件，就不会打断正在用这台机器的人
- `scripts/e2e/`：UIA 联测自动化脚本

## 架构设计要点

1. **共享 D3D11 设备（`rc-d3d11`）**：色彩转换输出、MF 编码输入、MF 解码输出、渲染上屏在同一设备上零拷贝。全进程设备单例，后端间传递裸纹理指针。**桌面复制是例外**：它放在独立的采集专用设备上，帧经共享纹理中转——阻塞的 `AcquireNextFrame` 若等在共享设备上，会把同设备上其它线程的 D3D11 调用一并卡住，硬件编码器只能按显示器垂直同步周期出帧。
2. **统一 Win32 基础层（`rc-win32core`）**：COM 套间、隐藏消息窗口与消息泵、MMCSS 线程 QoS、电源请求、QPC 时钟集中管理。
3. **界面约定**：设计令牌与控件模板只有一份（`rc-ui/src/ui/assets/styles.xaml`），浅色主窗口与深色会话窗口共用。按钮一律不带边框线（实心 / 幽灵图标 / 文字 / 分段开关四种模板，悬停渐亮、按下缩放）；文本框、密码框、下拉是填充式，悬停 / 聚焦的底色与描边靠覆盖系统模板引用的 `TextControl*` 资源接管（聚焦前后底色不跳）。运行时用 `XamlReader` 加载的字典写不了 `ThemeDictionaries`，所以深色会话窗口的覆盖放在它自己的根 `Resources` 里、按窗口就近生效。标题栏与侧栏是同一块灰底、内容区更浅；导航项悬停浅灰，选中 = 灰底 + 紧贴窗口左缘的蓝色竖条；侧栏页脚只放服务连接状态，账号与退出登录在右上角的账号弹窗里。首页只放设备列表，本机 ID / 验证码 / 开关只在「远程协助」页：卡片分「标题行 + 分隔线 + 内容」（对齐 UU 远程）。下拉菜单的底色、文字、悬停色也通过覆盖 `MenuFlyout*` 资源接管（系统默认的悬停是 20% 黑，太重）。
4. **被控端三进程（与 macOS 客户端同一模型）**：`rabbitcontrol-win.exe`（主界面，控制端）、`rc-host-agent.exe`（后台被控进程：托盘、审批小窗、信令与被控服务）、`rc-daemon.exe`（LocalSystem 服务：登录时把后台进程拉进用户会话，以及需要提权的操作）。同一设备同一时刻只能有一条信令连接（服务端会以新连接顶掉旧连接），所以主界面打开期间由主界面托管、后台进程让位，主界面退出后后台进程自动接管。三者的信令运行时与被控编排共用 `rc-appcore` 里的同一份实现。

## 仓库结构（15 crates）

| 分层 | Crate | 职责 |
|---|---|---|
| 平台基础 | `rc-win32core`, `rc-d3d11` | COM 套间/消息泵/MMCSS/时钟；共享 D3D11 设备上下文 |
| 硬件后端 | `rc-capture-win`, `rc-encode-mf`, `rc-decode-mf`, `rc-render-win` | DXGI Duplication 采集、MF 零拷贝 H.264 编解码（`GetOutputAvailableType` 协商）、D3D11 渲染 |
| 系统后端 | `rc-input-win`, `rc-audio-win`, `rc-clipboard-win` | SendInput 注入、WASAPI 环回采集与播放、Win32 剪贴板 |
| 共享应用逻辑 | `rc-appcore` | 信令运行时、被控编排、认证文案、进程间管道协议（主界面与后台进程共用） |
| 被控端 | `rc-host-win`, `rc-daemon` | 后台被控进程 `rc-host-agent`（引擎/托盘/审批窗/管道服务）与 LocalSystem 服务 |
| 平台总成 | `rc-platform-win` | 统一注入 rabbitcontrol-core 的注册入口 |
| 应用 | `rc-ui`, `rc-cli` | XAML Islands 桌面端（登录/设备/会话/工具）；诊断命令行 |

## 构建与验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1

cargo run -p rc-ui          # 桌面端
cargo run -p rc-cli -- probe
cargo run -p rc-cli -- loopback --duration 10                 # 全链路回环（桌面锁屏时自动改用合成帧）
cargo run -p rc-cli -- loopback --source dxgi --strict        # 必须采真实桌面
cargo run -p rc-cli -- hw-loop --width 1920 --height 1080     # 只验编解码
cargo run -p rc-cli -- relay-check                            # 采集跨设备中转自检

# 发布打包（产出 dist/*.zip）
powershell -ExecutionPolicy Bypass -File scripts\package-win.ps1
```

`v*` tag push 触发 GitHub Actions 自动构建并上传 Release（`.github/workflows/release.yml`）。

## 图标

`crates/rc-ui/app-icon-source.png` 是设计方交付的 256×256 图标母版；`crates/rc-ui/app.ico` 由 `scripts/make-icon.py` 从母版生成——256 帧原样保留，其余 9 个尺寸（16–128）为母版降采样，全部是带 AND 掩码的 32bpp BMP 帧。换图标时只替换母版并重新运行脚本，不要手工重绘：

```powershell
python scripts/make-icon.py   # 需要 Pillow
```

应用内（标题栏 / 登录页）由 `rc-ui` 的 `ui/brand.rs` 按显示尺寸从 ICO 里挑最合适的帧，单元测试会校验 256 帧与母版逐像素一致。

## 本地联合调试

若需修改相邻的 `../rabbitcontrol-core`，在本地未跟踪的 `.cargo/config.toml` 中加入 `[patch."https://github.com/TokenBoom/rabbitcontrol-core.git"]` 将全部 18 个 `rc-*` crate 指向本地路径（README 旧版有完整清单）。

## 依赖

- core `v0.2.2`（git tag 锁定）
- Rust stable（MSVC），Windows 10 1903+
- VS 2022 Build Tools「使用 C++ 的桌面开发」工作负载
- rc-ui 额外依赖 windows-rs 0.37（XAML Islands 投影）+ 0.58（DXGI/WASAPI），通过 `windows58` 别名共存

## 信任与安全

本项目为开源软件，代码签名证书（付费）暂未购买。用户可通过以下方式验证二进制可信：

1. **源码可审计**：全部代码托管于 GitHub，构建流程（CI + 本地脚本）公开可查
2. **SHA256 校验**：每次 Release 附带 zip 包，下载后可用 `Get-FileHash` 校验一致性
3. **防火墙路径规则**：安装守护服务时自动为三个程序注册路径级允许规则，重编译不影响授权
4. **命名管道访问控制**：守护服务管道只允许本机交互登录的用户、SYSTEM 与管理员连接；注入键鼠这类特权操作逐条核对调用方，必须是当前桌面会话的登录用户本人
5. **网络隔离**：被控端仅在用户显式开启「允许被控」开关后才监听端口；每次连接均需审批或密码验证；开关默认关闭
6. **后台进程可退出**：托盘菜单「退出 RabbitControl」会同时停掉后台进程并让守护服务不再拉起，下次打开主界面时恢复
7. **客户端安全锁定**：安全密钥只保存「随机盐 + PBKDF2-HMAC-SHA256（21 万轮）派生值」，按登录账号存于配置目录的 `client-lock.json`，保存后不可查看；连续输错 5 次起进入冷却。锁定只限制本地的设置修改与退出，不影响发起连接。忘了密钥：在锁定页面点「忘记密钥？退出登录」，重新登录（需要账号密码）后重新设置——正在远程协助这台机器的人没有账号密码，绕不过锁定

首次运行时 Windows 防火墙会为主界面、后台进程这两个程序各弹一次入站确认（未签名 exe 的正常行为），点「允许」即可；安装后台服务后自动注册路径规则，之后不再弹。开发时 `cargo test` 不会触发防火墙确认（单测不打开监听端口）。
