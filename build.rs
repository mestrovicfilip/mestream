//! Gives mestream.exe its icon and version details in Explorer.

fn main() {
    println!("cargo:rerun-if-changed=assets/mestream.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/mestream.ico");
        resource.set("ProductName", "Mestream");
        resource.set("FileDescription", "Mestream");
        resource.set("CompanyName", "MestApps");
        resource.set("LegalCopyright", "Copyright © 2026 Filip Meštrović, MestApps");
        if let Err(error) = resource.compile() {
            // The icon is a nicety: a machine without the resource compiler still builds.
            println!("cargo:warning=mestream.exe will have no icon: {error}");
        }
    }
}
