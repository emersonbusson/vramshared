// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/package_builder.rs
//! Product package manifest builder (DT-18).
//!
//! The installer reads `product-manifest.json` through [`parse_manifest`], but
//! the manifest used to be written by a lab PowerShell function. Two writers of
//! one contract drift silently: the parser could start refusing a package the
//! packager still emitted, and nothing would notice until an install failed.
//!
//! This module is the single writer. It hashes the seven canonical artifacts,
//! builds the manifest, and re-parses the exact bytes it is about to write
//! through the same [`parse_manifest`] the installer uses — a package that this
//! builder produces is a package the installer has already accepted.
//!
//! Packaging is host-independent: the builder runs on Linux or Windows, so a
//! Windows package can be assembled and inspected without the target host.

use crate::config::WinDriveConfig;
use crate::package::{
    ArtifactRole, ManifestArtifact, ProductManifestV1, ServiceIdentities, StartPolicy,
    parse_manifest, validate_cross_config,
};
use ramshared_winbroker::BrokerConfigV1;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// File the installer reads from a package directory.
pub const MANIFEST_FILE: &str = "product-manifest.json";

/// The only architecture the product manifest admits.
pub const PRODUCT_ARCHITECTURE: &str = "x86_64-pc-windows-msvc";

/// The seven product artifacts and the name each one must carry in a package.
pub const CANONICAL_ARTIFACTS: [(ArtifactRole, &str); 7] = [
    (ArtifactRole::BrokerExe, "ramshared-winbroker.exe"),
    (ArtifactRole::BrokerConfig, "broker.toml"),
    (ArtifactRole::WinsvcExe, "ramshared-winsvc.exe"),
    (ArtifactRole::WinsvcConfig, "winsvc.toml"),
    (ArtifactRole::DriverInf, "ramshared.inf"),
    (ArtifactRole::DriverCat, "ramshared.cat"),
    (ArtifactRole::DriverSys, "ramshared.sys"),
];

/// Everything a package declares apart from the artifacts themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageSpec {
    pub version: String,
    pub commit: String,
    pub start_policy: StartPolicy,
}

/// Parsed Windows INF `[Version]` `DriverVer = MM/DD/YYYY,major.minor.build.rev` directive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InfDriverVer {
    pub month: u8,
    pub day: u8,
    pub year: u16,
    pub version: [u16; 4],
}

impl InfDriverVer {
    /// True when `self` strictly supersedes `previous` for Windows PnP ranking:
    /// the date `(year, month, day)` does not regress and the 4-part version
    /// tuple is strictly higher.
    pub fn supersedes(&self, previous: &Self) -> bool {
        let self_date = (self.year, self.month, self.day);
        let prev_date = (previous.year, previous.month, previous.day);
        self_date >= prev_date && self.version > previous.version
    }
}

impl fmt::Display for InfDriverVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}/{:02}/{:04},{}.{}.{}.{}",
            self.month,
            self.day,
            self.year,
            self.version[0],
            self.version[1],
            self.version[2],
            self.version[3]
        )
    }
}

#[derive(Debug)]
pub enum PackageBuildError {
    /// The version or commit cannot become a version directory.
    Spec(String),
    /// An artifact is missing, is not a regular file, or cannot be read.
    Io(String),
    /// The bytes this builder produced were refused by the installer parser.
    Manifest(String),
    /// A package that the installer would refuse at staging time.
    Verify(String),
}

impl fmt::Display for PackageBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec(reason) => write!(f, "package spec: {reason}"),
            Self::Io(reason) => write!(f, "package artifact: {reason}"),
            Self::Manifest(reason) => write!(f, "package manifest: {reason}"),
            Self::Verify(reason) => write!(f, "package verify: {reason}"),
        }
    }
}

impl std::error::Error for PackageBuildError {}

