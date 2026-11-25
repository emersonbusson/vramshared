// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/watchdog/wsl_scm_watchdog/native.rs
//! Read-only broker status observation shared by SCM composition and native tests.
use super::WatchdogTransport;
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const BROKER_SERVICE_NAME: &str = "RamSharedBroker";
pub struct ScmBrokerProbe;
impl WatchdogTransport for ScmBrokerProbe {
    fn probe(&mut self) -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|error| error.to_string())?;
        let broker = manager
            .open_service(BROKER_SERVICE_NAME, ServiceAccess::QUERY_STATUS)
            .map_err(|error| error.to_string())?;
        let status = broker.query_status().map_err(|error| error.to_string())?;
        if status.current_state == ServiceState::Running {
            Ok(())
        } else {
            Err(format!("broker state {:?}", status.current_state))
        }
    }
}
