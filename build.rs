//! Windows exe resources: the app icon (drawn in src/icon.rs) and version info, so Explorer, the
//! taskbar, the Start menu and Installed apps show them.
#[path = "src/icon.rs"]
#[allow(dead_code)]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("kxs.ico");
    std::fs::write(&ico, icon::ico(&[16, 20, 24, 32, 40, 48, 64, 256])).expect("write kxs.ico");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let n: Vec<u32> = version.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).chain(std::iter::repeat(0)).take(3).collect();
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "KXS Watcher"
      VALUE "ProductName", "KXS Watcher"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "kxs-watcher.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "\\\\"),
        a = n[0],
        b = n[1],
        c = n[2],
    );
    let rc_path = out.join("kxs.rc");
    std::fs::write(&rc_path, rc).expect("write kxs.rc");
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional().expect("compile the icon resource");
}