fn validate_spec(spec: &PackageSpec) -> Result<(), PackageBuildError> {
    let version_ok = !spec.version.is_empty()
        && spec.version.len() <= 64
        && spec.version != "."
        && spec.version != ".."
        && !spec.version.starts_with('.')
        && spec
            .version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if !version_ok {
        return Err(PackageBuildError::Spec(format!(
            "version {:?} is not a path-safe release identifier",
            spec.version
        )));
    }
    let commit_ok = (12..=64).contains(&spec.commit.len())
        && spec.commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !commit_ok {
        return Err(PackageBuildError::Spec(format!(
            "commit {:?} must be 12..=64 hexadecimal characters",
            spec.commit
        )));
    }
    Ok(())
}

fn sha256_upper(path: &Path) -> Result<String, PackageBuildError> {
    let mut file = File::open(path)
        .map_err(|error| PackageBuildError::Io(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| PackageBuildError::Io(format!("{}: {error}", path.display())))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect())
}

/// Build and validate the manifest for the package in `root`.
///
/// Every canonical artifact must be a regular file reached without following a
/// redirect: a symlinked artifact would put bytes in the manifest that are not
/// the bytes the installer stages.
pub fn build_manifest(
    root: &Path,
    spec: &PackageSpec,
) -> Result<ProductManifestV1, PackageBuildError> {
    validate_spec(spec)?;
    let mut artifacts = Vec::with_capacity(CANONICAL_ARTIFACTS.len());
    for (role, name) in CANONICAL_ARTIFACTS {
        let path = root.join(name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| PackageBuildError::Io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(PackageBuildError::Io(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        artifacts.push(ManifestArtifact {
            role,
            relative_path: name.to_string(),
            sha256: sha256_upper(&path)?,
        });
    }
    let manifest = ProductManifestV1 {
        schema: 1,
        version: spec.version.clone(),
        commit: spec.commit.clone(),
        architecture: PRODUCT_ARCHITECTURE.to_string(),
        start_policy: spec.start_policy.clone(),
        services: ServiceIdentities {
            broker_name: "RamSharedBroker".to_string(),
            broker_account: r"NT SERVICE\RamSharedBroker".to_string(),
            consumer_name: "RamSharedWinSvc".to_string(),
            consumer_account: "LocalSystem".to_string(),
        },
        artifacts,
    };
    // One contract: the writer proves its own output through the installer's
    // reader before anything touches the package directory.
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| PackageBuildError::Manifest(error.to_string()))?;
    parse_manifest(&bytes).map_err(PackageBuildError::Manifest)
}

/// Build the manifest for `dir` from its parts and write it beside the
/// artifacts.
pub fn write_package(
    dir: &str,
    version: &str,
    commit: &str,
    start_policy: StartPolicy,
) -> Result<ProductManifestV1, PackageBuildError> {
    write_manifest(
        Path::new(dir),
        &PackageSpec {
            version: version.to_string(),
            commit: commit.to_string(),
            start_policy,
        },
    )
}

pub fn write_manifest(
    root: &Path,
    spec: &PackageSpec,
) -> Result<ProductManifestV1, PackageBuildError> {
    let manifest = build_manifest(root, spec)?;
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| PackageBuildError::Manifest(error.to_string()))?;
    let target: PathBuf = root.join(MANIFEST_FILE);
    let staged = root.join(format!(".{MANIFEST_FILE}.tmp"));
    let _ = std::fs::remove_file(&staged);
    std::fs::write(&staged, &bytes)
        .map_err(|error| PackageBuildError::Io(format!("{}: {error}", staged.display())))?;
    if cfg!(windows) {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&staged, &target)
        .map_err(|error| PackageBuildError::Io(format!("{}: {error}", target.display())))?;
    Ok(manifest)
}

/// Check a package the way `install` will before it reaches Windows.
///
/// The builder proves the manifest is accepted; this proves the package is
/// coherent: every artifact matches its hash, both configs parse, they agree
/// on capacity and tenant, and the INF actually references the driver binary
/// it ships. Catalog trust is not evaluated here (it needs `WinVerifyTrust`
/// on the target); the check is a refusal, not a signature claim.
pub fn verify_package(root: &Path) -> Result<ProductManifestV1, PackageBuildError> {
    let manifest_bytes = std::fs::read(root.join(MANIFEST_FILE))
        .map_err(|error| PackageBuildError::Io(format!("{MANIFEST_FILE}: {error}")))?;
    let manifest = parse_manifest(&manifest_bytes).map_err(PackageBuildError::Manifest)?;
    crate::package::verify_staged_files(root, &manifest).map_err(PackageBuildError::Verify)?;

    let read = |role: ArtifactRole| -> Result<Vec<u8>, PackageBuildError> {
        let relative = &manifest
            .artifact(role)
            .map_err(PackageBuildError::Manifest)?
            .relative_path;
        std::fs::read(root.join(relative))
            .map_err(|error| PackageBuildError::Io(format!("{relative}: {error}")))
    };
    let broker = BrokerConfigV1::from_toml(&read(ArtifactRole::BrokerConfig)?)
        .map_err(|error| PackageBuildError::Verify(format!("broker config: {error}")))?;
    let winsvc = WinDriveConfig::from_reader(&read(ArtifactRole::WinsvcConfig)?)
        .map_err(|error| PackageBuildError::Verify(format!("winsvc config: {error}")))?;
    validate_cross_config(&broker, &winsvc).map_err(PackageBuildError::Verify)?;

    let inf_path = manifest
        .artifact(ArtifactRole::DriverInf)
        .map_err(PackageBuildError::Manifest)?
        .relative_path
        .clone();
    let sys_name = manifest
        .artifact(ArtifactRole::DriverSys)
        .map_err(PackageBuildError::Manifest)?
        .relative_path
        .clone();
    let inf = String::from_utf8(read(ArtifactRole::DriverInf)?)
        .map_err(|_| PackageBuildError::Verify(format!("{inf_path} is not UTF-8 text")))?;
    let _ = parse_inf_driver_ver(&inf)?;
    if !inf_references(&inf, &sys_name) {
        return Err(PackageBuildError::Verify(format!(
            "{inf_path} does not list {sys_name} under [SourceDisksFiles]"
        )));
    }
    Ok(manifest)
}

/// Parse and validate the `[Version]` `DriverVer` directive of a Windows INF.
pub fn parse_inf_driver_ver(inf: &str) -> Result<InfDriverVer, PackageBuildError> {
    let mut in_version = false;
    let mut found: Option<InfDriverVer> = None;
    for raw_line in inf.lines() {
        let line = raw_line.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1].trim();
            in_version = section.eq_ignore_ascii_case("version");
            continue;
        }
        if !in_version {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("driverver") {
            continue;
        }
        if found.is_some() {
            return Err(PackageBuildError::Verify(
                "duplicate DriverVer in [Version] section".to_string(),
            ));
        }
        let Some((date_str, ver_str)) = value.trim().split_once(',') else {
            return Err(PackageBuildError::Verify(
                "DriverVer must be formatted as MM/DD/YYYY,a.b.c.d".to_string(),
            ));
        };
        let date_parts: Vec<&str> = date_str.trim().split('/').collect();
        if date_parts.len() != 3 {
            return Err(PackageBuildError::Verify(
                "DriverVer date must be MM/DD/YYYY".to_string(),
            ));
        }
        let month: u8 = date_parts[0]
            .parse()
            .map_err(|_| PackageBuildError::Verify("invalid DriverVer month".to_string()))?;
        let day: u8 = date_parts[1]
            .parse()
            .map_err(|_| PackageBuildError::Verify("invalid DriverVer day".to_string()))?;
        let year: u16 = date_parts[2]
            .parse()
            .map_err(|_| PackageBuildError::Verify("invalid DriverVer year".to_string()))?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(2000..=9999).contains(&year)
        {
            return Err(PackageBuildError::Verify(
                "DriverVer date is out of bounds".to_string(),
            ));
        }
        let ver_parts: Vec<&str> = ver_str.trim().split('.').collect();
        if ver_parts.len() != 4 {
            return Err(PackageBuildError::Verify(
                "DriverVer version must have 4 numeric components".to_string(),
            ));
        }
        let mut version = [0u16; 4];
        for (idx, part) in ver_parts.iter().enumerate() {
            version[idx] = part.parse().map_err(|_| {
                PackageBuildError::Verify("invalid DriverVer version component".to_string())
            })?;
        }
        found = Some(InfDriverVer {
            month,
            day,
            year,
            version,
        });
    }
    found.ok_or_else(|| {
        PackageBuildError::Verify("missing DriverVer in [Version] section".to_string())
    })
}

