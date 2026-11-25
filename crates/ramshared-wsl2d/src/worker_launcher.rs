// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_launcher.rs
//! Native child launch, socket handshake and exact failure ownership transfer.
use std::io;

use super::worker_custody::Reservation;
use super::worker_startup::{InitialStartResult, StartOutcome, Startup};
use super::{
    ChildWorkerProcess, IpcCacheClient, IsolatedWorkerSupervisor, WorkerChildHandle,
    WorkerSpawnParams, gpu_worker_request, worker_registry,
};

#[cfg(test)]
pub(super) fn begin(params: &WorkerSpawnParams) -> io::Result<Startup> {
    let owned = params.clone();
    worker_registry::registry().begin(
        params.expected_adapter.as_ref(),
        Box::new(move |reservation| launch_reserved(&owned, reservation)),
    )
}

pub(super) fn begin_guarded(
    params: &WorkerSpawnParams,
    authority: super::worker_startup::StartupAuthority,
) -> io::Result<Startup> {
    let owned = params.clone();
    worker_registry::registry().begin_guarded(
        params.expected_adapter.as_ref(),
        Box::new(move |reservation| launch_reserved(&owned, reservation)),
        authority,
    )
}

pub(super) fn spawn(params: &WorkerSpawnParams) -> InitialStartResult {
    launch(params).into_initial_result()
}

pub(super) fn launch(params: &WorkerSpawnParams) -> StartOutcome {
    match worker_registry::registry().reserve(params.expected_adapter.as_ref()) {
        Ok(reservation) => launch_reserved(params, reservation),
        Err(error) => StartOutcome {
            result: Err(error.to_string()),
            creator_child: None,
        },
    }
}

fn launch_reserved(params: &WorkerSpawnParams, reservation: Reservation) -> StartOutcome {
    launch_reserved_with_executable(params, reservation, None)
}

#[cfg(test)]
pub(super) fn begin_candidate(
    params: &WorkerSpawnParams,
    executable: std::path::PathBuf,
) -> io::Result<Startup> {
    let owned = params.clone();
    worker_registry::registry().begin(
        params.expected_adapter.as_ref(),
        Box::new(move |reservation| {
            launch_reserved_with_executable(&owned, reservation, Some(&executable))
        }),
    )
}

fn launch_reserved_with_executable(
    params: &WorkerSpawnParams,
    reservation: Reservation,
    executable: Option<&std::path::Path>,
) -> StartOutcome {
    let mut creator_child = None;
    let result = spawn_retained(params, reservation, &mut creator_child, executable)
        .map_err(|error| error.to_string());
    StartOutcome {
        result,
        creator_child,
    }
}

fn spawn_retained(
    params: &WorkerSpawnParams,
    reservation: Reservation,
    creator_child: &mut Option<ChildWorkerProcess>,
    executable: Option<&std::path::Path>,
) -> Result<(IpcCacheClient, IsolatedWorkerSupervisor), Box<dyn std::error::Error>> {
    let (client_sock, worker_sock) = std::os::unix::net::UnixStream::pair()?;
    let arguments = params.arguments(std::os::fd::AsRawFd::as_raw_fd(&std::io::stdin()))?;
    let executable = executable
        .map(std::path::Path::to_path_buf)
        .map_or_else(std::env::current_exe, Ok)?;
    let mut command = std::process::Command::new(executable);
    command.arg("__gpu_worker").args(arguments);
    if let Some(session) = &params.generation {
        super::native_cache::worker::inherit(&mut command, session)?;
    }
    let _catalog_guard = match &params.host_catalog {
        Some(catalog) => Some(gpu_worker_request::inherit_catalog_stream(
            &mut command,
            catalog,
        )?),
        None => None,
    };
    let child = gpu_worker_request::spawn_socket_child(&mut command, worker_sock)?;
    // Command retains a parent socket alias; close it before waiting for EOF.
    drop(command);

    let process = ChildWorkerProcess::from_resources(reservation.bind(child));
    *creator_child = Some(process.clone());
    let mut supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(process)),
    };
    let mut client = IpcCacheClient::new(
        client_sock,
        ramshared_block::DEFAULT_READ_TIMEOUT,
        params.target_bytes,
    );
    if let Some(expected) = &params.expected_adapter {
        client.expect_adapter(expected.clone())?;
    }
    if let Err(error) = client
        .perform_handshake()
        .and_then(|()| gpu_worker_request::complete_startup_binding(&mut client))
    {
        drop(client);
        supervisor.shutdown();
        return Err(format!("worker handshake failed: {error}").into());
    }
    Ok((client, supervisor))
}

#[cfg(test)]
mod tests;
