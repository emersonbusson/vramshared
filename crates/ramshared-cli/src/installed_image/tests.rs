// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/installed_image/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn build_identity_marks_dirty_and_unavailable_sources() {
    let commit = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
    assert_eq!(
        format_build_identity("0.15.0", commit, "clean"),
        "RamShared CLI v0.15.0 · abcdef01 (clean)"
    );
    assert_eq!(
        format_build_identity("0.15.0", commit, "dirty"),
        "RamShared CLI v0.15.0 · abcdef01 (dirty)"
    );
    assert_eq!(
        format_build_identity("0.15.0", "", "unavailable"),
        "RamShared CLI v0.15.0 · source unavailable"
    );
    assert_eq!(
        format_build_identity("0.15.0", commit, "unexpected"),
        "RamShared CLI v0.15.0 · abcdef01 (state unavailable)"
    );
}

#[test]
fn running_install_status_compares_current_executable_identity() {
    let root =
        std::env::temp_dir().join(format!("ramshared-running-identity-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let installed = root.join("usr/local/bin/ramshared");
    let local = root.join("build/ramshared");
    fs::create_dir_all(installed.parent().expect("installed parent"))
        .expect("create installed bin");
    fs::create_dir_all(local.parent().expect("local parent")).expect("create local bin");
    fs::write(&installed, b"installed binary").expect("write installed binary");
    fs::write(&local, b"local binary").expect("write local binary");
    let absent_active_release = root.join("missing-active/bin/ramshared");

    assert_eq!(
        running_install_status(Some(&installed), &installed, &absent_active_release),
        RunningInstallStatus::InstalledDirect
    );
    assert_eq!(
        running_install_status(Some(&local), &installed, &absent_active_release),
        RunningInstallStatus::NotInstalled
    );

    let replaced_target = root.join("replace-target/ramshared");
    let running_snapshot = root.join("running-image/ramshared");
    fs::create_dir_all(replaced_target.parent().expect("replacement parent"))
        .expect("create replacement target directory");
    fs::create_dir_all(running_snapshot.parent().expect("running parent"))
        .expect("create running image directory");
    fs::write(&replaced_target, b"binary mapped by running process")
        .expect("write original installed binary");
    fs::hard_link(&replaced_target, &running_snapshot).expect("snapshot running inode");
    let running_metadata = fs::metadata(&running_snapshot).expect("old running inode");
    assert_eq!(
        running_install_status_from_metadata(
            Some(&running_metadata),
            ProcessInstallOrigin::Direct,
            &replaced_target,
            &absent_active_release,
        ),
        RunningInstallStatus::InstalledDirect
    );
    let replacement = root.join("replace-target/staged");
    fs::write(&replacement, b"new installed binary").expect("write replacement binary");
    fs::rename(&replacement, &replaced_target).expect("atomically replace installed binary");
    assert_eq!(
        running_install_status_from_metadata(
            Some(&running_metadata),
            ProcessInstallOrigin::Direct,
            &replaced_target,
            &absent_active_release,
        ),
        RunningInstallStatus::UpdatePendingDirect
    );
    assert_eq!(
        running_install_status(None, &installed, &absent_active_release),
        RunningInstallStatus::Unknown
    );
    assert_eq!(
        running_install_status(
            Some(&local),
            &root.join("missing/ramshared"),
            &absent_active_release
        ),
        RunningInstallStatus::Unknown
    );
    assert_eq!(
        format_running_identity_line(
            "0.15.0",
            "abcdef0123456789abcdef0123456789abcdef01",
            "clean",
            RunningInstallStatus::InstalledDirect
        ),
        "Running: v0.15.0 · abcdef01 (clean) · matches the install"
    );
    assert_eq!(
        format_running_identity_line(
            "0.15.0",
            "",
            "unavailable",
            RunningInstallStatus::NotInstalled
        ),
        "Running: v0.15.0 · source unavailable · not installed"
    );
    assert_eq!(
        format_running_identity_line("0.15.0", "", "unavailable", RunningInstallStatus::Unknown),
        "Running: v0.15.0 · source unavailable · unknown"
    );

    let old_release = root.join("opt/ramshared/releases/v0.14.1/bin/ramshared");
    let active_release = root.join("opt/ramshared/releases/v0.15.0/bin/ramshared");
    fs::create_dir_all(old_release.parent().expect("old release bin"))
        .expect("create old release bin");
    fs::create_dir_all(active_release.parent().expect("active release bin"))
        .expect("create active release bin");
    fs::write(&old_release, b"old release binary").expect("write old release binary");
    fs::write(&active_release, b"active release binary").expect("write active release binary");
    let current_link = root.join("opt/ramshared/current");
    fs::create_dir_all(current_link.parent().expect("product root")).expect("create product root");
    #[cfg(unix)]
    std::os::unix::fs::symlink("releases/v0.15.0", &current_link).expect("select active release");
    assert_eq!(
        running_install_status(
            Some(&active_release),
            &installed,
            &current_link.join("bin/ramshared")
        ),
        RunningInstallStatus::InstalledActiveRelease
    );
    assert_eq!(
        running_install_status(
            Some(&old_release),
            &installed,
            &current_link.join("bin/ramshared")
        ),
        RunningInstallStatus::NotInstalled
    );
    let old_release_metadata = fs::metadata(&old_release).expect("old release inode");
    assert_eq!(
        running_install_status_from_metadata(
            Some(&old_release_metadata),
            ProcessInstallOrigin::ActiveRelease,
            &installed,
            &current_link.join("bin/ramshared"),
        ),
        RunningInstallStatus::UpdatePendingActiveRelease
    );

    fs::remove_dir_all(&root).expect("remove running identity fixtures");
}

#[test]
fn process_origin_survives_replaced_executable_paths() {
    let root =
        std::env::temp_dir().join(format!("ramshared-process-origin-{}", std::process::id()));
    let direct = root.join("usr/local/bin/ramshared");
    let active = root.join("opt/ramshared/current/bin/ramshared");
    let old_release = root.join("opt/ramshared/releases/v0.14.1/bin/ramshared");
    fs::create_dir_all(direct.parent().expect("direct bin")).expect("create direct bin");
    fs::create_dir_all(old_release.parent().expect("release bin")).expect("create release bin");
    fs::write(&direct, b"direct image").expect("write direct image");
    fs::write(&old_release, b"old release image").expect("write release image");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "releases/v0.14.1",
        active.parent().unwrap().parent().unwrap(),
    )
    .expect("select old release");

    let deleted_direct = PathBuf::from(format!("{} (deleted)", direct.display()));
    let deleted_release = PathBuf::from(format!("{} (deleted)", old_release.display()));
    assert_eq!(
        process_install_origin_for_path(&deleted_direct, &direct, &active),
        ProcessInstallOrigin::Direct
    );
    assert_eq!(
        process_install_origin_for_path(&deleted_release, &direct, &active),
        ProcessInstallOrigin::ActiveRelease
    );
    assert_eq!(
        process_install_origin_for_path(&root.join("target/debug/ramshared"), &direct, &active),
        ProcessInstallOrigin::Unknown
    );

    fs::remove_dir_all(&root).expect("remove process origin fixture");
}

#[test]
fn ramshared_top_reexec_targets_only_its_updated_install() {
    let root = std::env::temp_dir().join(format!("ramshared-tui-reexec-{}", std::process::id()));
    let target = root.join("usr/local/bin/ramshared");
    let old_image = root.join("running/ramshared");
    fs::create_dir_all(target.parent().expect("direct bin")).expect("create install bin");
    fs::create_dir_all(old_image.parent().expect("running bin")).expect("create running bin");
    fs::write(&target, b"new installed image").expect("write new installed image");
    fs::write(&old_image, b"old running image").expect("write old running image");
    let running_metadata = fs::metadata(&old_image).expect("stat running image");
    let installed = InstalledIdentity {
        executable: target.clone(),
        version: Some("0.15.0".to_string()),
        source_commit: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
        source_tree_state: Some("clean".to_string()),
        installed_at_utc: Some("2026-09-27T18:04:05Z".to_string()),
        executable_sha256: Some(sha256_file(&target).expect("hash new image")),
    };
    let running_sha256 = sha256_file(&old_image).expect("hash old image");

    assert_eq!(
        restart_target(
            ProcessInstallOrigin::Direct,
            Some(&running_metadata),
            Some(&running_sha256),
            true,
            Some(&installed),
            None,
        ),
        Some(target.clone())
    );
    assert_eq!(
        restart_target(
            ProcessInstallOrigin::Unknown,
            Some(&running_metadata),
            Some(&running_sha256),
            true,
            Some(&installed),
            None,
        ),
        None
    );
    assert_eq!(
        restart_target(
            ProcessInstallOrigin::Direct,
            Some(&running_metadata),
            Some(&running_sha256),
            false,
            Some(&installed),
            None,
        ),
        None
    );

    fs::write(&target, b"old running image").expect("install same running image content");
    let same_image_sha256 = sha256_file(&target).expect("hash identical installed image");
    let same_build = InstalledIdentity {
        executable: target.clone(),
        executable_sha256: Some(same_image_sha256.clone()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        source_commit: full_commit_sha(BUILD_GIT_SHA),
        source_tree_state: Some(BUILD_TREE_STATE.to_string()),
        installed_at_utc: Some("2026-09-27T18:04:05Z".to_string()),
    };
    assert_eq!(
        restart_target(
            ProcessInstallOrigin::Direct,
            Some(&running_metadata),
            Some(&same_image_sha256),
            true,
            Some(&same_build),
            None,
        ),
        None
    );

    fs::remove_dir_all(&root).expect("remove reexec fixture");
}

#[test]
fn installation_time_is_read_from_installer_metadata_only() {
    let root =
        std::env::temp_dir().join(format!("ramshared-install-metadata-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create release bin fixture");
    let executable = bin.join("ramshared");
    let release_commit = "abcdef0123456789abcdef0123456789abcdef01";
    fs::write(root.join("RELEASE_VERSION"), "v0.14.1\n").expect("write release version");
    fs::write(root.join("SOURCE_COMMIT"), format!("{release_commit}\n"))
        .expect("write source commit");
    fs::write(root.join("SOURCE_TREE_STATE"), "clean\n").expect("write tree state");
    fs::write(&executable, b"release binary").expect("write release executable");
    let daemon = root.join("bin/ramsharedd");
    fs::write(&daemon, b"release daemon").expect("write release daemon");
    let provenance = root.join("INSTALL_PROVENANCE.json");
    fs::write(
        &provenance,
        format!("{{\"schema_version\":\"{INSTALLED_PROVENANCE_V2}\",\"source_commit\":\"{release_commit}\",\"source_tree_state\":\"clean\",\"installed_at_utc\":\"2026-09-27T17:23:45Z\"}}"),
    )
    .expect("write installed provenance fixture");
    seal_release_fixture(&root);

    assert_eq!(
        read_install_timestamp_for_executable(&executable).as_deref(),
        Some("2026-09-27T17:23:45Z")
    );
    assert_eq!(
        read_installed_identity(&executable, InstallKind::ActiveRelease)
            .and_then(|metadata| metadata.version),
        Some("0.14.1".to_string())
    );
    let release_identity = read_installed_identity(&executable, InstallKind::ActiveRelease)
        .expect("read versioned installed identity");
    assert!(
        format_installed_identity_line("active release", Some(&release_identity)).contains(
            "Installed active release: v0.14.1 · abcdef01 (clean) · 2026-09-27 17:23:45 UTC"
        )
    );
    fs::write(&daemon, b"tampered release daemon").expect("tamper release daemon");
    let tampered = read_installed_identity(&executable, InstallKind::ActiveRelease)
        .expect("installed image still exists");
    assert!(
        format_installed_identity_line("active release", Some(&tampered))
            .contains("identity unknown")
    );
    fs::write(&daemon, b"release daemon").expect("restore release daemon");
    seal_release_fixture(&root);

    fs::write(
        &provenance,
        format!("{{\"schema_version\":\"{INSTALLED_PROVENANCE_V1}\",\"source_commit\":\"{release_commit}\",\"source_tree_state\":\"clean\"}}"),
    )
    .expect("write legacy provenance fixture");
    seal_release_fixture(&root);
    assert_eq!(read_install_timestamp_for_executable(&executable), None);
    let legacy_identity = read_installed_identity(&executable, InstallKind::ActiveRelease)
        .expect("versioned binary remains recognized without timestamp metadata");
    assert!(
        format_installed_identity_line("active release", Some(&legacy_identity))
            .contains("v0.14.1 · abcdef01 (clean) · install time unknown")
    );

    fs::remove_file(&provenance).expect("remove installed provenance fixture");
    assert_eq!(read_install_timestamp_for_executable(&executable), None);

    let direct_prefix = root.join("usr/local");
    let direct_executable = direct_prefix.join("bin/ramshared");
    let direct_timestamp = direct_prefix.join("share/ramshared/INSTALL_TIMESTAMP");
    fs::create_dir_all(direct_timestamp.parent().expect("timestamp parent"))
        .expect("create direct install metadata directory");
    fs::create_dir_all(direct_executable.parent().expect("direct bin"))
        .expect("create direct executable directory");
    fs::write(&direct_executable, b"direct installed binary").expect("write direct binary");
    let direct_daemon = direct_prefix.join("bin/ramsharedd");
    fs::write(&direct_daemon, b"direct installed daemon").expect("write direct daemon");
    fs::write(&direct_timestamp, "2026-09-27T18:04:05Z\n").expect("write direct install timestamp");
    assert_eq!(
        read_install_timestamp_for_executable(&direct_executable).as_deref(),
        Some("2026-09-27T18:04:05Z")
    );
    assert_eq!(
        direct_install_timestamp_path(Path::new("/usr/local/bin/ramshared")),
        Some(PathBuf::from(
            "/usr/local/share/ramshared/INSTALL_TIMESTAMP"
        ))
    );
    assert_eq!(
        direct_install_metadata_path(Path::new("/usr/local/bin/ramshared")),
        Some(PathBuf::from(
            "/usr/local/share/ramshared/INSTALL_METADATA.json"
        ))
    );
    assert!(
        format_installed_identity_line(
            "direct /usr/local",
            Some(&read_direct_install_identity(&direct_executable))
        )
        .contains("Installed direct /usr/local: identity unknown · 2026-09-27 18:04:05 UTC")
    );
    let legacy_record = format!(
        "{{\"schema_version\":\"{DIRECT_INSTALL_METADATA_V1}\",\"version\":\"0.15.0\",\"source_commit\":\"{release_commit}\",\"source_tree_state\":\"dirty\",\"installed_at_utc\":\"2026-09-27T18:04:05Z\"}}"
    );
    fs::write(
        direct_install_metadata_path(&direct_executable).expect("metadata path"),
        &legacy_record,
    )
    .expect("write legacy direct install identity metadata");
    assert!(
        format_installed_identity_line(
            "direct /usr/local",
            Some(&read_direct_install_identity(&direct_executable))
        )
        .contains("identity unknown · 2026-09-27 18:04:05 UTC")
    );

    let cli_sha256 = sha256_file(&direct_executable).expect("hash direct executable");
    let daemon_sha256 = sha256_file(&direct_daemon).expect("hash direct daemon");
    fs::write(
        direct_install_metadata_path(&direct_executable).expect("metadata path"),
        format!(
            "{{\"schema_version\":\"{DIRECT_INSTALL_METADATA_V2}\",\"version\":\"0.15.0\",\"source_commit\":\"{release_commit}\",\"source_tree_state\":\"dirty\",\"installed_at_utc\":\"2026-09-27T18:04:05Z\",\"cli_sha256\":\"{cli_sha256}\",\"daemon_sha256\":\"{daemon_sha256}\"}}"
        ),
    )
    .expect("write hash-bound direct install identity metadata");
    let direct_identity = read_installed_identity(&direct_executable, InstallKind::Direct)
        .expect("read direct installed identity");
    assert!(
        format_installed_identity_line("direct /usr/local", Some(&direct_identity)).contains(
            "Installed direct /usr/local: v0.15.0 · abcdef01 (dirty) · 2026-09-27 18:04:05 UTC"
        )
    );
    fs::write(&direct_daemon, b"replaced daemon bytes").expect("replace direct daemon");
    assert!(
        format_installed_identity_line(
            "direct /usr/local",
            Some(&read_direct_install_identity(&direct_executable))
        )
        .contains("identity unknown · 2026-09-27 18:04:05 UTC")
    );

    assert!(is_utc_timestamp("2024-02-29T23:59:59Z"));
    assert!(!is_utc_timestamp("2026-02-29T12:00:00Z"));
    assert!(!is_utc_timestamp("2026-02-31T12:00:00Z"));
    assert!(!is_utc_timestamp("2026-09-27T25:00:00Z"));

    fs::remove_dir_all(&root).expect("remove release metadata fixture");
}

/// A direct entry that is a link into a sealed release must report that
/// release's identity. The sealed installer never writes the direct
/// metadata sidecar, so without this the header shows "identity unknown"
/// for a binary the next line identifies — the same file, twice.
#[cfg(unix)]
#[test]
fn direct_entry_linked_into_release_reports_release_identity() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-direct-link-identity-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create release bin fixture");
    let executable = bin.join("ramshared");
    let release_commit = "abcdef0123456789abcdef0123456789abcdef01";
    fs::write(root.join("RELEASE_VERSION"), "v0.14.1\n").expect("write release version");
    fs::write(root.join("SOURCE_COMMIT"), format!("{release_commit}\n"))
        .expect("write source commit");
    fs::write(root.join("SOURCE_TREE_STATE"), "clean\n").expect("write tree state");
    fs::write(&executable, b"release binary").expect("write release executable");
    fs::write(bin.join("ramsharedd"), b"release daemon").expect("write release daemon");
    fs::write(
        root.join("INSTALL_PROVENANCE.json"),
        format!(
            "{{\"schema_version\":\"{INSTALLED_PROVENANCE_V2}\",\"source_commit\":\"{release_commit}\",\"source_tree_state\":\"clean\",\"installed_at_utc\":\"2026-09-27T17:23:45Z\"}}"
        ),
    )
    .expect("write installed provenance fixture");
    seal_release_fixture(&root);

    let direct_prefix = root.join("usr/local");
    let direct_bin = direct_prefix.join("bin");
    fs::create_dir_all(&direct_bin).expect("create direct bin fixture");
    let direct_executable = direct_bin.join("ramshared");
    std::os::unix::fs::symlink(&executable, &direct_executable)
        .expect("link direct entry into the release");

    // The sealed installer writes no direct sidecar, which is the state
    // that used to read as unidentified.
    assert!(
        direct_install_metadata_path(&direct_executable).is_none_or(|path| !path.exists()),
        "fixture must not carry a direct metadata sidecar"
    );
    let linked = read_direct_install_identity(&direct_executable);
    assert_eq!(linked.version.as_deref(), Some("0.14.1"));
    assert_eq!(linked.source_commit.as_deref(), Some(release_commit));
    assert_eq!(linked.source_tree_state.as_deref(), Some("clean"));
    assert_eq!(
        linked.installed_at_utc.as_deref(),
        Some("2026-09-27T17:23:45Z")
    );
    let rendered = format_installed_identity_line("direct /usr/local", Some(&linked));
    assert!(
        rendered.contains(
            "Installed direct /usr/local: v0.14.1 · abcdef01 (clean) · 2026-09-27 17:23:45 UTC"
        ),
        "linked direct entry must carry the release identity, got: {rendered}"
    );
    assert!(
        !rendered.contains("identity unknown"),
        "a linked direct entry must never read as unidentified, got: {rendered}"
    );

    // A direct entry that is a real file with no record of its own and no
    // release tree around it is still unidentified. It has to live outside
    // the release root — `parent().parent()` is what the release reader
    // walks up, so a file one level under the release root would find it.
    let standalone_prefix = root.join("elsewhere");
    let standalone = standalone_prefix.join("bin/ramshared");
    fs::create_dir_all(standalone.parent().expect("standalone parent"))
        .expect("create standalone fixture");
    fs::write(&standalone, b"standalone binary").expect("write standalone binary");
    assert!(
        format_installed_identity_line(
            "direct /usr/local",
            Some(&read_direct_install_identity(&standalone))
        )
        .contains("identity unknown")
    );

    fs::remove_dir_all(&root).expect("remove direct link fixture");
}

#[test]
fn agreed_installs_collapse_to_one_line_and_divergent_ones_do_not() {
    let image = |version: &str, sha: &str| InstalledIdentity {
        executable: PathBuf::from("/usr/local/bin/ramshared"),
        version: Some(version.to_string()),
        source_commit: Some("abcdef0123456789abcdef0123456789abcdef01".to_string()),
        source_tree_state: Some("clean".to_string()),
        installed_at_utc: Some("2026-09-27T17:23:45Z".to_string()),
        executable_sha256: Some(sha.to_string()),
    };
    let one = image("0.14.1", &"a".repeat(64));
    let same = image("0.14.1", &"a".repeat(64));
    let other = image("0.15.0", &"b".repeat(64));

    // Same image at both named paths is one install and one line. Naming a
    // path per line is what made a healthy install read as two half-installs.
    let agreed = format_installed_identity_lines(&[
        ("direct /usr/local", Some(&one)),
        ("active /opt/ramshared/current", Some(&same)),
    ]);
    assert_eq!(agreed.len(), 1, "agreed installs must collapse: {agreed:?}");
    assert_eq!(
        agreed[0],
        "Installed: v0.14.1 · abcdef01 (clean) · 2026-09-27 17:23:45 UTC"
    );

    // A different image at one of the paths is the news, so the paths are
    // named and each keeps its own line.
    let diverged = format_installed_identity_lines(&[
        ("direct /usr/local", Some(&one)),
        ("active /opt/ramshared/current", Some(&other)),
    ]);
    assert_eq!(
        diverged.len(),
        2,
        "divergent installs must expand: {diverged:?}"
    );
    assert!(diverged[0].starts_with("Installed direct /usr/local: "));
    assert!(diverged[1].starts_with("Installed active /opt/ramshared/current: "));
    assert!(diverged[0].contains("v0.14.1"));
    assert!(diverged[1].contains("v0.15.0"));

    // A path that is absent is the news even when the present one is
    // identified: it is named rather than silently dropped.
    let partial = format_installed_identity_lines(&[
        ("direct /usr/local", Some(&one)),
        ("active /opt/ramshared/current", None),
    ]);
    assert_eq!(
        partial,
        vec![
            "Installed: v0.14.1 · abcdef01 (clean) · 2026-09-27 17:23:45 UTC".to_string(),
            "Missing: active /opt/ramshared/current".to_string(),
        ]
    );

    // Nothing identified: one line, naming what was looked for.
    let absent = format_installed_identity_lines(&[
        ("direct /usr/local", None),
        ("active /opt/ramshared/current", None),
    ]);
    assert_eq!(
        absent,
        vec!["Installed: not found (direct /usr/local, active /opt/ramshared/current)".to_string()]
    );
}

#[test]
fn running_install_status_reports_the_operator_answer_not_the_path() {
    // The question is whether the running process is the installed binary.
    // Which named path it matches is the install block's job.
    assert_eq!(
        format_running_identity_line(
            "0.15.0",
            "abcdef0123456789abcdef0123456789abcdef01",
            "clean",
            RunningInstallStatus::InstalledBoth
        ),
        "Running: v0.15.0 · abcdef01 (clean) · matches the install"
    );
    assert_eq!(
        format_running_identity_line(
            "0.15.0",
            "abcdef0123456789abcdef0123456789abcdef01",
            "clean",
            RunningInstallStatus::UpdatePendingBoth
        ),
        "Running: v0.15.0 · abcdef01 (clean) · restart pending"
    );
}

fn seal_release_fixture(root: &Path) {
    let mut paths = vec![
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ];
    if root.join("INPUT_BUNDLE_SHA256SUMS").is_file() {
        paths.push("INPUT_BUNDLE_SHA256SUMS");
    }
    let mut entries = paths
        .iter()
        .map(|relative| {
            let digest = sha256_file(&root.join(relative)).expect("hash release fixture");
            format!("{digest}  ./{relative}")
        })
        .collect::<Vec<_>>();
    entries.sort();
    let manifest = format!("{}\n", entries.join("\n"));
    fs::write(root.join("SHA256SUMS"), &manifest).expect("write release fixture manifest");
    let manifest_sha256 = sha256_file(&root.join("SHA256SUMS")).expect("hash manifest");
    fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{manifest_sha256}\n"),
    )
    .expect("write release fixture manifest receipt");
}

#[cfg(unix)]
#[test]
fn installed_input_manifest_identity_requires_matching_provenance() {
    use std::os::unix::fs::symlink;
    let root =
        std::env::temp_dir().join(format!("ramshared-installed-input-{}", std::process::id()));
    fs::create_dir_all(root.join("release/bin")).unwrap();
    let release = root.join("release");
    for name in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
    ] {
        fs::write(release.join(name), b"fixture").unwrap();
    }
    let input = release.join("INPUT_BUNDLE_SHA256SUMS");
    fs::write(&input, b"original bundle manifest\n").unwrap();
    let digest = sha256_file(&input).unwrap();
    let provenance = |value: &str| {
        serde_json::json!({
            "schema_version": INSTALLED_PROVENANCE_V2,
            "input_bundle_manifest_sha256": value,
        })
        .to_string()
    };
    fs::write(release.join("INSTALL_PROVENANCE.json"), provenance(&digest)).unwrap();
    seal_release_fixture(&release);
    let selector = root.join("current");
    symlink("release", &selector).unwrap();
    assert_eq!(
        observe_installed_input_bundle_digest(&selector),
        Some(digest.clone())
    );
    assert_ne!(sha256_file(&release.join("SHA256SUMS")).unwrap(), digest);
    fs::write(&input, b"changed\n").unwrap();
    assert!(observe_installed_input_bundle_digest(&selector).is_none());
    fs::write(&input, b"original bundle manifest\n").unwrap();
    fs::write(
        release.join("INSTALL_PROVENANCE.json"),
        provenance(&"f".repeat(64)),
    )
    .unwrap();
    seal_release_fixture(&release);
    assert!(observe_installed_input_bundle_digest(&selector).is_none());
    fs::remove_file(&input).unwrap();
    seal_release_fixture(&release);
    assert!(observe_installed_input_bundle_digest(&selector).is_none());
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "read-only observation requires an installed derived release"]
fn live_installed_input_manifest_identity_is_observable() {
    let selector = Path::new(PRODUCT_ROOT).join("current");
    let observed = observe_installed_input_bundle_digest(&selector);
    assert!(
        observed.is_some(),
        "installed input identity was unavailable"
    );
}

