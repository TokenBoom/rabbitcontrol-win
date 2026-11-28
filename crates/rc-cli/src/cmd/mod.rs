//! rc-cli 子命令分发。

mod auth_probe;
mod capture_probe;
mod conv_bench;
mod hw_loop;
mod loopback;
mod power_probe;
mod relay_check;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// 命令行入口。
#[derive(Parser)]
#[command(name = "rc-cli", version, about = "RabbitControl Windows 诊断命令行")]
pub struct Cli {
    /// 子命令。
    #[command(subcommand)]
    pub command: Command,
}

/// 子命令集合。
#[derive(Subcommand)]
pub enum Command {
    /// 探测硬件能力（采集/编解码/渲染/音频）。
    Probe,
    /// 单独验证 DXGI 桌面复制能否产出画面帧（不经过会话管线）。
    CaptureProbe {
        /// 运行时长（秒）。
        #[arg(long, default_value_t = 5)]
        duration: u64,
        /// 创建 duplication 后先静置多久再取帧（秒）。
        #[arg(long, default_value_t = 0)]
        settle: u64,
    },
    /// 合成帧 → 编码 → 解码 → 读回校验，单独验证编解码链路（不经过采集）。
    HwLoop {
        /// 分辨率宽。
        #[arg(long, default_value_t = 1920)]
        width: u32,
        /// 分辨率高。
        #[arg(long, default_value_t = 1080)]
        height: u32,
        /// 帧率。
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// 帧数 = 帧率 × 秒数（上限 600 帧）。
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// 实测 BGRA → NV12 视频处理器转换（含缩放）的 GPU 耗时。
    ConvBench {
        /// 输入宽（桌面分辨率）。
        #[arg(long, default_value_t = 3840)]
        width: u32,
        /// 输入高。
        #[arg(long, default_value_t = 2160)]
        height: u32,
        /// 输出宽（编码分辨率）。
        #[arg(long, default_value_t = 1920)]
        out_width: u32,
        /// 输出高。
        #[arg(long, default_value_t = 1080)]
        out_height: u32,
        /// 连做多少次取平均。
        #[arg(long, default_value_t = 60)]
        iterations: u32,
    },
    /// 验证桌面采集的跨设备中转链路（采集专用设备 → 共享纹理 → 共享设备 → NV12）。
    RelayCheck,
    /// 验证显示器熄屏通知、熄屏唤醒与锁屏查询（被控端「没画面」时先查这里）。
    PowerProbe {
        /// 运行时长（秒）。
        #[arg(long, default_value_t = 5)]
        duration: u64,
        /// 显示器熄屏时执行唤醒并保持常亮。
        #[arg(long, default_value_t = false)]
        wake: bool,
        /// 无论熄屏与否都持有「显示器常亮 + 系统不眠」电源请求。
        #[arg(long, default_value_t = false)]
        hold: bool,
    },
    /// 进程内全链路回环（采集→编码→打包→解包→解码→呈现，Windows 真实后端）。
    Loopback {
        /// 运行时长（秒）。
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// 分辨率宽。
        #[arg(long, default_value_t = 1920)]
        width: u32,
        /// 分辨率高。
        #[arg(long, default_value_t = 1080)]
        height: u32,
        /// 目标帧率。
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// 采集源（auto = 桌面采集没挂起就用它，否则用合成帧）。
        #[arg(long, value_enum, default_value_t = loopback::Source::Auto)]
        source: loopback::Source,
        /// 严格模式：必须用真实桌面采集，不允许退到合成帧。
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
    /// 联调登录回归（探测 → 登录 → 设备列表，凭据不落盘）。
    AuthProbe {
        /// 服务器地址（默认 https://，本机调试可用 http://loopback）。
        #[arg(long)]
        origin: String,
        /// 账号邮箱。
        #[arg(long)]
        email: String,
        /// 账号密码。
        #[arg(long)]
        password: String,
    },
    /// 输出版本信息。
    Version,
}

/// 分发执行。
pub fn dispatch(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Probe => {
            run_probe();
            Ok(ExitCode::SUCCESS)
        }
        Command::CaptureProbe { duration, settle } => {
            capture_probe::run(duration, settle).map(|()| ExitCode::SUCCESS)
        }
        Command::HwLoop {
            width,
            height,
            fps,
            seconds,
        } => hw_loop::run(width, height, fps, seconds).map(|()| ExitCode::SUCCESS),
        Command::ConvBench {
            width,
            height,
            out_width,
            out_height,
            iterations,
        } => conv_bench::run(width, height, out_width, out_height, iterations)
            .map(|()| ExitCode::SUCCESS),
        Command::RelayCheck => relay_check::run().map(|()| ExitCode::SUCCESS),
        Command::PowerProbe {
            duration,
            wake,
            hold,
        } => power_probe::run(duration, wake, hold).map(|()| ExitCode::SUCCESS),
        Command::Loopback {
            duration,
            width,
            height,
            fps,
            source,
            strict,
        } => {
            loopback::run(duration, width, height, fps, source, strict).map(|()| ExitCode::SUCCESS)
        }
        Command::AuthProbe {
            origin,
            email,
            password,
        } => auth_probe::run(&origin, &email, &password).map(|()| ExitCode::SUCCESS),
        Command::Version => {
            println!("rc-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// 硬件能力探测：D3D11 设备 / DXGI 输出 / MF 编解码 MFT。
fn run_probe() {
    println!("== D3D11 设备 ==");
    match rc_d3d11::probe::probe_device() {
        Ok(device) => println!(
            "  [{kind}] {desc} VID 0x{vid:04X} DID 0x{did:04X} 显存 {mem} MiB 特性级别 0x{fl:04X}",
            kind = match device.kind {
                rc_d3d11::DeviceKind::Hardware => "硬件",
                rc_d3d11::DeviceKind::Warp => "WARP",
            },
            desc = device.adapter_description,
            vid = device.vendor_id,
            did = device.device_id,
            mem = device.dedicated_video_memory_bytes / (1024 * 1024),
            fl = device.feature_level,
        ),
        Err(err) => println!("  探测失败：{err}"),
    }

    println!("== 显示输出 ==");
    match rc_capture_win::probe::probe_outputs() {
        Ok(outputs) if outputs.is_empty() => println!("  （未发现任何输出）"),
        Ok(outputs) => {
            for output in outputs {
                println!(
                    "  {name}: {w}×{h} @({l},{t}) 旋转 {rot} [{}]",
                    if output.attached_to_desktop {
                        "已接入"
                    } else {
                        "未接入"
                    },
                    name = output.output_name,
                    w = output.desktop_rect.2 - output.desktop_rect.0,
                    h = output.desktop_rect.3 - output.desktop_rect.1,
                    l = output.desktop_rect.0,
                    t = output.desktop_rect.1,
                    rot = output.rotation,
                );
                println!("    所属适配器：{}", output.adapter_description);
            }
        }
        Err(err) => println!("  探测失败：{err}"),
    }

    for (title, list, result) in [
        (
            "视频编码 MFT",
            "编码",
            rc_encode_mf::probe::probe_encoders(),
        ),
        (
            "视频解码 MFT",
            "解码",
            rc_decode_mf::probe::probe_decoders(),
        ),
    ] {
        println!("== {title} ==");
        match result {
            Ok(mfts) if mfts.is_empty() => println!("  （未发现 MFT）"),
            Ok(mfts) => {
                for mft in mfts {
                    println!(
                        "  [{tag}] {name}",
                        tag = if mft.hardware { "硬件" } else { "软件" },
                        name = mft.friendly_name,
                    );
                }
            }
            Err(err) => println!("  {list}探测失败：{err}"),
        }
    }
}