/// Deterministically advance an INF's `[Version] DriverVer` to a strictly
/// superseding revision so a rebuilt `.sys` never reuses an existing `oem*.inf`
/// version in the Windows Driver Store.
pub fn bump_inf_driver_ver(
    inf: &str,
    date_mdy: Option<(u8, u8, u16)>,
) -> Result<(String, InfDriverVer), PackageBuildError> {
    let previous = parse_inf_driver_ver(inf)?;
    let (month, day, year) = date_mdy.unwrap_or((previous.month, previous.day, previous.year));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(2000..=9999).contains(&year) {
        return Err(PackageBuildError::Verify(
            "DriverVer target date is out of bounds".to_string(),
        ));
    }
    if (year, month, day) < (previous.year, previous.month, previous.day) {
        return Err(PackageBuildError::Verify(
            "DriverVer target date cannot regress before previous DriverVer date".to_string(),
        ));
    }
    let next_rev = previous.version[3].checked_add(1).ok_or_else(|| {
        PackageBuildError::Verify("DriverVer revision component overflowed u16::MAX".to_string())
    })?;
    let next = InfDriverVer {
        month,
        day,
        year,
        version: [
            previous.version[0],
            previous.version[1],
            previous.version[2],
            next_rev,
        ],
    };
    let mut in_version = false;
    let mut out = String::with_capacity(inf.len() + 32);
    for raw_line in inf.lines() {
        let trimmed = raw_line.split(';').next().unwrap_or("").trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed[1..trimmed.len() - 1].trim();
            in_version = section.eq_ignore_ascii_case("version");
        } else if in_version
            && let Some((key, _)) = trimmed.split_once('=')
            && key.trim().eq_ignore_ascii_case("driverver")
        {
            out.push_str(&format!("DriverVer   = {next}\n"));
            continue;
        }
        out.push_str(raw_line);
        out.push('\n');
    }
    let reparsed = parse_inf_driver_ver(&out)?;
    if !reparsed.supersedes(&previous) {
        return Err(PackageBuildError::Verify(
            "bumped DriverVer does not supersede previous DriverVer".to_string(),
        ));
    }
    Ok((out, reparsed))
}

