//! ScreenCaptureKit is reached through a Swift bridge, which links against the
//! Swift compatibility libraries. The bridge looks for them in the full Xcode
//! toolchain; with only the Command Line Tools installed they live elsewhere,
//! so the active toolchain's own library folder is added to the search path.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(out) = Command::new("xcrun").args(["--find", "swift"]).output() else {
        return;
    };
    let swift = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let Some(usr) = swift.parent().and_then(|bin| bin.parent()) else {
        return;
    };
    let lib = usr.join("lib/swift/macosx");
    if lib.is_dir() {
        println!("cargo:rustc-link-search=native={}", lib.display());
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
