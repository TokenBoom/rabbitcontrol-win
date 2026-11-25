//! RabbitControl Windows 诊断命令行（对应 macOS `rc-cli`）。
//!
//! 子命令规划：
//! - `probe`：硬件能力探测（DXGI 采集、MF 编解码、D3D11 渲染、WASAPI 音频）；
//! - `loopback`：进程内端到端回环压测；
//! - `bench`：性能 Gate 压测（帧率/延迟/码率）；
//! - `host` / `client`：独立双端运行。

mod cmd;

use std::process::ExitCode;

use clap::Parser;

use cmd::Cli;

fn main() -> ExitCode {
    rc_platform_win::register_all();

    let cli = Cli::parse();
    match cmd::dispatch(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}
