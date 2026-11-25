// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/lease_supervisor/session.rs
//! One owned lease session; no replacement while a campaign/refresher remains.
use super::{CampaignHandle, LeaseLog, SessionAuthority, SessionHostMemory, sleep_unless_stopped};
use ramshared_ipc::gpu_catalog::AuthenticatedHostMemory;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

pub(super) struct SessionRun {
    session: SessionAuthority,
    lost: Arc<AtomicBool>,
    memory: Arc<Mutex<Option<SessionHostMemory>>>,
    stop: Arc<AtomicBool>,
    refresher: Option<JoinHandle<()>>,
    campaign: Option<Box<dyn CampaignHandle>>,
    log: LeaseLog,
    join_ceiling: Duration,
    complete: bool,
}
impl SessionRun {
    pub(super) fn new(
        session: SessionAuthority,
        lost: Arc<AtomicBool>,
        memory: Arc<Mutex<Option<SessionHostMemory>>>,
        log: LeaseLog,
        join_ceiling: Duration,
    ) -> Self {
        Self {
            session,
            lost,
            memory,
            stop: Arc::new(AtomicBool::new(false)),
            refresher: None,
            campaign: None,
            log,
            join_ceiling,
            complete: false,
        }
    }
    pub(super) fn attach(&mut self, campaign: Box<dyn CampaignHandle>) {
        self.campaign = Some(campaign);
    }
    pub(super) fn start_refresher(
        &mut self,
        source: Arc<Mutex<Option<AuthenticatedHostMemory>>>,
    ) -> Result<(), String> {
        let target = self.memory.clone();
        let session = self.session.clone();
        let stop = self.stop.clone();
        self.refresher = Some(
            std::thread::Builder::new()
                .name("ramshared-lease-memory".into())
                .spawn(move || {
                    while session.is_current() && !stop.load(Ordering::Acquire) {
                        sleep_unless_stopped(Duration::from_millis(500), &stop);
                        if stop.load(Ordering::Acquire) || !session.is_current() {
                            break;
                        }
                        let value = source.lock().ok().and_then(|slot| slot.clone());
                        let published = session.mutate(|_| {
                            if !stop.load(Ordering::Acquire)
                                && let Ok(mut slot) = target.lock()
                            {
                                *slot = value
                                    .map(|sample| SessionHostMemory::new(session.clone(), sample));
                            }
                        });
                        if !published {
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }
    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.session.retire(|slot| {
            slot.on_lease_expired();
            slot.enter_origin_only();
            self.lost.store(true, Ordering::Release);
            if let Ok(mut memory) = self.memory.lock() {
                *memory = None;
            }
        });
        if let Some(campaign) = &self.campaign {
            campaign.request_stop();
        }
    }
    pub(super) fn finish(&mut self) {
        if self.complete {
            return;
        }
        self.request_stop();
        if let Some(campaign) = self.campaign.as_mut() {
            let mut reported = false;
            while !campaign.join_within(self.join_ceiling) {
                if !reported {
                    self.log.emit("vsock session shutdown pending; retaining the same campaign before replacement");
                    reported = true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.campaign.take();
        if let Some(handle) = self.refresher.take() {
            let _ = handle.join();
        }
        self.complete = true;
    }
}
impl Drop for SessionRun {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests;
