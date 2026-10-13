//! Embeds Windows resources into both binaries: icon, version info (shown in
//! Explorer, Task Manager and Settings → Apps) and an application manifest
//! (no elevation, per-monitor DPI awareness so the tray menu stays sharp).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/simonaircast.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir
        .join("../../assets/simonaircast.ico")
        .canonicalize()
        .expect("assets/simonaircast.ico");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = numeric_version(&version);

    let manifest = out.join("simonaircast.manifest");
    std::fs::write(&manifest, MANIFEST.replace("{VERSION}", &numeric.join("."))).unwrap();

    for (bin, description) in [
        ("simonaircast-tray", "simonaircast"),
        ("simonaircast", "simonaircast command-line tool"),
    ] {
        let rc = out.join(format!("{bin}.rc"));
        let text = RC
            .replace("{ICON}", &rc_path(&icon))
            .replace("{MANIFEST}", &rc_path(&manifest))
            .replace("{VERSION_COMMAS}", &numeric.join(","))
            .replace("{VERSION}", &version)
            .replace("{DESCRIPTION}", description)
            .replace("{EXE}", &format!("{bin}.exe"));
        std::fs::write(&rc, text).unwrap();
        embed_resource::compile_for(&rc, [bin], embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}

/// "0.1.0" -> ["0", "1", "0", "0"] (Windows versions have four parts).
fn numeric_version(version: &str) -> Vec<String> {
    let mut parts: Vec<String> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse::<u16>().unwrap_or(0).to_string())
        .collect();
    parts.resize(4, "0".into());
    parts
}

/// Paths in .rc files are C string literals: escape backslashes.
fn rc_path(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "\\\\")
}

const RC: &str = r#"
1 ICON "{ICON}"
1 24 "{MANIFEST}"

1 VERSIONINFO
FILEVERSION {VERSION_COMMAS}
PRODUCTVERSION {VERSION_COMMAS}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "simonaircast"
            VALUE "FileDescription", "{DESCRIPTION}"
            VALUE "FileVersion", "{VERSION}"
            VALUE "InternalName", "{EXE}"
            VALUE "LegalCopyright", "MIT License"
            VALUE "OriginalFilename", "{EXE}"
            VALUE "ProductName", "simonaircast"
            VALUE "ProductVersion", "{VERSION}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="simonaircast" version="{VERSION}" processorArchitecture="amd64"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;
