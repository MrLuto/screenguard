#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
mod app;
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    app::run()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Requires Windows");
}
