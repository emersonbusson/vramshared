// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_identity.rs
//! Sealed release identity — one acceptance predicate for on-disk identity files.
//!
//! A sealed release root is a small set of single-file identity documents
//! ([`SEALED_IDENTITY_FILES`]). Every consumer reads them through this module.
//!
//! **Why this module exists (DT-10).** `RELEASE_VERSION` used to be read twice,
//! with two different predicates: the running-binary identity used
//! `read_regular_line` (rejects symlinks and multi-line content), while the
//! selector release gate used `fs::read_to_string(...).trim()` (follows
//! symlinks and trims). A malformed file could therefore be refused by one
//! reader and laundered into a passing gate by the other: `"\n0.15.0\n"` is
//! refused by the strict reader, so the running identity falls back to crate
//! semver `0.15.0`, and the permissive reader then reports `0.15.0` — a match,
//! on a file neither reader had verified. Two halves of one contract must not
//! be re-derived in two places.
//!
//! The rule, applied once:
//! 1. the document must be a regular file opened without following a redirect
//!    — a symlink is not a sealed identity file, and the test runs on the
//!    descriptor the bytes are read from, not on a path looked up twice;
//! 2. the content, with at most one trailing newline stripped, is non-empty;
//! 3. it contains no embedded newline, carriage return or NUL;
//! 4. it carries no surrounding whitespace and is at most 4096 bytes.
//!
//! Rules 3 and 4 together mean: exactly one non-empty, whitespace-trimmed line.
//! A missing final newline is tolerated — that is a filesystem encoding detail,
//! not an identity defect.

use std::fs;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;

use serde_json::Value;

/// Identity documents a sealed release root carries as single files.
///
/// Shared by the install-fingerprint sweep and the manifest re-verification so
/// the name set cannot drift between the two.
pub const SEALED_IDENTITY_FILES: &[&str] = &[
    "RELEASE_VERSION",
    "SOURCE_COMMIT",
    "SOURCE_TREE_STATE",
    "INSTALL_PROVENANCE.json",
];

/// The bundle identity file `scripts/package/build-linux-bundle.sh` writes as
/// `git describe --always --dirty --tags`.
pub const RELEASE_VERSION_FILE: &str = "RELEASE_VERSION";

/// The manifest seal under a release root — the `SHA256SUMS` cover over the
/// identity documents and product binaries.
pub const SHA256SUMS_FILE: &str = "SHA256SUMS";

/// Maximum on-disk size of a single-line release identity, including newline.
pub(crate) const MAX_IDENTITY_LINE_BYTES: usize = 4096;

/// Bounds shared by installed manifest, unit and metadata observations.
pub(crate) const MAX_SEALED_TEXT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SEALED_JSON_BYTES: usize = 64 * 1024;

/// True only for a regular file at `path`.
///
/// [`fs::symlink_metadata`] so a symlink is refused rather than followed: a
/// sealed identity document is the file itself, not a redirect to one.
///
/// This is the predicate for callers that only need to ask. Readers go through
/// [`read_regular_text`] and its siblings, which hold the check and the read on
/// one descriptor — asking this and then opening the path leaves a window.
pub fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
}

/// Open a regular file without following its final component, or refuse it.
///
/// The open is `O_NOFOLLOW` and the regular-file test runs on the descriptor
/// returned to the caller, so the file this function stats is the file the
/// caller can read. The previous shape asked [`is_regular_file`] and then
/// re-opened the path: a document replaced in between would be read as content
/// after having been refused as a redirect, which is the laundering this
/// module exists to prevent. No path re-lookup sits between the check and the
/// bytes. Nonblocking open prevents a FIFO from waiting for a writer before
/// its type can be rejected; it does not cancel a stalled filesystem call.
pub(crate) fn open_regular_no_follow(path: &Path) -> Option<File> {
    use rustix::fs::{Mode, OFlags, open};

    let file = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    regular_descriptor(file)
}

/// Open a regular file through an intentional selector or process link.
/// Nonblocking open lets the type check reject a FIFO without a writer.
pub(crate) fn open_regular_follow(path: &Path) -> Option<File> {
    use rustix::fs::{Mode, OFlags, open};

    let file = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    regular_descriptor(file)
}

/// Open one named regular file beneath an already held directory.
pub(crate) fn open_regular_at(directory: &File, name: &str) -> Option<File> {
    use rustix::fs::{Mode, OFlags, openat};

    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\0']) {
        return None;
    }
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    regular_descriptor(file)
}

fn regular_descriptor(file: OwnedFd) -> Option<File> {
    use rustix::fs::{FileType, fstat};

    let stat = fstat(&file).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return None;
    }
    Some(File::from(file))
}

