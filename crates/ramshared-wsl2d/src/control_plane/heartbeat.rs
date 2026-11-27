// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/heartbeat.rs
//! Verify the complete response before committing heartbeat authority.
use super::{ControlPlaneError, VsockControlPlane};
use crate::host_gate::LeaseToken;
use ipc::gpu_catalog::{AuthenticatedHeartbeat, CatalogTiming};
use ipc::vsock::VsockStreamTrait;
use ipc::{
    FrameError, HeartbeatAck, MSG_GPU_CATALOG, MSG_HEARTBEAT, MSG_HEARTBEAT_ACK, read_frame,
    write_frame,
};
use ramshared_ipc as ipc;
use std::sync::atomic::Ordering;
use std::time::Instant;

impl<S: VsockStreamTrait> VsockControlPlane<S> {
    pub(super) fn exchange_heartbeat(
        &mut self,
        payload: &[u8],
        now_ms: u64,
    ) -> Result<(), ControlPlaneError> {
        let started = Instant::now();
        let mut exchange = ipc::deadline::DeadlineIo::new(&mut self.stream, self.io_timeout)
            .map_err(|error| FrameError::StreamIo(error.to_string()))?;
        let round = self
            .catalog_session
            .as_ref()
            .map_or(4, |session| session.round());
        write_frame(&mut exchange, MSG_HEARTBEAT, payload, round)?;
        let (msg_type, ack_payload, correlation) =
            read_frame(&mut exchange, ipc::MAX_CONTROL_PAYLOAD)?;
        if msg_type != MSG_HEARTBEAT_ACK || (self.catalog_session.is_some() && correlation != round)
        {
            return Err(ControlPlaneError::Protocol(
                "expected current heartbeat ack".into(),
            ));
        }
        let ack: HeartbeatAck = ipc::decode_control(&ack_payload)?;
        let host_observations = if ack.lease_remaining_ms > 0 {
            if let Some(session) = self.catalog_session.as_mut() {
                let (kind, bytes, correlation) =
                    read_frame(&mut exchange, ipc::MAX_GPU_CATALOG_PAYLOAD)?;
                if kind != MSG_GPU_CATALOG {
                    return Err(ControlPlaneError::Protocol("expected GPU catalog".into()));
                }
                let authenticated = session.open_verified(
                    &self.secret,
                    &ack,
                    &bytes,
                    correlation,
                    CatalogTiming {
                        started,
                        received: Instant::now(),
                    },
                )?;
                Some(authenticated)
            } else {
                None
            }
        } else {
            None
        };
        exchange
            .finish()
            .map_err(|error| FrameError::StreamIo(error.to_string()))?;
        let lease_remaining_ms = ack.lease_remaining_ms;
        self.commit_heartbeat(ack, host_observations, now_ms)?;
        self.refresh_host_storage(lease_remaining_ms)
    }
    fn refresh_host_storage(&mut self, lease_remaining_ms: u64) -> Result<(), ControlPlaneError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = lease_remaining_ms;
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            if lease_remaining_ms == 0 {
                return Ok(());
            }
            let Some(observer) = self.root_vhd_observer else {
                return Ok(());
            };
            let Some(session) = self.storage_session.as_mut() else {
                return Ok(());
            };
            let Some(directory) = observer() else {
                self.revoke_host_storage();
                return Ok(());
            };
            let remaining = std::time::Duration::from_millis(lease_remaining_ms);
            let io_budget = self.io_timeout.min(remaining);
            let Some(lease_deadline) = Instant::now().checked_add(remaining) else {
                return Ok(());
            };
            let mut exchange = ipc::deadline::DeadlineIo::new(&mut self.stream, io_budget)
                .map_err(|error| FrameError::StreamIo(error.to_string()))?;
            let pending = ipc::storage_transport::send_request(
                &mut exchange,
                session,
                &self.secret,
                directory.identifier,
                lease_deadline,
            )?;
            let storage = ipc::storage_transport::receive_response_optional(
                &mut exchange,
                session,
                &self.secret,
                pending,
            )?;
            exchange
                .finish()
                .map_err(|error| FrameError::StreamIo(error.to_string()))?;
            match storage {
                Some(storage) => {
                    self.publish_host_storage(super::AuthenticatedHostStorage {
                        directory,
                        storage,
                    });
                }
                None => self.revoke_host_storage(),
            }
            Ok(())
        }
    }
    fn commit_heartbeat(
        &mut self,
        ack: HeartbeatAck,
        observations: Option<AuthenticatedHeartbeat>,
        now_ms: u64,
    ) -> Result<(), ControlPlaneError> {
        if ack.lease_remaining_ms == 0 {
            self.authority.on_lease_expired();
            self.publish_authority();
            self.cadence = None;
            self.lease_deadline_ms = 0;
            self.host_catalog = None;
            self.publish_host_catalog();
            self.publish_host_activity(None);
            #[cfg(target_os = "linux")]
            self.revoke_host_storage();
            return Err(ControlPlaneError::LeaseExpired);
        }
        self.lease_deadline_ms = now_ms.saturating_add(ack.lease_remaining_ms);
        if let Some(lease) = self.authority.lease().cloned() {
            let slid = LeaseToken {
                lease_id: lease.lease_id,
                deadline_ms: self.lease_deadline_ms,
            };
            let _ = self
                .authority
                .accept_lease(slid, self.origin.clone(), now_ms);
        }
        self.publish_authority();
        self.last_ack_ms.store(now_ms, Ordering::Relaxed);
        let (catalog, authenticated_memory, activity) = match observations {
            Some(observations) => (
                observations.catalog,
                observations.host_memory,
                observations.activity,
            ),
            None => (None, None, None),
        };
        if let Ok(mut host_memory) = self.host_memory.lock() {
            *host_memory = authenticated_memory;
        }
        self.host_catalog = catalog;
        self.publish_host_catalog();
        self.publish_host_activity(activity);
        Ok(())
    }
}
