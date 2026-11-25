// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/operation_tests.rs
//! Shared-budget tests use real silent Unix peers and no GPU allocations.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::BoundedCache;

#[test]
fn shared_read_deadline_does_not_multiply_silent_peer_waits() {
    let timeout = Duration::from_millis(30);
    let mut clients = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..4 {
        let (client, peer) = UnixStream::pair().unwrap();
        clients.push(IpcCacheClient::new(client, timeout, 4096));
        peers.push(peer);
    }
    let started = Instant::now();
    let deadline = started + timeout;
    for client in &mut clients {
        assert_ne!(client.read_until(0, &mut [0; 4], deadline), CacheRead::Hit);
    }
    assert!(
        started.elapsed() < Duration::from_millis(90),
        "shared read deadline became one full timeout per silent peer"
    );
    assert_eq!(
        clients[1].state(),
        CacheState::Active,
        "an expired shared budget revoked an uncontacted peer"
    );
    drop(peers);
}

#[test]
fn expired_read_and_refresh_start_no_frames_or_peer_revocation() {
    use std::io::Read;
    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client, Duration::from_millis(30), 4096);
    let expired = Instant::now();
    assert_eq!(client.read_until(0, &mut [0; 4], expired), CacheRead::Miss);
    assert!(client.refresh_until(expired).is_err());
    assert_eq!(client.state(), CacheState::Active);
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.read(&mut [0; 32]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn expired_release_is_unconfirmed_and_never_replays_a_control_frame() {
    use std::io::Read;
    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client, Duration::from_millis(30), 4096);
    assert_eq!(client.disable_until(Instant::now()), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Stuck);
    assert_eq!(client.disable(), CacheMutation::Failed);
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.read(&mut [0; 32]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn shared_refresh_deadline_preserves_uncontacted_peers() {
    let mut clients = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..4 {
        let (client, peer) = UnixStream::pair().unwrap();
        clients.push(IpcCacheClient::new(client, Duration::from_millis(30), 4096));
        peers.push(peer);
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(30);
    for client in &mut clients {
        assert!(client.refresh_until(deadline).is_err());
    }
    assert!(started.elapsed() < Duration::from_millis(90));
    assert_eq!(clients[1].state(), CacheState::Active);
    drop(peers);
}

#[test]
fn caller_cannot_extend_the_clients_local_read_timeout() {
    let (client, _peer) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client, Duration::from_millis(30), 4096);
    let started = Instant::now();
    assert_eq!(
        client.read_until(0, &mut [0; 4], started + Duration::from_secs(1)),
        CacheRead::Failed
    );
    assert!(started.elapsed() < Duration::from_millis(90));
}
