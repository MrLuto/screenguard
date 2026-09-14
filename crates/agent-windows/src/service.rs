use anyhow::Result;
use std::{ffi::OsString, time::Duration};
use tokio::sync::watch;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

define_windows_service!(ffi_main, service_main);
pub fn dispatch() -> Result<()> {
    service_dispatcher::start("ScreenGuard", ffi_main)?;
    Ok(())
}
fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}
fn service_main(_: Vec<OsString>) {
    let (tx, rx) = watch::channel(false);
    let Ok(handle) =
        service_control_handler::register("ScreenGuard", move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })
    else {
        return;
    };
    let _ = handle.set_service_status(status(ServiceState::StartPending));
    let result = run(rx, || {
        handle.set_service_status(status(ServiceState::Running))?;
        Ok(())
    });
    let mut stopped = status(ServiceState::Stopped);
    if let Err(e) = result {
        tracing::error!("Service stopped: {e:#}");
        stopped.exit_code = ServiceExitCode::ServiceSpecific(1);
    }
    let _ = handle.set_service_status(stopped);
}
fn run(rx: watch::Receiver<bool>, ready: impl FnOnce() -> Result<()>) -> Result<()> {
    crate::native::require_admin()?;
    let logs = crate::config::data_dir()?.join("logs");
    std::fs::create_dir_all(&logs)?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("agent")
        .filename_suffix("log")
        .max_log_files(7)
        .build(logs)?;
    let (writer, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .with_env_filter("info")
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    ready()?;
    rt.block_on(crate::runtime::run(rx))
}
pub fn console() -> Result<()> {
    let (tx, rx) = watch::channel(false);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::spawn(move || {
        rt.block_on(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(true);
        })
    });
    run(rx, || Ok(()))
}
