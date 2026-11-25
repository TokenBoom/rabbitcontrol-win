fn main() {
    // Resolve WaitOnAddress and WakeByAddress* from the desktop SDK import library
    // before windows-rs 0.37's umbrella library can select the enclave DLL.
    println!("cargo:rustc-link-lib=dylib:+verbatim=Synchronization.Lib");
    for symbol in ["WaitOnAddress", "WakeByAddressSingle", "WakeByAddressAll"] {
        println!("cargo:rustc-link-arg=/INCLUDE:__imp_{symbol}");
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