/// Read from the checked descriptor, refusing bytes beyond `limit`.
pub(crate) fn read_regular_bytes(path: &Path, limit: usize) -> Option<Vec<u8>> {
    read_file_bytes(open_regular_no_follow(path)?, limit)
}

/// Read a bounded document from an already validated descriptor.
pub(crate) fn read_file_bytes(file: File, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit.checked_add(1)?).ok()?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limit {
        return None;
    }
    Some(bytes)
}

/// Read a sealed identity document whose content is free-form text.
///
/// Same regular-file rule as [`read_regular_line`]; no line shape is imposed,
/// so this is what `SHA256SUMS` (a multi-line manifest) goes through. Readers
/// of that file must not open it directly — a symlinked manifest is not a
/// sealed manifest, and `sha256_file` already refuses one.
pub fn read_regular_text(path: &Path) -> Option<String> {
    String::from_utf8(read_regular_bytes(path, MAX_SEALED_TEXT_BYTES)?).ok()
}

/// Read a sealed single-line identity document under the one rule above.
pub fn read_regular_line(path: &Path) -> Option<String> {
    parse_identity_line(&read_regular_bytes(path, MAX_IDENTITY_LINE_BYTES)?)
}

/// Apply the shared line rule to the exact bytes already read by a caller.
pub(crate) fn parse_identity_line(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_IDENTITY_LINE_BYTES {
        return None;
    }
    let contents = std::str::from_utf8(bytes).ok()?;
    let line = contents.strip_suffix('\n').unwrap_or(contents);
    if line.is_empty() || line.contains(['\n', '\r', '\0']) || line.trim() != line {
        return None;
    }
    Some(line.to_string())
}

/// Read a sealed identity document that is JSON, under the same regular-file rule.
pub fn read_regular_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&read_regular_bytes(path, MAX_SEALED_JSON_BYTES)?).ok()
}

