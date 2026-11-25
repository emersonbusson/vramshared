// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease/listener.rs
//! Configured control-plane construction; returned custody includes native samplers.
#[cfg(not(windows))]
use super::VsockError;
use super::{ControlPlaneTask, LeasePolicy, LeaseServerError, VsockConfig};
use std::path::Path;
#[cfg(windows)]
mod native;

/// Absent configuration returns no task. Configured failures preserve storage
/// availability; callers report that cache control authority is unavailable.
pub fn spawn_configured_listener(
    config_path: Option<&Path>,
    product_executable: &Path,
) -> Result<Option<ControlPlaneTask>, LeaseServerError> {
    let Some(path) = config_path else {
        return Ok(None);
    };
    let Some(config) = VsockConfig::load(path)? else {
        return Ok(None);
    };
    let key = config.read_key()?;
    let policy = LeasePolicy::new(config.heartbeat_secs, config.lease_timeout_secs)?;
    if !product_executable.is_absolute() || !product_executable.is_file() {
        return Err(LeaseServerError::Config(
            "control plane requires the verified absolute product executable".into(),
        ));
    }
    #[cfg(windows)]
    {
        native::spawn(
            config,
            key,
            policy,
            path.with_file_name("vsock.log"),
            product_executable.to_path_buf(),
        )
        .map(Some)
    }
    #[cfg(not(windows))]
    {
        let _ = (config, key, policy);
        Err(LeaseServerError::Vsock(VsockError::Unsupported))
    }
}
