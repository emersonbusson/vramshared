// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/tier_release/cleanup_retry_tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::release_tests::{HEADER, LIMITED, ROOMY, ReleaseFixture};
use super::*;

fn blocked<'a>(slot: &'a Mutex<CustodyState>, io: &mut ReleaseFixture) -> ReleaseFlight<'a> {
    io.frame(LIMITED, true, None);
    io.frame(LIMITED, true, Some(0));
    io.swaps(false, Some(1048576));
    io.frame(LIMITED, false, Some(1048576));
    assert!(!io.release());
    let mut flight = begin_release_in(slot, "/dev/nbd0").unwrap();
    flight.lease.custody.absorber = io.retained.take();
    flight.lease.custody.cleanup_waits_for_memory = io.cleanup_waits_for_memory;
    assert!(flight.lease.custody.cleanup_waits_for_memory);
    record_release_outcome(&mut flight, ReleaseOutcome::Pending);
    flight
}

fn resume(
    flight: &mut ReleaseFlight<'_>,
    io: &mut ReleaseFixture,
    budget: Duration,
) -> ReleaseOutcome {
    let io = std::cell::RefCell::new(io);
    resume_owned_cleanup_with(
        flight,
        budget,
        &mut |path| io.borrow_mut().read_proc(path),
        |backing, deadline, _| remove_emergency_swap(backing, deadline, *io.borrow_mut()),
    )
}

fn active_observations(io: &mut ReleaseFixture) {
    io.swaps(false, Some(1048576));
    io.swaps(false, Some(1048576));
}

#[test]
fn retained_cleanup_retries_only_after_capacity_recovers() {
    let mut io = ReleaseFixture::new();
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = blocked(&slot, &mut io);
    let descriptor = flight
        .lease
        .custody
        .absorber
        .as_ref()
        .unwrap()
        .effect_path();
    active_observations(&mut io);
    io.frame(LIMITED, false, Some(1048576));
    assert_eq!(
        resume(&mut flight, &mut io, Duration::from_secs(2)),
        ReleaseOutcome::Pending
    );
    assert_eq!(io.commands.len(), 4, "unchanged shortage starts no cleanup");
    assert_eq!(
        flight
            .lease
            .custody
            .absorber
            .as_ref()
            .unwrap()
            .effect_path(),
        descriptor
    );
    active_observations(&mut io);
    io.frame(ROOMY, false, Some(1048576));
    io.swaps(false, None);
    io.swaps(false, None);
    assert_eq!(
        resume(&mut flight, &mut io, Duration::from_secs(2)),
        ReleaseOutcome::Released
    );
    assert_eq!(
        io.commands,
        ["fallocate", "mkswap", "swapon", "swapoff", "swapoff"]
    );
    assert!(!io.backing_path().exists());
    io.swaps(false, None);
    assert_eq!(
        resume(&mut flight, &mut io, Duration::from_secs(2)),
        ReleaseOutcome::Released
    );
    assert_eq!(io.commands.len(), 5);
    assert!(
        begin_release_in(&slot, "/dev/nbd1").is_none(),
        "same flight still owns admission"
    );
    drop(flight);
    assert!(custody_is_clear(&slot));
}

#[test]
fn retained_cleanup_never_duplicates_a_running_helper() {
    let mut io = ReleaseFixture::new();
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = blocked(&slot, &mut io);
    assert_eq!(
        resume(&mut flight, &mut io, Duration::ZERO),
        ReleaseOutcome::Pending
    );
    assert!(flight.lease.custody.cleanup_waits_for_memory);
    flight.lease.custody.child = Some(Command::new("/usr/bin/sleep").arg("0.5").spawn().unwrap());
    assert_eq!(
        resume(&mut flight, &mut io, Duration::from_secs(1)),
        ReleaseOutcome::Pending
    );
    assert_eq!(io.commands.len(), 4);
    assert_eq!(
        observe_helper_until(
            flight.lease.custody.child.as_mut().unwrap(),
            Instant::now() + Duration::from_secs(2)
        ),
        HelperOutcome::Success
    );
    flight.lease.custody.child = None;
}

#[test]
fn retained_cleanup_refuses_unknown_identity_and_permanent_failures() {
    for case in ["memory", "identity", "capacity", "helper"] {
        let mut io = ReleaseFixture::new();
        let slot = Mutex::new(CustodyState::Idle);
        let mut flight = blocked(&slot, &mut io);
        active_observations(&mut io);
        match case {
            "identity" => {
                std::fs::rename(io.backing_path(), io.root.join("original")).unwrap();
                std::fs::write(io.backing_path(), b"foreign").unwrap();
            }
            "memory" => io.reads.push_back(("/proc/meminfo", None)),
            _ => {
                io.frame(ROOMY, false, Some(1048576));
                if case == "capacity" {
                    io.reads.back_mut().unwrap().1 = Some(format!(
                        "{HEADER}{} file 3145728 1048576 0\n",
                        io.backing_path().display()
                    ));
                } else {
                    io.outcomes.push_back(HelperOutcome::Failed);
                }
            }
        }
        assert_eq!(
            resume(&mut flight, &mut io, Duration::from_secs(2)),
            ReleaseOutcome::Pending,
            "{case}"
        );
        assert!(
            !flight.lease.custody.cleanup_waits_for_memory,
            "{case} is not a retryable headroom shortage"
        );
        let calls = io.commands.len();
        io.swaps(false, Some(1048576));
        assert_eq!(
            resume(&mut flight, &mut io, Duration::from_secs(2)),
            ReleaseOutcome::Pending
        );
        assert_eq!(
            io.commands.len(),
            calls,
            "{case} cannot start another helper"
        );
        assert!(io.backing_path().exists());
    }
}
