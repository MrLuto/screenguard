#[cfg(windows)]
mod config;
#[cfg(windows)]
mod filter;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod update;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => service::dispatch(),
        Some("--console") => service::console(),
        Some("--version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--reset") => {
            native::require_admin()?;
            native::ensure_service_stopped()?;
            agent_core::db::Db::open(Some(config::db_path()?.to_str().unwrap()))?.reset_pairing()
        }
        _ => {
            println!(
                "ScreenGuard Windows agent {}\nUse the installer, or --console, --service, --reset, --version.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("This binary requires Windows 11.");
    std::process::exit(1);
}
