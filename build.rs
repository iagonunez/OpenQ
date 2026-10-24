//! Embeds the app icon (`assets/icon/opnq.ico`) in the Windows executable, so Explorer, the Start
//! menu shortcut and "Installed apps" show it.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/opnq.ico");
    // Checked at run time rather than with cfg!: the build script runs on the host, so this also
    // covers cross-compiling for Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/opnq.ico");
    if let Err(e) = res.compile() {
        // A real Windows build (MSVC, with the Windows SDK's rc.exe) must carry the icon; a GNU
        // cross-check without windres only loses it.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            panic!("cannot embed the Windows icon: {e}");
        }
        println!("cargo:warning=Windows icon not embedded: {e}");
    }
}
