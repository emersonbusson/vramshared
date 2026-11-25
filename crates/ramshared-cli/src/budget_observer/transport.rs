// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/budget_observer/transport.rs
//! Bounded one-way local observation transport with kernel peer credentials.
use ramshared_ipc::budget_observation::{MAX_BYTES, Packet, SOCKET_NAME};
use std::fs::File;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;
const IO_TIME: Duration = Duration::from_millis(100);

pub(super) fn protected(path: &Path, uid: u32) -> Result<(), String> {
    for parent in path.ancestors() {
        let metadata = std::fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink()
            || ![0, uid].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err("observation image hierarchy is not protected".into());
        }
    }
    Ok(())
}
pub(super) fn probe_image() -> Result<File, String> {
    use crate::installed_image as image;
    let (_, selected) = image::cli_install_entries();
    probe_image_for(
        rustix::process::geteuid().as_raw(),
        image::current_process_install_origin(),
        &selected,
        Path::new(image::PRODUCT_ROOT),
        image::current_running_executable_sha256().as_deref(),
    )
}
fn probe_image_for(
    caller_uid: u32,
    origin: crate::installed_image::ProcessInstallOrigin,
    selected: &Path,
    product: &Path,
    running: Option<&str>,
) -> Result<File, String> {
    use crate::installed_image as image;
    if caller_uid != 0
        || !matches!(
            origin,
            image::ProcessInstallOrigin::ActiveRelease | image::ProcessInstallOrigin::Both
        )
    {
        return Err("Off budget observer requires the root selected installed supervisor".into());
    }
    probe_image_with(selected, product, 0, running)
}
fn probe_image_with(
    selected: &Path,
    product: &Path,
    uid: u32,
    running: Option<&str>,
) -> Result<File, String> {
    use crate::installed_image as image;
    let selected = selected.canonicalize().map_err(|e| e.to_string())?;
    if !selected.starts_with(product) {
        return Err("observer release escapes the protected product root".into());
    }
    protected(&selected, uid)?;
    let root = selected
        .parent()
        .and_then(Path::parent)
        .ok_or("observer release root absent")?;
    let manifest =
        image::verify_release_manifest(root).ok_or("observer release manifest unavailable")?;
    if manifest.get("bin/ramshared").map(String::as_str) != running {
        return Err("observer image differs from selected release".into());
    }
    let daemon = root.join("bin/ramsharedd");
    protected(&daemon, uid)?;
    let fd = rustix::fs::openat2(
        rustix::fs::CWD,
        &daemon,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| e.to_string())?;
    let mut file = File::from(fd);
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err("native probe inode is unsealed".into());
    }
    if manifest.get("bin/ramsharedd") != image::sha256_reader(&mut file).as_ref() {
        return Err("probe image differs from selected release".into());
    }
    Ok(file)
}
pub(super) fn bind(name: &[u8]) -> Result<UnixListener, String> {
    let address = SocketAddr::from_abstract_name(name).map_err(|e| e.to_string())?;
    let listener = UnixListener::bind_addr(&address).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(listener)
}
pub(super) fn send(mut stream: UnixStream, packet: &Packet) -> Result<(), String> {
    let bytes = serde_json::to_vec(packet).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("observer response exceeds byte bound".into());
    }
    stream
        .set_write_timeout(Some(IO_TIME))
        .map_err(|e| e.to_string())?;
    stream.write_all(&bytes).map_err(|e| e.to_string())
}
fn receive(name: &[u8], uid: u32, selected: &Path, now: u64) -> Result<Packet, String> {
    let started = std::time::Instant::now();
    let deadline = started + IO_TIME;
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|e| e.to_string())?;
    let address = SocketAddrUnix::new_abstract_name(name).map_err(|e| e.to_string())?;
    // Nonblocking Unix connect refuses a full backlog instead of waiting for an
    // untrusted endpoint. The next normal readiness sample may try again.
    rustix::net::connect(&socket, &address).map_err(|e| e.to_string())?;
    let mut stream = UnixStream::from(socket);
    let peer = rustix::net::sockopt::socket_peercred(&stream).map_err(|e| e.to_string())?;
    if peer.uid.as_raw() != uid {
        return Err("budget observer peer is not the native authority".into());
    }
    let selected = selected.canonicalize().map_err(|e| e.to_string())?;
    protected(&selected, uid)?;
    let expected = std::fs::metadata(&selected).map_err(|e| e.to_string())?;
    let executable = format!("/proc/{}/exe", peer.pid.as_raw_nonzero());
    let pid = peer.pid.as_raw_nonzero().get() as u32;
    let generation = crate::workload::process_start_time(pid)
        .ok_or("observer process incarnation is unavailable")?;
    let held = File::open(&executable).map_err(|e| e.to_string())?;
    let metadata = held.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.dev() != expected.dev()
        || metadata.ino() != expected.ino()
    {
        return Err("observer peer is not the selected installed image".into());
    }
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("observer response deadline expired".into());
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_BYTES {
            return Err("observer response exceeds byte bound".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let after = std::fs::metadata(&executable).map_err(|e| e.to_string())?;
    if crate::workload::process_start_time(pid) != Some(generation)
        || after.dev() != metadata.dev()
        || after.ino() != metadata.ino()
    {
        return Err("observer process or executable changed during response".into());
    }
    let elapsed_ms = u64::try_from(started.elapsed().as_nanos().div_ceil(1_000_000))
        .map_err(|e| e.to_string())?;
    let now = now
        .checked_add(elapsed_ms)
        .ok_or("observer response age overflows")?;
    Packet::decode(&bytes, now)?.include_transit(elapsed_ms)
}
pub(super) fn read(now: u64) -> Option<Packet> {
    let (_, path) = crate::installed_image::cli_install_entries();
    receive(SOCKET_NAME, 0, &path, now).ok()
}

#[cfg(test)]
pub(super) fn read_fixture(
    name: &[u8],
    uid: u32,
    selected: &Path,
    now: u64,
) -> Result<Packet, String> {
    receive(name, uid, selected, now)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn endpoint(
        label: &str,
        write: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (Vec<u8>, std::thread::JoinHandle<()>) {
        let name = format!("ramshared-observer-test-{label}-{}", std::process::id()).into_bytes();
        let listener = bind(&name).unwrap();
        let thread = std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        write(stream);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        (name, thread)
    }
    #[test]
    fn observer_peer_credentials_image_and_response_bounds_fail_closed() {
        if super::super::tests::protected_process(
            "budget_observer::transport::tests::observer_peer_credentials_image_and_response_bounds_fail_closed",
        ) {
            return;
        }
        let now = super::super::unix_ms().unwrap();
        let executable = std::env::current_exe().unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let (name, thread) = endpoint("uid", |_| {});
        assert!(receive(&name, uid.wrapping_add(1), &executable, now).is_err());
        thread.join().unwrap();
        let (name, thread) = endpoint("image", |_| {});
        assert!(receive(&name, uid, Path::new("/bin/sh"), now).is_err());
        thread.join().unwrap();
        let (name, thread) = endpoint("malformed", |mut s| {
            let _ = s.write_all(b"invalid");
        });
        assert!(receive(&name, uid, &executable, now).is_err());
        thread.join().unwrap();
        let (name, thread) = endpoint("oversized", |mut s| {
            let _ = s.write_all(&vec![b'x'; MAX_BYTES + 1]);
        });
        assert!(receive(&name, uid, &executable, now).is_err());
        thread.join().unwrap();
        let (name, thread) = endpoint("drip", |mut s| {
            for _ in 0..6 {
                if s.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let start = std::time::Instant::now();
        assert!(receive(&name, uid, &executable, now).is_err());
        assert!(start.elapsed() < Duration::from_millis(250));
        thread.join().unwrap();
        let (a, _b) = UnixStream::pair().unwrap();
        let huge = Packet {
            report: None,
            age_ms: 0,
        };
        assert!(send(a, &huge).is_ok());
    }
    #[test]
    fn native_probe_image_is_verified_on_its_held_descriptor() {
        let fixture = super::super::tests::Fixture::new();
        let root = fixture.0.join("release");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let paths = ["bin/ramshared", "bin/ramsharedd"].into_iter().chain(
            crate::release_identity::SEALED_IDENTITY_FILES
                .iter()
                .copied(),
        );
        let mut manifest = String::new();
        for path in paths {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"sealed fixture").unwrap();
            manifest.push_str(&format!(
                "{}  ./{path}\n",
                crate::installed_image::sha256_file(&file).unwrap()
            ));
        }
        std::fs::write(root.join("SHA256SUMS"), manifest).unwrap();
        let digest = crate::installed_image::sha256_file(&root.join("SHA256SUMS")).unwrap();
        std::fs::write(root.join("INSTALLED_MANIFEST_SHA256"), digest).unwrap();
        let selected = root.join("bin/ramshared");
        let running = crate::installed_image::sha256_file(&selected).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        assert!(probe_image_with(&selected, &fixture.0, uid, Some(&running)).is_ok());
        assert!(probe_image_with(&selected, Path::new("/foreign"), uid, Some(&running)).is_err());
        assert!(probe_image_with(&selected, &fixture.0, uid, None).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(probe_image_with(&selected, &fixture.0, uid, Some(&running)).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("bin/ramsharedd"), b"changed").unwrap();
        assert!(probe_image_with(&selected, &fixture.0, uid, Some(&running)).is_err());
        if uid != 0 {
            assert!(probe_image().is_err());
        }
        assert!(
            probe_image_for(
                0,
                crate::installed_image::ProcessInstallOrigin::Direct,
                &selected,
                &fixture.0,
                Some(&running),
            )
            .is_err()
        );
        assert!(
            probe_image_for(
                0,
                crate::installed_image::ProcessInstallOrigin::ActiveRelease,
                &selected,
                Path::new("/foreign"),
                Some(&running),
            )
            .is_err()
        );
        let (slow_name, slow_thread) = endpoint("slow-drip", |mut stream| {
            for _ in 0..5 {
                if stream.write_all(b"{").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(35));
            }
        });
        assert!(
            receive(
                &slow_name,
                uid,
                &std::env::current_exe().unwrap(),
                1_700_000_000_000
            )
            .is_err()
        );
        slow_thread.join().unwrap();
        assert!(read(1_700_000_000_000).is_none());
        std::fs::set_permissions(
            root.join("bin/ramsharedd"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(probe_image_with(&selected, &fixture.0, uid, Some(&running)).is_err());
        let (closed_tx, closed_rx) = UnixStream::pair().unwrap();
        drop(closed_rx);
        assert!(
            send(
                closed_tx,
                &Packet {
                    report: None,
                    age_ms: 0
                }
            )
            .is_err()
        );
        assert!(bind(&[b'a'; 120]).is_err());
        let (a, _b) = UnixStream::pair().unwrap();
        let oversized = Packet {
            report: Some(ramshared_ipc::budget_observation::Report {
                schema_version: 1,
                purpose: ramshared_ipc::budget_observation::Purpose::NativeLinuxPlanningObservation,
                pid: 1,
                started_unix_ms: 1,
                elapsed_ms: 0,
                budgets: Vec::new(),
                unavailable_reason: Some("x".repeat(MAX_BYTES + 128)),
            }),
            age_ms: 0,
        };
        assert!(send(a, &oversized).is_err());
    }
}
