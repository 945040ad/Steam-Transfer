fn main() {
    // Windows 11 look: Slint's Fluent style follows the system light/dark mode and accent colour.
    slint_build::compile_with_config("ui/app.slint", slint_build::CompilerConfiguration::new().with_style("fluent".into())).expect("Slint UI failed to compile");

    // Exe icon + manifest (per-monitor DPI, modern common controls for message boxes).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        res.set_manifest_file("app.manifest");
        res.set("ProductName", "Steam Game Transfer");
        res.set("FileDescription", "Steam Game Transfer");
        res.compile().expect("failed to embed Windows resources");
    }
    println!("cargo:rerun-if-changed=app.ico");
    println!("cargo:rerun-if-changed=app.manifest");
}
