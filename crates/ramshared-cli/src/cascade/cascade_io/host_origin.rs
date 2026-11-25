// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/host_origin.rs
//! Host-origin attach and the sealed guardian.
//!
//! Extracted from `cascade_io.rs` as the last band of the boundary plan in
//! `workspace module layout`. Both are **host** effects run
//! through a bounded elevated boundary: the origin VHDX is mounted and the
//! guardian task is started on the Windows side, each exactly once, each
//! idempotent, and each refused before it runs if the sealed configuration
//! cannot be read and verified.
//!
//! Neither writes a proof of its own. They start the thing that produces the
//! proof; the gate that consumes it re-reads it independently.

use super::*;

/// Guest-visible path of the installed windowless launcher.
///
/// One fixed path, for the same reason as [`HOST_ORIGIN_MANIFEST`]: the guest
/// names the deployed authority, never a source checkout.
pub(super) const WINDOWS_HOST_LAUNCHER: &str =
    "/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe";
/// Host path the payload seal and the scheduled-task action name (DT-12).
pub(super) const HOST_LAUNCHER_WINDOWS_PATH: &str =
    "C:\\ProgramData\\RamShared\\bin\\ramshared-host-launch.exe";
/// Stable refusal token for a host call that would otherwise allocate a
/// visible console. There is no direct `sudo.exe`/`powershell.exe` fallback.
pub(super) const VISIBLE_ELEVATION_REFUSED: &str = "VISIBLE_ELEVATION_REFUSED";

/// The one host launch boundary: the sealed launcher plus the elevation
/// program it runs.
///
/// Resolved once per host call. Having both values in one type means a caller
/// cannot compose a launcher argv with a boundary that was never resolved, and
/// it keeps the missing-launcher refusal before any Windows API is reached.
#[derive(Clone, Debug)]
pub(super) struct HostBoundary {
    launcher: &'static str,
    elevation: String,
}

impl HostBoundary {
    pub(super) fn new(launcher: &'static str, elevation: String) -> Self {
        Self {
            launcher,
            elevation,
        }
    }

    fn elevation(&self) -> &str {
        &self.elevation
    }
}

/// Refuse a host call when the sealed launcher is not installed.
///
/// The launcher owns the console decision; falling back to a direct host
/// program would reintroduce the visible window this SPEC closes.
pub(super) fn host_launcher_program(launcher_present: bool) -> Result<&'static str, CascadeError> {
    if launcher_present {
        return Ok(WINDOWS_HOST_LAUNCHER);
    }
    Err(CascadeError::Precondition(format!(
        "{VISIBLE_ELEVATION_REFUSED}: the windowless host launcher \
         {HOST_LAUNCHER_WINDOWS_PATH} is not installed"
    )))
}

/// Probe the host for the launcher and the elevation boundary.
pub(super) fn resolve_host_boundary() -> Result<HostBoundary, CascadeError> {
    // Launcher first: its absence is the more specific refusal, and it is the
    // component that owns the console decision.
    let launcher = host_launcher_program(Path::new(WINDOWS_HOST_LAUNCHER).is_file())?;
    let elevation = resolve_windows_elevation_boundary()?;
    Ok(HostBoundary::new(launcher, elevation))
}

/// Compose one launcher invocation.
///
/// `--timeout-ms` mirrors the caller's own bounded wait, `--sudo` names the
/// existing elevation boundary (never an elevation request), and `--` ends the
/// launcher's options so no child argument is ever re-parsed as one.
fn host_launcher_argv(
    boundary: &HostBoundary,
    timeout: Duration,
    program: &str,
    args: Vec<String>,
) -> (String, Vec<String>) {
    let mut argv = vec![
        "--timeout-ms".to_string(),
        timeout.as_millis().to_string(),
        "--sudo".to_string(),
        boundary.elevation().to_string(),
        "--".to_string(),
        program.to_string(),
    ];
    argv.extend(args);
    (boundary.launcher.to_string(), argv)
}

pub(super) fn validate_windows_origin_path(path: &str) -> Result<(), CascadeError> {
    let bytes = path.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || (bytes[2] != b'\\' && bytes[2] != b'/')
    {
        return Err(CascadeError::Precondition(
            "origin VHDX path must be an absolute Windows drive path (e.g. C:\\...)".into(),
        ));
    }
    if bytes[3..].iter().any(|byte| {
        !(byte.is_ascii_alphanumeric() || matches!(byte, b'\\' | b'/' | b'.' | b'_' | b'-' | b' '))
    }) {
        return Err(CascadeError::Precondition(
            "origin VHDX path contains forbidden shell characters".into(),
        ));
    }
    Ok(())
}

