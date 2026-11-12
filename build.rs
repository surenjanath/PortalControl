//! Windows only: put the application icon, version details and a manifest
//! (DPI awareness, modern common controls) into the executable. Other platforms build nothing extra.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

/// Record which commit this build came from and when it was made, for the
/// About page. Both are optional: a build outside a Git checkout has no commit.
fn stamp() {
    // Rebuild the stamp whenever the sources or the checked-out commit change.
    for watched in ["src", "web", "Cargo.toml", ".git/HEAD", ".git/refs/heads"] {
        println!("cargo:rerun-if-changed={watched}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(commit) = git(&["rev-parse", "--short", "HEAD"]).filter(|c| !c.is_empty()) {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        println!("cargo:rustc-env=PORTALCONTROL_COMMIT={commit}{}", if dirty { " + local changes" } else { "" });
    }
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        println!("cargo:rustc-env=PORTALCONTROL_BUILT={}", now.as_secs());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    stamp();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    // Resource id 1; `tray.rs` loads the tray icon by that id.
    res.set_icon_with_id("assets/icon.ico", "1");
    res.set("ProductName", "PortalControl");
    res.set("FileDescription", "PortalControl");
    res.set_manifest(MANIFEST);
    // Required, not optional: the About window needs the modern common
    // controls that this manifest asks for, and Windows refuses to start the
    // program without them.
    if let Err(e) = res.compile() {
        panic!("could not embed the Windows icon and manifest (is the Windows SDK resource compiler installed?): {e}");
    }
}
