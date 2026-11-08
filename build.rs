//! Puts the program icon (`assets/panoweave.ico`, made from `assets/logo.svg`) into the Windows .exe,
//! so Explorer and the task bar show PanoWeave's icon. Other systems: nothing to do.

fn main() {
    println!("cargo:rerun-if-changed=assets/panoweave.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/panoweave.ico");
        if let Err(e) = res.compile() {
            // a missing resource compiler must not stop the build: the exe then has the default icon
            println!("cargo:warning=program icon not embedded: {e}");
        }
    }
}