/// `RELEASE_VERSION` at `release_root` — the single reader for the bundle identity.
///
/// Both the running-binary identity and the selector release gate go through
/// here, so a file one side accepts is a file the other side accepts. Callers
/// that *display* a normalized form may strip the `v` prefix on top; callers
/// that *compare* identities compare this raw string, because `git describe`
/// output is not crate semver (DT-10).
pub fn read_release_version(release_root: &Path) -> Option<String> {
    read_regular_line(&release_root.join(RELEASE_VERSION_FILE))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ramshared-release-identity-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn read_regular_line_accepts_one_trimmed_line_with_or_without_final_newline() {
        let dir = temp_dir("accept");
        write(&dir.join("with-nl"), "v0.15.0\n");
        write(&dir.join("no-nl"), "v0.15.0");
        assert_eq!(
            read_regular_line(&dir.join("with-nl")).as_deref(),
            Some("v0.15.0")
        );
        assert_eq!(
            read_regular_line(&dir.join("no-nl")).as_deref(),
            Some("v0.15.0")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_line_refuses_malformed_identity_documents() {
        // The DT-10 laundering shapes. Each of these is `trim()`-able into a
        // plausible identity, which is exactly how the permissive reader used
        // to manufacture a match the strict reader had refused.
        let dir = temp_dir("malformed");
        for (name, contents) in [
            ("leading-newline", "\n0.15.0\n"),
            ("multi-line", "0.15.0\n0.15.1\n"),
            ("leading-space", " 0.15.0\n"),
            ("trailing-space", "0.15.0 \n"),
            ("interior-tab", "0.15.0\t\n"),
            ("carriage-return", "0.15.0\r\n"),
            ("embedded-nul", "0.15.0\0extra\n"),
            ("empty", "\n"),
            ("blank", ""),
        ] {
            let path = dir.join(name);
            write(&path, contents);
            assert_eq!(
                read_regular_line(&path),
                None,
                "{name} must be refused, not trimmed into a match"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_identity_line_matches_shell_utf8_and_trim_contract() {
        let whitespace = [
            '\u{0085}', '\u{00a0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}',
            '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}',
            '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
        ];
        for character in whitespace {
            let leading = format!("{character}identity");
            let trailing = format!("identity{character}");
            let interior = format!("identity{character}inside");
            assert_eq!(parse_identity_line(leading.as_bytes()), None);
            assert_eq!(parse_identity_line(trailing.as_bytes()), None);
            assert_eq!(parse_identity_line(interior.as_bytes()), Some(interior));
        }

        // Rust's trim predicate does not classify these C0 separators as
        // whitespace; the shell reader uses the C locale for the same result.
        assert_eq!(
            parse_identity_line(b"\x1cidentity\x1f"),
            Some("\x1cidentity\x1f".into())
        );
        assert_eq!(parse_identity_line(b"\xffidentity"), None);
        assert_eq!(parse_identity_line(b"\xc0\xafidentity"), None);
    }

    #[test]
    fn read_regular_line_bounds_identity_bytes() {
        let dir = temp_dir("bounded-line");
        let path = dir.join("identity");
        let limit = "x".repeat(4096);
        write(&path, &limit);
        assert_eq!(read_regular_line(&path).as_deref(), Some(limit.as_str()));
        write(&path, &"x".repeat(4097));
        assert_eq!(read_regular_line(&path), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_text_bounds_manifest_bytes() {
        let dir = temp_dir("bounded-text");
        let path = dir.join("manifest");
        let limit = 1024 * 1024;
        write(&path, &"x".repeat(limit));
        assert_eq!(read_regular_text(&path).unwrap().len(), limit);
        write(&path, &"x".repeat(limit + 1));
        assert!(read_regular_text(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_json_bounds_metadata_bytes() {
        let dir = temp_dir("bounded-json");
        let path = dir.join("metadata");
        let limit = 64 * 1024;
        write(&path, &format!("\"{}\"", "x".repeat(limit - 2)));
        assert!(read_regular_json(&path).is_some());
        write(&path, &format!("\"{}\"", "x".repeat(limit - 1)));
        assert!(read_regular_json(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn opened_identity_descriptor_survives_path_replacement() {
        let dir = temp_dir("retained-descriptor");
        let path = dir.join("identity");
        write(&path, "original\n");
        let mut opened = open_regular_no_follow(&path).expect("open original regular file");
        fs::rename(&path, dir.join("previous")).unwrap();
        write(&path, "replacement\n");
        let mut value = String::new();
        opened.read_to_string(&mut value).unwrap();
        assert_eq!(value, "original\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_line_refuses_fifo_without_waiting_for_writer() {
        use rustix::fs::{CWD, Mode, OFlags, mkfifoat, open};
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = temp_dir("fifo");
        let fifo = dir.join("identity");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let path = fifo.clone();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(read_regular_line(&path));
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        if result.is_err() {
            // Unblock a regressed blocking reader before failing the test.
            if open(&fifo, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()).is_ok() {
                let _ = worker.join();
            }
            panic!("FIFO read waited for a writer");
        }
        worker.join().unwrap();
        assert_eq!(result.unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_line_refuses_symlinks_directories_and_missing_files() {
        let dir = temp_dir("non-regular");
        let target = dir.join("target");
        write(&target, "v0.15.0\n");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            read_regular_line(&link),
            None,
            "a symlink is not a sealed identity file"
        );
        assert_eq!(
            read_regular_line(&dir),
            None,
            "a directory is not one either"
        );
        assert_eq!(read_regular_line(&dir.join("absent")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_release_version_is_the_single_reader_for_the_bundle_identity() {
        let dir = temp_dir("release-version");
        let file = dir.join(RELEASE_VERSION_FILE);
        write(&file, "v0.14.1-386-gdffa5ea2-dirty\n");
        assert_eq!(
            read_release_version(&dir).as_deref(),
            Some("v0.14.1-386-gdffa5ea2-dirty"),
            "git-describe output is the sealed identity verbatim"
        );
        // A malformed file yields None — never a trimmed stand-in — so both
        // call sites see the same refusal and neither can fall back to crate
        // semver against a file the other side still reads.
        write(&file, "\nv0.14.1-386-gdffa5ea2-dirty\n");
        assert_eq!(read_release_version(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_text_refuses_symlinks_but_keeps_multi_line_manifests() {
        let dir = temp_dir("text");
        let target = dir.join("SHA256SUMS");
        write(&target, "00  bin/ramshared\n11  RELEASE_VERSION\n");
        let link = dir.join("link-sums");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            read_regular_text(&link),
            None,
            "a symlinked manifest is not a sealed manifest"
        );
        assert_eq!(
            read_regular_text(&target).map(|text| text.lines().count()),
            Some(2),
            "a real multi-line manifest is read in full"
        );
        assert_eq!(read_regular_text(&dir.join("absent")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_regular_json_refuses_symlinks_and_non_json() {
        let dir = temp_dir("json");
        let target = dir.join("target.json");
        write(&target, r#"{"schema_version":"v1"}"#);
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_regular_json(&link), None);
        write(&dir.join("bad.json"), "not json");
        assert_eq!(read_regular_json(&dir.join("bad.json")), None);
        assert!(read_regular_json(&target).is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
