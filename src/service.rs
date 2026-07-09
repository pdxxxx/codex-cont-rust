#[cfg(windows)]
mod imp {
    use std::{
        ffi::OsString,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use windows_service::{
        define_windows_service,
        service::{
            Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
            ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    const SERVICE_NAME: &str = "CodexCont";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> Result<(), String> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| format!("failed to start service dispatcher: {e}"))
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(err) = run_service() {
            eprintln!("service error: {err}");
        }
    }

    fn run_service() -> Result<(), String> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
        let handler_tx = shutdown_tx.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop => {
                    if let Some(tx) = handler_tx.lock().ok().and_then(|mut tx| tx.take()) {
                        let _ = tx.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .map_err(|e| format!("failed to register service control handler: {e}"))?;
        status_handle
            .set_service_status(status(
                ServiceState::StartPending,
                ServiceControlAccept::empty(),
            ))
            .map_err(|e| format!("failed to report service status: {e}"))?;

        let result = crate::block_on_server(
            async move {
                let _ = shutdown_rx.await;
            },
            || {
                status_handle
                    .set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP))
                    .map_err(|e| format!("failed to report service status: {e}"))
            },
        );
        status_handle
            .set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))
            .map_err(|e| format!("failed to report stopped service status: {e}"))?;
        result
    }

    pub fn install() -> Result<(), String> {
        let manager =
            manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let service_info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_NAME),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()
                .map_err(|e| format!("failed to locate current executable: {e}"))?,
            launch_arguments: vec![OsString::from("service")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let service = manager
            .create_service(
                &service_info,
                ServiceAccess::QUERY_STATUS | ServiceAccess::START,
            )
            .map_err(|e| format!("failed to create {SERVICE_NAME} service: {e}"))?;
        service
            .start::<&str>(&[])
            .map_err(|e| format!("failed to start {SERVICE_NAME} service: {e}"))?;
        wait_for_state(&service, ServiceState::Running)
    }

    pub fn uninstall() -> Result<(), String> {
        let service =
            open(ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
        stop_if_needed(&service)?;
        service
            .delete()
            .map_err(|e| format!("failed to delete {SERVICE_NAME} service: {e}"))
    }

    pub fn start() -> Result<(), String> {
        let service = open(ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
        match service
            .query_status()
            .map_err(|e| e.to_string())?
            .current_state
        {
            ServiceState::Running => return Ok(()),
            ServiceState::StartPending => return wait_for_state(&service, ServiceState::Running),
            ServiceState::StopPending => wait_for_state(&service, ServiceState::Stopped)?,
            _ => {}
        }
        service
            .start::<&str>(&[])
            .map_err(|e| format!("failed to start {SERVICE_NAME} service: {e}"))?;
        wait_for_state(&service, ServiceState::Running)
    }

    pub fn stop() -> Result<(), String> {
        let service = open(ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)?;
        stop_if_needed(&service)
    }

    pub fn restart() -> Result<(), String> {
        stop()?;
        start()
    }

    fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
        ServiceManager::local_computer(None::<&str>, access).map_err(|e| e.to_string())
    }

    fn open(access: ServiceAccess) -> Result<Service, String> {
        manager(ServiceManagerAccess::CONNECT)?
            .open_service(SERVICE_NAME, access)
            .map_err(|e| format!("failed to open {SERVICE_NAME} service: {e}"))
    }

    fn stop_if_needed(service: &Service) -> Result<(), String> {
        match service
            .query_status()
            .map_err(|e| e.to_string())?
            .current_state
        {
            ServiceState::Stopped => return Ok(()),
            ServiceState::StopPending => return wait_for_state(service, ServiceState::Stopped),
            _ => {}
        }
        service
            .stop()
            .map_err(|e| format!("failed to stop {SERVICE_NAME} service: {e}"))?;
        wait_for_state(service, ServiceState::Stopped)
    }

    fn wait_for_state(service: &Service, target: ServiceState) -> Result<(), String> {
        for _ in 0..50 {
            let state = service
                .query_status()
                .map_err(|e| format!("failed to query {SERVICE_NAME} service: {e}"))?
                .current_state;
            if state == target {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(format!("{SERVICE_NAME} service did not reach {target:?}"))
    }

    fn status(
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
    ) -> ServiceStatus {
        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: match current_state {
                ServiceState::StartPending | ServiceState::StopPending => Duration::from_secs(10),
                _ => Duration::default(),
            },
            process_id: None,
        }
    }
}

#[cfg(windows)]
pub use imp::{install, restart, run, start, stop, uninstall};

#[cfg(not(windows))]
fn unsupported() -> Result<(), String> {
    Err("Windows service commands are only supported on Windows.".to_string())
}

#[cfg(not(windows))]
pub fn run() -> Result<(), String> {
    unsupported()
}

#[cfg(not(windows))]
pub fn install() -> Result<(), String> {
    unsupported()
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<(), String> {
    unsupported()
}

#[cfg(not(windows))]
pub fn start() -> Result<(), String> {
    unsupported()
}

#[cfg(not(windows))]
pub fn stop() -> Result<(), String> {
    unsupported()
}

#[cfg(not(windows))]
pub fn restart() -> Result<(), String> {
    unsupported()
}
