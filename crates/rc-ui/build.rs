use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("app.manifest");
    let icon = manifest_dir.join("app.ico");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.ico");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ENV");

    match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => {
            // 优先链接桌面端同步导入库，避免旧版 windows-rs 选中 enclave 导出。
            println!("cargo:rustc-link-lib=dylib:+verbatim=Synchronization.Lib");
            for symbol in ["WaitOnAddress", "WakeByAddressSingle", "WakeByAddressAll"] {
                println!("cargo:rustc-link-arg=/INCLUDE:__imp_{symbol}");
            }
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
                manifest.display()
            );
            // exe 资源（图标，资源 ID 1）：任务栏/资源管理器/alt-tab 图标来源。
            embed_resource::compile("app.rc", embed_resource::NONE);
        }
        Ok("gnu") => {
            // GNU 链接器不接受 MSVC 清单参数，通过 windres 编译图标与清单资源嵌入二进制。
            let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("缺少 OUT_DIR"));
            let rc_path = out_dir.join("app.rc");
            let obj_path = out_dir.join("app_resources.o");
            let icon_posix = icon.to_string_lossy().replace('\\', "/");
            let manifest_posix = manifest.to_string_lossy().replace('\\', "/");
            fs::write(
                &rc_path,
                format!("1 ICON \"{icon_posix}\"\n1 24 \"{manifest_posix}\"\n"),
            )
            .expect("写入临时资源脚本失败");
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
