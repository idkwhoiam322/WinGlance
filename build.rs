//! Embeds the application manifest into WinGlance.exe.
//!
//! The manifest's load-bearing entry is the Microsoft.Windows.Common-Controls
//! version 6.0.0.0 side-by-side dependency (`new_manifest` includes it by
//! default): without it Windows loads comctl32 5.82, whose tooltip control
//! silently drops every history-row tool (see main_window.rs). Everything
//! else stays at the crate defaults that match this app: AsInvoker execution
//! and no manifest DPI setting, so the runtime
//! `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` call in main.rs
//! remains the single owner of DPI awareness.

use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("WinGlance")).expect("unable to embed the application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