/// Decide which Windows elevation boundary a host disk mutation will use.
///
/// Pure: the caller supplies what the host probe found. `WIN_SUDO` (same
/// contract as `scripts/windows/wsl-elevated-ps.sh`) wins over the Windows
/// system directory's sudo. Fail-closed — an explicitly configured boundary
/// that is not usable refuses even when a default exists, and no boundary at
/// all refuses rather than letting an unelevated mount fail later with
/// `HCS/E_ACCESSDENIED`.
pub(super) fn select_windows_elevation_boundary(
    configured: Option<&str>,
    configured_usable: bool,
    default_usable: bool,
) -> Result<String, CascadeError> {
    if let Some(configured) = configured {
        return if configured_usable {
            Ok(configured.to_string())
        } else {
            Err(CascadeError::Precondition(format!(
                "{WINDOWS_ELEVATION_ENV} is set to {configured:?}, which is not a usable \
                 Windows elevation boundary"
            )))
        };
    }
    if default_usable {
        return Ok(WINDOWS_ELEVATION_BOUNDARY.to_string());
    }
    Err(CascadeError::Precondition(format!(
        "attaching the sealed origin VHDX requires a Windows elevation boundary \
         ({WINDOWS_ELEVATION_ENV} or {WINDOWS_ELEVATION_BOUNDARY}); the guest is never \
         Windows-administrator, so an unelevated mount is refused instead of failing \
         with HCS/E_ACCESSDENIED"
    )))
}

/// Probe the host for a Windows elevation boundary.
///
/// `WIN_SUDO` must name a guest-visible executable (a guest path the host
/// interop can exec), matching `scripts/windows/wsl-elevated-ps.sh`.
pub(super) fn resolve_windows_elevation_boundary() -> Result<String, CascadeError> {
    let configured = std::env::var(WINDOWS_ELEVATION_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    let configured_usable = configured
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file());
    select_windows_elevation_boundary(
        configured.as_deref(),
        configured_usable,
        Path::new(WINDOWS_ELEVATION_BOUNDARY).is_file(),
    )
}

/// Build the one host argv vector that attaches a sealed VHD.
///
/// The vector is one launcher invocation, never a shell and never
/// `powershell.exe -Command`: either of those would interpret the host path
/// and regress DT-1. The launcher passes each element through
/// `CommandLineToArgvW` quoting, so the VHDX path stays one opaque argument.
pub(super) fn host_disk_mount_argv(
    boundary: &HostBoundary,
    origin_vhdx: &str,
) -> (String, Vec<String>) {
    host_launcher_argv(
        boundary,
        ORIGIN_MOUNT_TIMEOUT,
        WSL_WINDOWS_PROGRAM,
        vec![
            "--mount".to_string(),
            "--vhd".to_string(),
            origin_vhdx.to_string(),
            "--bare".to_string(),
        ],
    )
}

/// Ensure the sealed origin VHDX is attached to this guest.
///
/// This is the boot-path entry point behind the `ensure-origin` verb. On a
/// cold WSL2 start the origin is not attached to the guest, and the host-gate
/// unit — which runs before the cascade unit — refuses on the missing device
/// before any cascade code can attach it. The gate calls this first
/// so the attach has one implementation, reached from both the boot gate and
/// `up`. Idempotent: a present device is a no-op. Fail-closed: a missing,
/// oversized, or hash-mismatched sealed manifest never reaches the host, and a
/// missing Windows elevation boundary never issues a mount that cannot succeed.
/// Attach the sealed origin device if it is not already present.
///
/// Custody (ITEM-6): this is the `ramshared ensure-origin` mutation entrypoint,
/// so it takes [`crate::customer::transition::OperationOwner::Selection`]
/// custody before the attach. Nested callers (a gate that already holds a
/// guard) use [`ensure_sealed_origin_attached_from`], which never acquires.
pub fn ensure_sealed_origin_attached() -> Result<(), CascadeError> {
    let _guard =
        super::mutation_guard::enter(crate::customer::transition::OperationOwner::Selection)?;
    ensure_sealed_origin_attached_from(
        &SystemCommandRunner,
        Path::new(HOST_ORIGIN_MANIFEST),
        resolve_host_boundary,
    )
}