#[test]
fn executable_hash_refuses_oversized_files_before_reading() {
    use std::io::Seek;

    let root = std::env::temp_dir().join(format!(
        "ramshared-bounded-executable-hash-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create bounded hash fixture");
    let path = root.join("executable");
    fs::write(&path, b"12345").expect("write executable fixture");

    let mut oversized = fs::File::open(&path).expect("open oversized executable fixture");
    assert_eq!(sha256_open_file(&mut oversized, 4), None);
    assert_eq!(oversized.stream_position().expect("read position"), 0);

    let mut at_limit = fs::File::open(&path).expect("open exact-limit executable fixture");
    assert_eq!(
        sha256_open_file(&mut at_limit, 5),
        Some(sha256_reader(&mut b"12345".as_slice()).expect("hash exact-limit bytes"))
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn executable_hash_reader_refuses_growth_past_its_limit() {
    let mut oversized = std::io::Cursor::new(b"123456");
    assert_eq!(sha256_reader_with_limit(&mut oversized, 5), None);
    assert_eq!(
        oversized.position(),
        6,
        "read at most one byte beyond the cap"
    );

    let mut at_limit = std::io::Cursor::new(b"12345");
    assert!(sha256_reader_with_limit(&mut at_limit, 5).is_some());
}

#[cfg(unix)]
#[test]
fn release_manifest_refuses_redirected_binary_directory() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "ramshared-release-bin-redirect-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let release = root.join("release");
    fs::create_dir_all(release.join("bin")).unwrap();
    for path in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ] {
        fs::write(release.join(path), b"fixture").unwrap();
    }
    seal_release_fixture(&release);
    let selector = root.join("current");
    symlink("release", &selector).unwrap();
    assert!(
        verify_release_manifest(&selector).is_some(),
        "the normal release selector is permitted"
    );
    fs::rename(release.join("bin"), root.join("outside-bin")).unwrap();
    symlink("../outside-bin", release.join("bin")).unwrap();
    assert!(
        verify_release_manifest(&selector).is_none(),
        "a substituted bin directory is not part of the selected release"
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn pinned_release_tree_does_not_follow_replaced_root() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-release-root-replaced-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let release = root.join("release");
    fs::create_dir_all(release.join("bin")).unwrap();
    for path in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ] {
        fs::write(release.join(path), b"original").unwrap();
    }
    seal_release_fixture(&release);
    let pinned = release_tree::ReleaseTree::open(&release).unwrap();
    assert!(pinned.matches_selected(&release));
    fs::rename(&release, root.join("original-release")).unwrap();
    fs::create_dir_all(release.join("bin")).unwrap();
    assert!(
        manifest::verify_release_manifest_tree(&pinned).is_some(),
        "the retained descriptors refer to the original tree"
    );
    assert!(
        !pinned.matches_selected(&release),
        "the replaced root is no longer the selected tree"
    );
    assert!(
        verify_release_manifest(&release).is_none(),
        "a later path lookup sees the unrelated replacement"
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn pinned_release_tree_refuses_replaced_selector_to_same_release() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "ramshared-release-selector-replaced-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let release = root.join("releases/v1.2.3-4-gabcd");
    fs::create_dir_all(release.join("bin")).unwrap();
    for path in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ] {
        fs::write(release.join(path), b"selected bytes").unwrap();
    }
    seal_release_fixture(&release);

    let selector = root.join("current");
    let replacement = root.join("current.new");
    symlink("releases/v1.2.3-4-gabcd", &selector).unwrap();
    let pinned = release_tree::ReleaseTree::open(&selector).unwrap();
    assert!(pinned.matches_selected(&selector));

    symlink("releases/v1.2.3-4-gabcd", &replacement).unwrap();
    fs::rename(&replacement, &selector).unwrap();
    assert!(
        !pinned.matches_selected(&selector),
        "replacing the selector entry is an identity change even when its target bytes agree"
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn release_tree_refuses_symlinked_ancestors_and_traversing_selector_targets() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "ramshared-release-selector-unsafe-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("real/release/bin")).unwrap();

    let traversing = root.join("traversing");
    symlink("../real/release", &traversing).unwrap();
    assert!(
        release_tree::ReleaseTree::open(&traversing).is_none(),
        "a selected symlink target may not leave its parent through .."
    );

    symlink("real", root.join("linked-parent")).unwrap();
    assert!(
        release_tree::ReleaseTree::open(&root.join("linked-parent/release")).is_none(),
        "a parent path component may not redirect selector ancestry"
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn pinned_release_tree_does_not_follow_replaced_bin() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "ramshared-release-bin-replaced-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("bin")).unwrap();
    for path in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ] {
        fs::write(root.join(path), b"original").unwrap();
    }
    seal_release_fixture(&root);
    let pinned = release_tree::ReleaseTree::open(&root).unwrap();
    assert!(pinned.matches_selected(&root));
    fs::rename(root.join("bin"), root.join("prior-bin")).unwrap();
    symlink("prior-bin", root.join("bin")).unwrap();
    assert!(!pinned.matches_selected(&root));
    fs::remove_file(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/ramshared"), b"replacement").unwrap();
    assert!(manifest::verify_release_manifest_tree(&pinned).is_some());
    assert!(
        !pinned.matches_selected(&root),
        "the replaced bin is no longer the selected directory"
    );
    assert!(verify_release_manifest(&root).is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn installed_manifest_rejects_ambiguous_paths() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-ambiguous-manifest-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("bin")).unwrap();
    for path in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INSTALL_PROVENANCE.json",
    ] {
        fs::write(root.join(path), b"fixture").unwrap();
    }
    seal_release_fixture(&root);
    assert!(verify_release_manifest(&root).is_some());
    let original = fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    let digest = sha256_file(&root.join("bin/ramshared")).unwrap();
    for path in [
        "bin//ramshared",
        "bin/./ramshared",
        "bin/ramshared/",
        "bin/../ramshared",
        "bin/ramshared\0suffix",
        "bin/ramshared\r",
    ] {
        fs::write(
            root.join("SHA256SUMS"),
            format!("{original}{digest}  ./{path}\n"),
        )
        .unwrap();
        let receipt = sha256_file(&root.join("SHA256SUMS")).unwrap();
        fs::write(
            root.join("INSTALLED_MANIFEST_SHA256"),
            format!("{receipt}\n"),
        )
        .unwrap();
        assert!(
            verify_release_manifest(&root).is_none(),
            "manifest accepted ambiguous path {path:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn verified_identity_bytes_refuse_changed_or_redirected_files() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "ramshared-verified-identity-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("bin")).unwrap();
    let path = root.join("identity");
    fs::write(&path, b"verified\n").unwrap();
    let expected = sha256_file(&path).unwrap();
    let tree = release_tree::ReleaseTree::open(&root).unwrap();
    assert_eq!(tree.verified_bytes("bin/../identity", &expected, 32), None);
    assert_eq!(tree.verified_bytes("bin/", &expected, 32), None);
    assert_eq!(
        tree.verified_bytes("identity", &expected, 32),
        Some(b"verified\n".to_vec())
    );
    assert_eq!(tree.verified_bytes("identity", &expected, 4), None);
    fs::write(&path, b"changed\n").unwrap();
    assert_eq!(tree.verified_bytes("identity", &expected, 32), None);
    let original = root.join("original");
    fs::rename(&path, &original).unwrap();
    symlink(&original, &path).unwrap();
    assert_eq!(tree.verified_bytes("identity", &expected, 32), None);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn sealed_release_tui_origin_promotes_unknown_only_under_sealed_releases_root() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-tui-origin-promote-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let release_root = root.join("releases/v0.15.0-sealed");
    let exe = release_root.join("bin/ramshared");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"sealed cli").unwrap();
    fs::write(release_root.join("bin/ramsharedd"), b"sealed daemon").unwrap();
    let commit = "1234567890abcdef1234567890abcdef12345678";
    fs::write(release_root.join("RELEASE_VERSION"), "v0.15.0-sealed\n").unwrap();
    fs::write(release_root.join("SOURCE_COMMIT"), format!("{commit}\n")).unwrap();
    fs::write(release_root.join("SOURCE_TREE_STATE"), "clean\n").unwrap();
    fs::write(
        release_root.join("INSTALL_PROVENANCE.json"),
        format!(
            "{{\"schema_version\":\"ramshared-installed-release-provenance/v2\",\"source_commit\":\"{commit}\",\"source_tree_state\":\"clean\",\"installed_at_utc\":\"2026-11-18T12:00:00Z\"}}"
        ),
    )
    .unwrap();
    seal_release_fixture(&release_root);

    assert_eq!(
        effective_tui_reexec_origin(ProcessInstallOrigin::Unknown, Some(&exe), &root),
        ProcessInstallOrigin::ActiveRelease
    );
    let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
    assert_eq!(
        effective_tui_reexec_origin(ProcessInstallOrigin::Unknown, Some(&deleted), &root),
        ProcessInstallOrigin::ActiveRelease
    );

    let traversal = release_root.join("../v0.15.0-sealed/bin/ramshared");
    assert_eq!(
        effective_tui_reexec_origin(ProcessInstallOrigin::Unknown, Some(&traversal), &root),
        ProcessInstallOrigin::Unknown
    );

    let checkout_bin = root.join("target/release/ramshared");
    assert_eq!(
        effective_tui_reexec_origin(ProcessInstallOrigin::Unknown, Some(&checkout_bin), &root),
        ProcessInstallOrigin::Unknown
    );
    assert_eq!(
        effective_tui_reexec_origin(ProcessInstallOrigin::Direct, Some(&checkout_bin), &root),
        ProcessInstallOrigin::Direct
    );

    let _ = fs::remove_dir_all(&root);
}
