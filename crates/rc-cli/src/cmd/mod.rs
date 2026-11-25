//! rc-cli 子命令分发（骨架）。

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
    /// 输出版本信息。
    Version,
}

/// 分发执行。
pub fn dispatch(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Probe => {
            println!("probe: 硬件探测尚未实现（骨架）");
            Ok(ExitCode::SUCCESS)
        }
        Command::Version => {
            println!("rc-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
    }
}