/// Read an INF file from disk, advance its `[Version] DriverVer` to the next
/// strictly superseding revision, and write the updated INF back in place.
pub fn bump_inf_driver_ver_file(
    inf_path: &Path,
    date_mdy: Option<(u8, u8, u16)>,
) -> Result<InfDriverVer, PackageBuildError> {
    let bytes = std::fs::read(inf_path)
        .map_err(|error| PackageBuildError::Io(format!("{}: {error}", inf_path.display())))?;
    let inf = String::from_utf8(bytes).map_err(|_| {
        PackageBuildError::Verify(format!("{} is not UTF-8 text", inf_path.display()))
    })?;
    let (bumped, next_ver) = bump_inf_driver_ver(&inf, date_mdy)?;
    std::fs::write(inf_path, bumped.as_bytes())
        .map_err(|error| PackageBuildError::Io(format!("{}: {error}", inf_path.display())))?;
    Ok(next_ver)
}

/// Dispatch host-independent package builder CLI commands (`package`,
/// `package-verify`, `package-bump-inf`).
pub fn run_package_cli(cmd: &crate::runtime::ProductCommand) -> Option<i32> {
    use crate::runtime::ProductCommand;
    match cmd {
        ProductCommand::Package {
            dir,
            version,
            commit,
            start_policy,
        } => Some(
            match write_package(dir, version, commit, start_policy.clone()) {
                Ok(manifest) => {
                    println!(
                        "package manifest written for {}",
                        manifest.version_directory()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("package failed: {e}");
                    1
                }
            },
        ),
        ProductCommand::VerifyPackage { dir } => Some(match verify_package(Path::new(dir)) {
            Ok(manifest) => {
                println!("package verified for {}", manifest.version_directory());
                0
            }
            Err(e) => {
                eprintln!("package verify failed: {e}");
                1
            }
        }),
        ProductCommand::BumpInfDriverVer { inf } => {
            Some(match bump_inf_driver_ver_file(Path::new(inf), None) {
                Ok(next_ver) => {
                    println!("DriverVer bumped to {next_ver}");
                    0
                }
                Err(e) => {
                    eprintln!("package-bump-inf failed: {e}");
                    1
                }
            })
        }
        _ => None,
    }
}

/// Whether an INF lists `file_name` in a `[SourceDisksFiles*]` section.
///
/// Section-scoped on purpose: the same token can appear in `[Strings]` or a
/// comment without meaning that the file is part of the package.
pub fn inf_references(inf: &str, file_name: &str) -> bool {
    let mut in_source_disks = false;
    for line in inf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            in_source_disks = section.starts_with("sourcedisksfiles");
            continue;
        }
        if in_source_disks {
            let key = line.split('=').next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case(file_name) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;

    fn package_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ramshared-package-builder-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (role, file) in CANONICAL_ARTIFACTS {
            let contents = match role {
                ArtifactRole::BrokerConfig => {
                    r#"[local_broker]
schema = 1
capacity_bytes = 67108864
allowed_tenant = "windows-drive"
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
"#
                    .to_string()
                }
                ArtifactRole::WinsvcConfig => {
                    r#"[win_drive]
size_bytes = 67108864
block_size = 4096
cuda_device = 0
reserve_bytes = 536870912
queue_depth = 4
max_io_bytes = 1048576
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
volume_letter = "R"
broker_pipe = "named_pipe_v1"
broker_ready_timeout_secs = 30
tenant = "windows-drive"
heartbeat_secs = 5
"#
                    .to_string()
                }
                ArtifactRole::DriverInf => {
                    "[Version]\nSignature=\"$Windows NT$\"\nDriverVer = 08/09/2026,10.0.26200.8\n[SourceDisksFiles]\nramshared.sys = 1,,\n"
                        .to_string()
                }
                _ => format!("bytes for {role:?}"),
            };
            std::fs::write(root.join(file), contents).unwrap();
        }
        root
    }

    fn spec() -> PackageSpec {
        PackageSpec {
            version: "0.15.0".to_string(),
            commit: "abcdef1234567890".to_string(),
            start_policy: StartPolicy::Demand,
        }
    }

    fn hash_file(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }

    #[test]
    fn manifest_covers_every_canonical_artifact_with_its_own_hash() {
        let root = package_dir("hashes");
        let manifest = build_manifest(&root, &spec()).unwrap();
        assert_eq!(manifest.artifacts.len(), CANONICAL_ARTIFACTS.len());
        for ((role, file), artifact) in CANONICAL_ARTIFACTS.iter().zip(&manifest.artifacts) {
            assert_eq!(artifact.role, *role);
            assert_eq!(artifact.relative_path, *file);
            assert_eq!(artifact.sha256, hash_file(&root.join(file)));
        }
        crate::package::verify_staged_files(&root, &manifest).unwrap();
    }

    #[test]
    fn manifest_bytes_are_accepted_by_the_installer_parser() {
        let root = package_dir("roundtrip");
        let manifest = write_manifest(&root, &spec()).unwrap();
        let bytes = std::fs::read(root.join(MANIFEST_FILE)).unwrap();
        let parsed = parse_manifest(&bytes).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.version_directory(), "0.15.0-abcdef123456");
    }

    #[test]
    fn missing_artifact_is_named_in_the_refusal() {
        let root = package_dir("missing");
        std::fs::remove_file(root.join("ramshared.cat")).unwrap();
        let error = build_manifest(&root, &spec()).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("ramshared.cat"), "{text}");
    }

    #[test]
    fn spec_must_be_a_path_safe_version_and_hex_commit() {
        let root = package_dir("spec");
        for (version, commit) in [
            ("../escape", "abcdef1234567890"),
            ("", "abcdef1234567890"),
            (".hidden", "abcdef1234567890"),
            ("0.15.0", "abcdef12345"),
            ("0.15.0", "zzzzzzzzzzzz"),
        ] {
            let candidate = PackageSpec {
                version: version.to_string(),
                commit: commit.to_string(),
                start_policy: StartPolicy::Demand,
            };
            assert!(
                build_manifest(&root, &candidate).is_err(),
                "{version:?} / {commit:?} accepted"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_artifact_is_refused() {
        let root = package_dir("symlink");
        let real = root.join("ramshared.sys");
        let link = root.join("ramshared.cat");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = build_manifest(&root, &spec()).unwrap_err();
        assert!(error.to_string().contains("ramshared.cat"), "{error}");
    }

    #[test]
    fn verify_accepts_a_coherent_package() {
        let root = package_dir("verify-ok");
        let manifest = write_manifest(&root, &spec()).unwrap();
        let verified = verify_package(&root).unwrap();
        assert_eq!(verified, manifest);
    }

    #[test]
    fn verify_refuses_a_tampered_artifact() {
        let root = package_dir("verify-tamper");
        write_manifest(&root, &spec()).unwrap();
        std::fs::write(root.join("ramshared.sys"), b"tampered").unwrap();
        let error = verify_package(&root).unwrap_err();
        assert!(error.to_string().contains("hash mismatch"), "{error}");
    }

    #[test]
    fn verify_refuses_configs_that_disagree() {
        let root = package_dir("verify-cross");
        write_manifest(&root, &spec()).unwrap();
        let winsvc = std::fs::read_to_string(root.join("winsvc.toml")).unwrap();
        std::fs::write(
            root.join("winsvc.toml"),
            winsvc.replace("tenant = \"windows-drive\"", "tenant = \"other-tenant\""),
        )
        .unwrap();
        // The manifest still hashes the original bytes, so the tamper is
        // caught by the hash gate; re-sign the package to reach the config
        // comparison and prove it refuses there too.
        write_manifest(&root, &spec()).unwrap();
        let error = verify_package(&root).unwrap_err();
        assert!(error.to_string().contains("tenant"), "{error}");
    }

    #[test]
    fn verify_refuses_an_inf_that_does_not_list_the_driver() {
        let root = package_dir("verify-inf");
        std::fs::write(
            root.join("ramshared.inf"),
            "[Version]\nSignature=\"$Windows NT$\"\nDriverVer = 08/09/2026,10.0.26200.8\n[SourceDisksFiles]\nother.sys = 1,,\n",
        )
        .unwrap();
        write_manifest(&root, &spec()).unwrap();
        let error = verify_package(&root).unwrap_err();
        assert!(error.to_string().contains("SourceDisksFiles"), "{error}");
    }

    #[test]
    fn package_owner_generates_monotonic_driver_ver_and_proves_receipt_supersession() {
        let repo_inf = include_str!("../../../drivers/windows/ramshared/ramshared.inf");
        let initial_ver = parse_inf_driver_ver(repo_inf).unwrap();
        assert_eq!(
            initial_ver,
            InfDriverVer {
                month: 8,
                day: 9,
                year: 2026,
                version: [10, 0, 26200, 8],
            }
        );

        let root_v1 = package_dir("supersede-v1");
        std::fs::write(root_v1.join("ramshared.inf"), repo_inf).unwrap();
        std::fs::write(root_v1.join("ramshared.sys"), b"storport-binary-v8").unwrap();
        let manifest_v1 = write_manifest(
            &root_v1,
            &PackageSpec {
                version: "0.15.0-r8".to_string(),
                commit: "1111111111111111".to_string(),
                start_policy: StartPolicy::Demand,
            },
        )
        .unwrap();
        verify_package(&root_v1).unwrap();
        let plan_v1 =
            crate::installation::driver_store::plan_publication(&root_v1, &manifest_v1).unwrap();

        let (bumped_inf, next_ver) = bump_inf_driver_ver(repo_inf, None).unwrap();
        assert_eq!(next_ver.version, [10, 0, 26200, 9]);
        assert!(next_ver.supersedes(&initial_ver));
        assert!(!initial_ver.supersedes(&next_ver));

        let root_v2 = package_dir("supersede-v2");
        std::fs::write(root_v2.join("ramshared.inf"), repo_inf).unwrap();
        assert_eq!(
            run_package_cli(&crate::runtime::ProductCommand::BumpInfDriverVer {
                inf: root_v2.join("ramshared.inf").to_string_lossy().into_owned(),
            }),
            Some(0)
        );
        let file_ver =
            parse_inf_driver_ver(&std::fs::read_to_string(root_v2.join("ramshared.inf")).unwrap())
                .unwrap();
        assert_eq!(file_ver, next_ver);
        assert_eq!(
            std::fs::read_to_string(root_v2.join("ramshared.inf")).unwrap(),
            bumped_inf
        );
        std::fs::write(root_v2.join("ramshared.sys"), b"storport-binary-v9").unwrap();
        assert_eq!(
            run_package_cli(&crate::runtime::ProductCommand::Package {
                dir: root_v2.to_string_lossy().into_owned(),
                version: "0.15.0-r9".to_string(),
                commit: "2222222222222222".to_string(),
                start_policy: StartPolicy::Demand,
            }),
            Some(0)
        );
        assert_eq!(
            run_package_cli(&crate::runtime::ProductCommand::VerifyPackage {
                dir: root_v2.to_string_lossy().into_owned(),
            }),
            Some(0)
        );
        assert_eq!(
            run_package_cli(&crate::runtime::ProductCommand::InspectGpus),
            None
        );
        let manifest_v2 = write_manifest(
            &root_v2,
            &PackageSpec {
                version: "0.15.0-r9".to_string(),
                commit: "2222222222222222".to_string(),
                start_policy: StartPolicy::Demand,
            },
        )
        .unwrap();
        verify_package(&root_v2).unwrap();
        let plan_v2 =
            crate::installation::driver_store::plan_publication(&root_v2, &manifest_v2).unwrap();

        assert_ne!(plan_v1.inf_sha256, plan_v2.inf_sha256);
        assert_ne!(plan_v1.sys_sha256, plan_v2.sys_sha256);

        let mut receipts = crate::installation::driver_store::DriverReceipts::empty();
        receipts.record(&crate::installation::driver_store::PublishedDriver {
            oem_inf: "oem38.inf".to_string(),
            inf_sha256: plan_v1.inf_sha256.clone(),
            reboot_required: false,
        });
        receipts.record(&crate::installation::driver_store::PublishedDriver {
            oem_inf: "oem39.inf".to_string(),
            inf_sha256: plan_v2.inf_sha256.clone(),
            reboot_required: false,
        });
        assert_eq!(receipts.packages.len(), 2);

        // Also cover refusal branches on malformed/regressing DriverVer inputs.
        assert!(bump_inf_driver_ver(repo_inf, Some((8, 8, 2026))).is_err());
        assert!(parse_inf_driver_ver("[Version]\nSignature=\"$Windows NT$\"\n").is_err());
        assert!(
            parse_inf_driver_ver(
                "[Version]\nDriverVer=08/09/2026,1.0.0.1\nDriverVer=08/09/2026,1.0.0.2\n"
            )
            .is_err()
        );
    }

    #[test]
    fn automatic_start_policy_is_serialized() {
        let root = package_dir("automatic");
        let candidate = PackageSpec {
            start_policy: StartPolicy::Automatic,
            ..spec()
        };
        let manifest = write_manifest(&root, &candidate).unwrap();
        assert_eq!(manifest.start_policy, StartPolicy::Automatic);
        let text = std::fs::read_to_string(root.join(MANIFEST_FILE)).unwrap();
        assert!(text.contains("\"automatic\""), "{text}");
    }
}
