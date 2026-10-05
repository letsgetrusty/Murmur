fn main() {
    // Kokoro (ONNX Runtime) has no prebuilt x86_64-macOS binaries, so it is
    // compiled only on other architectures. Keep in sync with the
    // `[target.'cfg(not(target_arch = "x86_64"))'.dependencies]` table in Cargo.toml.
    println!("cargo:rustc-check-cfg=cfg(kokoro)");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        println!("cargo:rustc-cfg=kokoro");
    }
    tauri_build::build()
}
