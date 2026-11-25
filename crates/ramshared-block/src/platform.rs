// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/platform.rs
//! Bounded kernel and WSL interop observations; no lifecycle/serving authority.
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_KERNEL_IDENTITY_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformKind {
    NativeLinux,
    Wsl1,
    Wsl2,
}

pub struct PlatformProbe {
    pub platform: Option<PlatformKind>,
    pub release: String,
    pub version: String,
}

fn read_identity(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_KERNEL_IDENTITY_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_KERNEL_IDENTITY_BYTES {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_owned())
}

/// Missing kernel evidence or uncertain interop state never means native Linux.
pub fn probe_platform() -> PlatformProbe {
    probe_with(
        Path::new("/proc/sys/kernel/osrelease"),
        Path::new("/proc/version"),
        &[
            Path::new("/run/WSL"),
            Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"),
        ],
        std::env::var_os("WSL_INTEROP").is_some(),
    )
}

fn probe_with(
    release_path: &Path,
    version_path: &Path,
    markers: &[&Path],
    interop_env: bool,
) -> PlatformProbe {
    let release = read_identity(release_path);
    let version = read_identity(version_path);
    let mut interop = interop_evidence(markers);
    if interop_env {
        interop = Some(true);
    }
    let platform = match (&release, &version) {
        (Some(release), Some(version)) => {
            resolve_platform(classify_platform(release, version), interop)
        }
        _ => None,
    };
    PlatformProbe {
        platform,
        release: release.unwrap_or_default(),
        version: version.unwrap_or_default(),
    }
}

fn interop_evidence(paths: &[&Path]) -> Option<bool> {
    let mut unknown = false;
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Some(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => unknown = true,
        }
    }
    if unknown { None } else { Some(false) }
}

fn resolve_platform(kernel: Option<PlatformKind>, interop: Option<bool>) -> Option<PlatformKind> {
    match (kernel, interop) {
        (known @ Some(PlatformKind::Wsl1 | PlatformKind::Wsl2), _) => known,
        (_, Some(true)) => None,
        (_, None) => None,
        (kernel, Some(false)) => kernel,
    }
}

