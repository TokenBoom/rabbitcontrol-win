//! 把应用图标与清单嵌入后台进程 exe：托盘图标与高 DPI（PerMonitorV2）显示都依赖它。
//!
//! 图标与清单复用桌面端的同一份（`crates/rc-ui/app.ico`、`app.manifest`），
//! 不另存一套——品牌图标必须与主程序完全一致。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR"));
    let ui_dir = manifest_dir
        .parent()
        .map(|p| p.join("rc-ui"))
        .expect("rc-host-win 应位于 workspace 的 crates 目录下");
    let icon = ui_dir.join("app.ico");
    let manifest = ui_dir.join("app.manifest");
    for file in [&icon, &manifest] {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ENV");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
    match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => {
            // 图标走资源脚本（资源 ID 1）；清单由链接器嵌入，与桌面端同一做法。
            // 路径必须用正斜杠：rc.exe 会把反斜杠当成转义序列。
            let rc_path = out_dir.join("agent.rc");
            fs::write(&rc_path, format!("1 ICON \"{}\"\n", posix(&icon)))
                .expect("写入资源脚本失败");
            embed_resource::compile(rc_path.display().to_string(), embed_resource::NONE);
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
                manifest.display()
            );
        }
        Ok("gnu") => {
            // GNU 链接器不接受 MSVC 清单参数，由 windres 把图标与清单一起编成目标文件
            let rc_path = out_dir.join("agent.rc");
            let obj_path = out_dir.join("agent_resources.o");
            fs::write(
                &rc_path,
                format!(
                    "1 ICON \"{}\"\n1 24 \"{}\"\n",
                    posix(&icon),
                    posix(&manifest)
                ),
            )
            .expect("写入资源脚本失败");
            let status = Command::new("windres")
                .args(["-O", "coff", "-i"])
                .arg(&rc_path)
                .arg("-o")
                .arg(&obj_path)
                .status()
                .expect("调用 windres 失败");
            assert!(status.success(), "windres 编译图标与清单资源失败");
            println!("cargo:rustc-link-arg-bins={}", obj_path.display());
        }
        _ => {}
    }
}