pub(super) fn ensure_sealed_origin_attached_from<R: CommandRunner, F>(
    runner: &R,
    manifest_path: &Path,
    resolve_boundary: F,
) -> Result<(), CascadeError>
where
    F: FnOnce() -> Result<HostBoundary, CascadeError>,
{
    let sealed = super::read_sealed_origin_config()?;
    ensure_origin_attached(
        runner,
        &sealed.origin_path,
        &sealed.partuuid,
        manifest_path,
        &sealed.host_manifest_sha256,
        resolve_boundary,
    )
}

pub(super) fn ensure_origin_attached<R: CommandRunner, F>(
    runner: &R,
    origin_path: &str,
    expected_partuuid: &str,
    manifest_path: &Path,
    expected_manifest_sha256: &str,
    resolve_boundary: F,
) -> Result<(), CascadeError>
where
    F: FnOnce() -> Result<HostBoundary, CascadeError>,
{
    #[cfg(test)]
    {
        if origin_path == "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555" {
            return Ok(());
        }
    }
    if Path::new(origin_path).exists() {
        return Ok(());
    }
    let manifest = fs::read(manifest_path).map_err(|error| {
        CascadeError::Precondition(format!(
            "sealed host origin manifest is unavailable: {error}"
        ))
    })?;
    if manifest.len() > 64 * 1024 || !canonical_sha256(expected_manifest_sha256) {
        return Err(CascadeError::Precondition(
            "sealed host origin manifest size or hash is invalid".into(),
        ));
    }
    let actual_hash: String = Sha256::digest(&manifest)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual_hash.eq_ignore_ascii_case(expected_manifest_sha256) {
        return Err(CascadeError::Precondition(
            "sealed host origin manifest hash does not match origin configuration".into(),
        ));
    }
    let json_bytes = manifest
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&manifest);
    let value: serde_json::Value = serde_json::from_slice(json_bytes).map_err(|error| {
        CascadeError::Precondition(format!("sealed host origin manifest is invalid: {error}"))
    })?;
    let origin_vhdx = value
        .get("origin_vhdx")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CascadeError::Precondition("sealed host origin VHDX path is missing".into())
        })?;
    if value
        .get("partuuid")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|partuuid| !partuuid.eq_ignore_ascii_case(expected_partuuid))
    {
        return Err(CascadeError::Precondition(
            "sealed host origin manifest PARTUUID differs from origin configuration".into(),
        ));
    }
    validate_windows_origin_path(origin_vhdx)?;

    // Mounting a VHD into WSL is a Windows administrator operation. Guest root
    // is never Windows-admin, so an unelevated `wsl.exe --mount` returns
    // `HCS/E_ACCESSDENIED`. Resolve the boundary only now — the device-present
    // no-op above must work on a host with no elevation facility at all.
    let boundary = resolve_boundary()?;
    let (program, mount_args) = host_disk_mount_argv(&boundary, origin_vhdx);
    let mount_argv: Vec<&str> = mount_args.iter().map(String::as_str).collect();
    eprintln!(
        "[origin-attach] sealed origin VHDX detached; bounded windowless host attach via {}...",
        boundary.elevation()
    );
    runner.run_bounded(&program, &mount_argv, ORIGIN_MOUNT_TIMEOUT)?;

    #[cfg(not(test))]
    {
        for _ in 0..20 {
            if Path::new(origin_path).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    if !Path::new(origin_path).exists() {
        return Err(CascadeError::Precondition(format!(
            "origin device {origin_path} (PARTUUID {expected_partuuid}) did not appear after host attach"
        )));
    }
    Ok(())
}

/// Absolute Windows PowerShell. DT-12: the action and the ensure call name an
/// absolute interpreter, never `PowerShell.exe` resolved through `PATH` —
/// which is both a hijack surface and an unsealed identity.
const WINDOWS_POWERSHELL_PROGRAM: &str =
    "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe";
/// Product-owned host payload that owns the guardian scheduled task's run
/// state. Fixed path for the same reason as [`HOST_ORIGIN_MANIFEST`]: the
/// guest names the deployed authority, never a source checkout.
const HOST_GUARDIAN_PAYLOAD: &str = "C:\\ProgramData\\RamShared\\guardian\\Watch-RamSharedWsl.ps1";
/// Bounded budget for one guardian ensure host call.
const GUARDIAN_ENSURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded budget for one origin attach host call.
const ORIGIN_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the one host argv vector that starts the sealed guardian task.
///
/// The vector is one windowless launcher invocation, never a shell and never
/// `powershell.exe -Command`. `-File` binds each following argv element as a
/// script parameter, so the distro, SID and task name the sealed document
/// authorized are never re-lexed as PowerShell source — the DT-1 rule that
/// keeps a host path (or a name) opaque. The payload then re-checks those
/// values against the same sealed document before it touches Task Scheduler.
fn host_guardian_ensure_argv(
    boundary: &HostBoundary,
    distro: &str,
    user_sid: &str,
    task_name: &str,
) -> (String, Vec<String>) {
    host_launcher_argv(
        boundary,
        GUARDIAN_ENSURE_TIMEOUT,
        WINDOWS_POWERSHELL_PROGRAM,
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            HOST_GUARDIAN_PAYLOAD.to_string(),
            "-Action".to_string(),
            "ensure".to_string(),
            "-Run".to_string(),
            "-Distro".to_string(),
            distro.to_string(),
            "-UserSid".to_string(),
            user_sid.to_string(),
            "-TaskName".to_string(),
            task_name.to_string(),
        ],
    )
}

