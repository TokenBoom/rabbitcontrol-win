//! rc-cli 子命令分发。

mod auth_probe;
mod loopback;

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
    /// 进程内采集→编码→解码→呈现回环压测（真实后端）。
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
        /// 严格模式（延迟不变式违例即失败）。
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
        Command::Loopback {
            duration,
            width,
            height,
            fps,
            strict,
        } => loopback::run(duration, width, height, fps, strict).map(|()| ExitCode::SUCCESS),
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
