// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/heartbeat_loop/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use crate::host_gate::{ControlPlaneState, GateError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
struct Plane {
    calls: u32,
    stop: Arc<AtomicBool>,
}
impl ControlPlane for Plane {
    fn state(&self) -> ControlPlaneState {
        ControlPlaneState::Disconnected
    }
    fn admit_cache(&self, _: u64) -> Result<(), GateError> {
        Ok(())
    }
    fn admit_origin_io(&self) -> Result<(), GateError> {
        Ok(())
    }
    fn heartbeat(&mut self, _: u64) -> Result<(), ControlPlaneError> {
        self.calls += 1;
        self.stop.store(true, Ordering::Release);
        Ok(())
    }
}
#[test]
fn heartbeat_cancellation_before_first_effect_and_after_beat() {
    let stop = Arc::new(AtomicBool::new(true));
    let mut plane = Plane {
        calls: 0,
        stop: stop.clone(),
    };
    let policy = HeartbeatLoop::new(10, 3).unwrap();
    assert_eq!(
        policy.run_until(
            &mut plane,
            10,
            || 0,
            |_| panic!("no sleep"),
            || stop.load(Ordering::Acquire)
        ),
        HeartbeatLoopStop::Cancelled { ticks: 0 }
    );
    assert_eq!(plane.calls, 0);
    stop.store(false, Ordering::Release);
    assert_eq!(
        policy.run_until(
            &mut plane,
            10,
            || 0,
            |_| panic!("no sleep after cancellation"),
            || stop.load(Ordering::Acquire)
        ),
        HeartbeatLoopStop::Cancelled { ticks: 1 }
    );
    assert_eq!(plane.calls, 1);
}
