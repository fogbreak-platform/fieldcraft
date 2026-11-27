//! Windows builds embed the application icon (assets/app-icon/icon.ico) in `fieldcraft.exe`, so
//! Explorer, the taskbar and shortcuts show it. Other targets need nothing here.
#![forbid(unsafe_code)]

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
