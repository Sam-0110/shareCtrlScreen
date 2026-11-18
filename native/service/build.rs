//! Link the VC runtime statically into sharectrl-service.exe (UCRT stays dynamic; it
//! ships with Windows 10+).
//!
//! WHY: a Rust MSVC binary imports VCRUNTIME140.dll by default, which a clean
//! Windows install does not have (it comes with the Visual C++ Redistributable).
//! The app gets static linking for free from `tauri_build::build()` (tauri-build
//! calls `static_vcruntime::metabuild()`), but this crate is not a Tauri crate, so
//! without this script the service fails to load on such machines with
//! STATUS_DLL_NOT_FOUND (0xC0000135, exit code -1073741515) — which the installer
//! reports as the "stop the running service before replacing files" step failing.
//!
//! `metabuild()` is a no-op on non-MSVC targets, so the workspace still builds
//! everywhere.
//!
//! Reference: https://docs.rs/static_vcruntime/3.0.0/static_vcruntime/

fn main() {
    static_vcruntime::metabuild();
}