fn classify_platform(release: &str, version: &str) -> Option<PlatformKind> {
    if release.trim().is_empty() && version.trim().is_empty() {
        return None;
    }
    let release_lower = release.to_lowercase();
    let combined = format!("{release} {version}").to_lowercase();
    if combined.contains("wsl2") {
        return Some(PlatformKind::Wsl2);
    }
    if release_lower.contains("microsoft") {
        return Some(PlatformKind::Wsl1);
    }
    if combined.contains("wsl") || combined.contains("microsoft") {
        return None;
    }
    if combined.contains("linux") || !release.trim().is_empty() {
        return Some(PlatformKind::NativeLinux);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn custom_kernel_interop_and_unknown_evidence_never_authorize_native_writes() {
        let kernel = classify_platform("6.18-custom", "Linux version custom");
        assert_eq!(resolve_platform(kernel, Some(true)), None);
        assert_eq!(resolve_platform(kernel, None), None);
        assert_eq!(
            resolve_platform(kernel, Some(false)),
            Some(PlatformKind::NativeLinux)
        );
        assert_eq!(resolve_platform(None, Some(true)), None);
        assert_eq!(
            resolve_platform(Some(PlatformKind::Wsl1), Some(true)),
            Some(PlatformKind::Wsl1)
        );
        let root =
            std::env::temp_dir().join(format!("ramshared-interop-evidence-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        assert_eq!(interop_evidence(&[&root.join("missing")]), Some(false));
        std::fs::write(root.join("marker"), "").unwrap();
        assert_eq!(interop_evidence(&[&root.join("marker")]), Some(true));
        assert_eq!(interop_evidence(&[&root.join("marker/invalid")]), None);
        std::fs::remove_dir_all(root).unwrap();
    }
    /// AUD-39. Native Linux is a platform, not a failed WSL.
    #[test]
    fn classify_platform_reports_native_linux_without_failing_wsl() {
        assert_eq!(
            classify_platform("6.18.40.1-microsoft-standard-WSL2+", "#1 SMP"),
            Some(PlatformKind::Wsl2)
        );
        assert_eq!(
            classify_platform("5.15.167.4-microsoft-standard-WSL2", "#1"),
            Some(PlatformKind::Wsl2)
        );
        assert_eq!(
            classify_platform("4.4.0-19041-Microsoft", "#1"),
            Some(PlatformKind::Wsl1)
        );
        assert_eq!(
            classify_platform("6.8.0-45-generic", "#45-Ubuntu"),
            Some(PlatformKind::NativeLinux)
        );
        assert_eq!(classify_platform("", ""), None);
    }

    #[test]
    fn kernel_wsl_hints_without_a_qualified_kind_never_authorize_native() {
        for (release, version) in [
            ("6.6.0-wsl", "Linux version custom"),
            ("6.6-custom", "Linux version WSL"),
            ("6.6-custom", "Linux version Microsoft custom"),
        ] {
            assert_ne!(
                classify_platform(release, version),
                Some(PlatformKind::NativeLinux),
                "unresolved WSL hint was native: {release:?} {version:?}"
            );
        }
    }

    #[test]
    fn kernel_identity_reads_are_bounded_and_reject_missing_or_invalid_evidence() {
        let root =
            std::env::temp_dir().join(format!("ramshared-kernel-evidence-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("identity");
        assert_eq!(read_identity(&path), None);
        std::fs::write(&path, "  Linux version observed\n").unwrap();
        assert_eq!(
            read_identity(&path).as_deref(),
            Some("Linux version observed")
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert_eq!(read_identity(&path), None);
        std::fs::write(&path, vec![b'x'; MAX_KERNEL_IDENTITY_BYTES as usize + 1]).unwrap();
        assert_eq!(read_identity(&path), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kernel_identity_blank_or_control_content_is_unknown() {
        let root =
            std::env::temp_dir().join(format!("ramshared-platform-empty-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("identity");
        for data in [
            b"".as_slice(),
            b" \n\t",
            b"Linux\0version",
            b"Linux\x1bversion",
        ] {
            std::fs::write(&file, data).unwrap();
            let observed = read_identity(&file);
            std::fs::remove_file(&file).unwrap();
            if observed.is_some() {
                std::fs::remove_dir(&root).unwrap();
            }
            assert!(
                observed.is_none(),
                "blank/control kernel identity was accepted: {observed:?}"
            );
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn wsl2_presence_folds_three_probes_without_inventing_a_negative() {
        let native = classify_platform("6.8-native", "Linux version native");
        let wsl = classify_platform("6.8-microsoft-standard-WSL2", "Linux version observed");
        assert_eq!(resolve_platform(wsl, None), Some(PlatformKind::Wsl2));
        assert_eq!(
            resolve_platform(native, Some(false)),
            Some(PlatformKind::NativeLinux)
        );
        assert_eq!(resolve_platform(native, Some(true)), None);
        assert_eq!(resolve_platform(native, None), None);
        assert_eq!(resolve_platform(None, Some(false)), None);
    }

    #[test]
    fn path_presence_distinguishes_absent_from_unprobed() {
        let root =
            std::env::temp_dir().join(format!("ramshared-path-presence-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let present = root.join("present");
        std::fs::write(&present, b"x").unwrap();
        assert_eq!(interop_evidence(&[&present]), Some(true));
        assert_eq!(interop_evidence(&[&root.join("absent")]), Some(false));
        assert_eq!(interop_evidence(&[&present.join("invalid")]), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn platform_probe_requires_two_valid_identities_and_complete_interop_evidence() {
        let root =
            std::env::temp_dir().join(format!("ramshared-platform-joined-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let release = root.join("release");
        let version = root.join("version");
        let marker = root.join("marker");
        let probe = |env| probe_with(&release, &version, &[&marker], env).platform;
        std::fs::write(&release, "6.18-custom").unwrap();
        std::fs::write(&version, "Linux version custom").unwrap();
        assert_eq!(probe(false), Some(PlatformKind::NativeLinux));
        assert_eq!(probe(true), None);
        std::fs::write(&marker, "").unwrap();
        assert_eq!(probe(false), None);
        std::fs::remove_file(&marker).unwrap();
        for path in [&release, &version] {
            let original = std::fs::read(path).unwrap();
            std::fs::write(path, " \n\t").unwrap();
            assert_eq!(probe(false), None);
            std::fs::write(path, &original).unwrap();
        }
        std::fs::write(&release, "6.6-microsoft-standard-WSL2").unwrap();
        assert_eq!(probe(false), Some(PlatformKind::Wsl2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
