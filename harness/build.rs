// Embeds a VERSIONINFO resource and an application manifest into Windows
// builds. Security tools (and people reading file properties) treat an .exe
// with no product name, publisher or version as anonymous, which counts
// against it. Other targets build nothing here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=windows/buildwithnexus.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "buildwithnexus")
        .set("FileDescription", "buildwithnexus: agentic coding CLI")
        .set("CompanyName", "buildwithnexus (Garretts-Apps)")
        .set("LegalCopyright", "MIT License")
        .set("InternalName", "buildwithnexus")
        .set("OriginalFilename", "buildwithnexus.exe")
        .set(
            "Comments",
            "Open source: https://github.com/Garretts-Apps/buildwithnexus",
        )
        .set_manifest(include_str!("windows/buildwithnexus.manifest"));
    // A machine without a resource compiler (a bare `cargo install`) still
    // gets a working binary, just without the resource. The release workflow
    // checks that its .exe has one.
    if let Err(e) = res.compile() {
        println!("cargo:warning=Windows version resource not embedded: {e}");
    }
}
