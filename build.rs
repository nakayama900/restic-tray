fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=Cargo.toml");

    #[cfg(windows)]
    {
        if std::path::Path::new("assets/icon.ico").exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "Restic Tray");
            res.set(
                "FileDescription",
                "Restic Tray // Periodic Restic Backup for Cloudflare R2 & MCP Server",
            );
            res.set("CompanyName", "nayuengin");
            res.set("LegalCopyright", "Copyright (c) 2026 nayuengin (MIT)");
            if let Err(err) = res.compile() {
                println!("cargo:warning=winresource compile skipped or failed: {err}");
            }
        }
    }
}
