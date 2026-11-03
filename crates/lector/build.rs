//! Embeds the application icon and version information into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/app-icon/lector.ico");
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("../../assets/app-icon/lector.ico")
                .set("ProductName", "Lector")
                .set("FileDescription", "Lector — reading-first code viewer")
                .set("InternalName", "lector")
                .set("OriginalFilename", "lector.exe")
                .set(
                    "LegalCopyright",
                    "Copyright (c) 2026 Lector contributors. MIT OR Apache-2.0.",
                );
            if let Err(err) = res.compile() {
                println!("cargo:warning=failed to embed Windows resources: {err}");
            }
        }
    }
}
