// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/memory_observation/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::{MetadataExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn fixture_cgroups(root: &Path, membership: &str, sampled: u64) -> io::Result<Vec<CgroupSample>> {
    visible_cgroups_inner(root, membership, sampled, false)
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ramshared-cgroup-observation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn group(&self, relative: &str, max: &str, current: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("memory.max"), max).unwrap();
        std::fs::write(path.join("memory.current"), current).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn hierarchy_includes_root_and_every_visible_ancestor() {
    let fixture = Fixture::new();
    fixture.group("", "max\n", "123\n");
    fixture.group("parent", "500\n", "100\n");
    fixture.group("parent/child", "300\n", "50\n");
    assert!(visible_cgroups(&fixture.0, "0::/parent/child\n", 1234).is_err());
    let samples = fixture_cgroups(&fixture.0, "0::/parent/child\n", 1234).unwrap();
    assert_eq!(samples.len(), 3);
    assert_eq!(samples[0].limit_bytes, None);
    assert_eq!(samples[1].limit_bytes, Some(500));
    assert_eq!(samples[2].limit_bytes, Some(300));
    assert_eq!(samples[2].current_bytes, 50);
    for (sample, path) in samples.iter().zip(["", "parent", "parent/child"]) {
        let metadata = std::fs::metadata(fixture.0.join(path)).unwrap();
        assert_eq!(sample.identity.device, metadata.dev());
        assert_eq!(sample.identity.inode, metadata.ino());
    }
    assert!(
        samples
            .iter()
            .all(|sample| sample.sampled_at_unix_ms == 1234)
    );
}

#[test]
fn unsafe_paths_missing_metrics_and_symlinks_refuse() {
    let fixture = Fixture::new();
    fixture.group("", "max", "1");
    fixture.group("parent", "max", "2");
    for membership in [
        "",
        "0::relative",
        "0::/parent/../escape",
        "0::/parent//child",
        "0::/parent\n0::/other\n",
    ] {
        assert!(
            fixture_cgroups(&fixture.0, membership, 1).is_err(),
            "{membership}"
        );
    }
    symlink("parent", fixture.0.join("link")).unwrap();
    assert!(fixture_cgroups(&fixture.0, "0::/link", 1).is_err());
    std::fs::remove_file(fixture.0.join("parent/memory.current")).unwrap();
    assert!(fixture_cgroups(&fixture.0, "0::/parent", 1).is_err());
    symlink("../memory.current", fixture.0.join("parent/memory.current")).unwrap();
    assert!(fixture_cgroups(&fixture.0, "0::/parent", 1).is_err());
}

#[test]
#[ignore = "requires a readable Linux cgroup v2 mount"]
fn visible_host_cgroup_and_meminfo_are_readable() {
    let membership = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let groups = visible_cgroups(Path::new("/sys/fs/cgroup"), &membership, 1).unwrap();
    assert!(!groups.is_empty());
    let metadata = std::fs::metadata("/sys/fs/cgroup").unwrap();
    assert_eq!(groups[0].identity.device, metadata.dev());
    assert_eq!(groups[0].identity.inode, metadata.ino());
    eprintln!(
        "LINUX_VISIBLE_MEMORY_OBSERVATION=PASS groups={}",
        groups.len()
    );
}

#[test]
fn replaced_directory_at_same_path_changes_cgroup_identity() {
    let fixture = Fixture::new();
    fixture.group("", "max", "0");
    fixture.group("child", "100", "10");
    let before = fixture_cgroups(&fixture.0, "0::/child\n", 1).unwrap();
    std::fs::rename(fixture.0.join("child"), fixture.0.join("previous")).unwrap();
    fixture.group("child", "100", "10");
    let after = fixture_cgroups(&fixture.0, "0::/child\n", 2).unwrap();
    assert_eq!(before[0].identity, after[0].identity);
    assert_ne!(before[1].identity, after[1].identity);
}