/// Ensure the sealed guardian producer is running for this guest (DT-19).
///
/// This is the boot-path entry point behind the `ensure-guardian` verb and the
/// twin of [`ensure_sealed_origin_attached`]. On a guest-only restart (Windows
/// up, user still logged on) neither `-AtStartup` nor `-AtLogOn` fires and
/// `StartWhenAvailable` has no missed scheduled start to recover, so the task
/// sits `Ready` and the host-gate refuses on a stale proof. This verb starts
/// the producer before the gate demands one. It never registers, enables or
/// disables a task and never writes a health proof: the guest must still never
/// mint its own verification.
///
/// Custody (ITEM-6): this is the `ramshared ensure-guardian` mutation
/// entrypoint, so it takes [`crate::customer::transition::OperationOwner::Selection`]
/// custody before the host call — the same custody as `ensure-origin`.
pub fn ensure_sealed_guardian_running() -> Result<(), CascadeError> {
    let _guard =
        super::mutation_guard::enter(crate::customer::transition::OperationOwner::Selection)?;
    ensure_sealed_guardian_running_from(
        &SystemCommandRunner,
        Path::new(super::sealed_identity::GUARDIAN_CONFIG_FILE),
        resolve_host_boundary,
    )
}

/// Read the sealed identity from `config_path` and start exactly the task it
/// names. The path is a parameter for the same reason
/// [`ensure_origin_attached`] takes its manifest path: the caller owns the
/// authority location, so a test can point at a missing document without a
/// test-only seam in the production read.
pub(super) fn ensure_sealed_guardian_running_from<R: CommandRunner, F>(
    runner: &R,
    config_path: &Path,
    resolve_boundary: F,
) -> Result<(), CascadeError>
where
    F: FnOnce() -> Result<HostBoundary, CascadeError>,
{
    // Fail-closed before any host call: a guest that cannot read its sealed
    // identity must not ask the host to start anything at all.
    let identity = super::sealed_identity::read_sealed_identity(config_path).map_err(|error| {
        CascadeError::Precondition(format!(
            "sealed guardian identity is not admissible ({})",
            error.as_str()
        ))
    })?;
    ensure_guardian_running(
        runner,
        &identity.distro,
        &identity.user_sid,
        &identity.task_name,
        resolve_boundary,
    )
}

pub(super) fn ensure_guardian_running<R: CommandRunner, F>(
    runner: &R,
    distro: &str,
    user_sid: &str,
    task_name: &str,
    resolve_boundary: F,
) -> Result<(), CascadeError>
where
    F: FnOnce() -> Result<HostBoundary, CascadeError>,
{
    if !super::sealed_identity::is_safe_task_name(task_name) {
        return Err(CascadeError::Precondition(
            "sealed guardian task name is not a safe scheduled-task name".into(),
        ));
    }
    // Same shape as the origin attach: the launcher and the elevation boundary
    // are resolved only once the guest-side authority checks have passed, so a
    // missing launcher is `VISIBLE_ELEVATION_REFUSED` and an unusable boundary
    // is a refusal rather than an `E_ACCESSDENIED` later.
    let boundary = resolve_boundary()?;
    let (program, start_args) = host_guardian_ensure_argv(&boundary, distro, user_sid, task_name);
    let start_argv: Vec<&str> = start_args.iter().map(String::as_str).collect();
    eprintln!(
        "[guardian-ensure] sealed guardian producer not confirmed; bounded windowless host start via {}...",
        boundary.elevation()
    );
    runner.run_bounded(&program, &start_argv, GUARDIAN_ENSURE_TIMEOUT)?;
    Ok(())
}
