// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/trusted_library/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

fn system_library() -> PathBuf {
    [
        "/usr/lib/x86_64-linux-gnu/libm.so.6",
        "/usr/lib/aarch64-linux-gnu/libm.so.6",
        "/usr/lib64/libm.so.6",
        "/lib64/libm.so.6",
    ]
    .into_iter()
    .find_map(|p| discover(Path::new(p)).ok())
    .unwrap()
}

#[test]
fn trusted_library_requires_root_and_protected_permissions() {
    assert!(check_authority(0, 0o755, false).is_ok());
    assert!(check_authority(1000, 0o755, false).is_err());
    for mode in [0o777, 0o775, 0o757, 0o666] {
        assert!(check_authority(0, mode, false).is_err());
    }
    assert!(check_authority(0, 0o777, true).is_ok());
    assert!(check_authority(1000, 0o777, true).is_err());
}

#[test]
fn trusted_library_validates_system_file_and_refuses_noncanonical_inputs() {
    let file = system_library();
    validate(&file).unwrap();
    crate::NvcompPath::validate(&file).unwrap();
    assert_eq!(discover(&file).unwrap(), file);
    for path in [
        Path::new("libcuda.so.1"),
        Path::new("/"),
        Path::new("/usr"),
        Path::new("/tmp"),
        Path::new("/ramshared_library_missing"),
    ] {
        assert!(validate(path).is_err(), "{path:?}");
    }
    let noncanonical = file
        .parent()
        .unwrap()
        .join(".")
        .join(file.file_name().unwrap());
    assert!(validate(&noncanonical).is_err());
    let ancestor_file = file.join("not_a_directory");
    assert!(discover(&ancestor_file).is_err());
    let parent = file.parent().unwrap();
    let through_parent = parent
        .join("..")
        .join(parent.file_name().unwrap())
        .join(file.file_name().unwrap());
    assert_eq!(discover(&through_parent).unwrap(), file);
    assert!(validate(&through_parent).is_err());
}

#[test]
fn trusted_library_discovery_resolves_only_protected_system_aliases() {
    // Merged-/usr distributions expose this actual protected directory alias.
    // Other layouts still execute the regular-file and refusal controls above.
    if std::fs::symlink_metadata("/lib").is_ok_and(|m| m.is_symlink()) {
        let file = system_library();
        let candidate = Path::new("/lib").join(file.strip_prefix("/usr/lib").unwrap());
        assert_eq!(discover(&candidate).unwrap(), file);
        assert!(validate(&candidate).is_err());
    }
}

#[test]
fn trusted_library_bounds_path_bytes_components_and_empty_aliases() {
    assert!(components(Path::new(&format!("/{}", "a".repeat(MAX_PATH_BYTES)))).is_err());
    assert!(components(Path::new(&format!("/{}", "a/".repeat(MAX_COMPONENTS)))).is_err());
    assert!(discover(Path::new("relative/driver.so")).is_err());
    assert!(validate(Path::new("/usr/../")).is_err());
}

#[test]
#[ignore = "requires uid 0; creates and removes only its own protected /run fixture"]
fn trusted_library_actual_root_aliases_permissions_and_limits() {
    use std::os::unix::fs::{PermissionsExt, chown, symlink};
    assert_eq!(
        std::fs::metadata("/proc/self").unwrap().uid(),
        0,
        "root fixture prerequisite"
    );
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let path = Path::new("/run").join(format!("ramshared-library-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let root = Fixture(path);
    std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = root.0.join("fixture.so");
    std::fs::copy(system_library(), &file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    validate(&file).unwrap();
    let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    // SAFETY: owned terminated fixture path; the loader validates it normally.
    let library = crate::driver::Lib(unsafe { crate::loader::open(path.as_ptr()) });
    assert!(!library.0.is_null(), "{}", crate::loader::error());
    assert!(!unsafe { crate::loader::sym(library.0, c"cos".as_ptr()) }.is_null());
    drop(library);
    let alias = root.0.join("alias.so");
    symlink("fixture.so", &alias).unwrap();
    assert_eq!(discover(&alias).unwrap(), file);
    assert!(validate(&alias).is_err());
    let absolute = root.0.join("absolute.so");
    symlink(&alias, &absolute).unwrap();
    assert_eq!(discover(&absolute).unwrap(), file);
    let nested = root.0.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let nested_file = nested.join("regular.so");
    std::fs::copy(&file, &nested_file).unwrap();
    crate::NvcompPath::validate(&nested_file).unwrap();
    symlink("../fixture.so", nested.join("parent.so")).unwrap();
    assert_eq!(discover(&nested.join("parent.so")).unwrap(), file);
    for path in [&file, &nested] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
        if path == &nested {
            assert!(
                crate::NvcompPath::validate(&nested_file).is_err(),
                "nvCOMP accepted a root-owned leaf beneath a writable ancestor"
            );
        }
        let target = if path == &file {
            file.clone()
        } else {
            nested.join("parent.so")
        };
        assert!(discover(&target).is_err());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        chown(path, Some(65_534), None).unwrap();
        assert!(discover(&target).is_err());
        chown(path, Some(0), None).unwrap();
    }
    chown(&alias, Some(65_534), None).unwrap(); // chown follows this owned alias.
    assert!(discover(&alias).is_err());
    chown(&file, Some(0), None).unwrap();
    let looped = root.0.join("loop.so");
    symlink("loop.so", &looped).unwrap();
    assert!(discover(&looped).unwrap_err().contains("alias budget"));
    let missing = root.0.join("missing.so");
    symlink("absent.so", &missing).unwrap();
    assert!(discover(&missing).is_err());
    let socket_path = root.0.join("socket");
    let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    assert!(discover(&socket_path).is_err());
    let mut deep = root.0.clone();
    for _ in 0..250 {
        deep.push("d");
        std::fs::create_dir(&deep).unwrap();
    }
    std::fs::write(deep.join("target.so"), b"not executable").unwrap();
    symlink(deep.join("target.so"), root.0.join("deep.so")).unwrap();
    assert!(
        discover(&root.0.join("deep.so"))
            .unwrap_err()
            .contains("component budget")
    );
    println!("ROOT_LIBRARY_PATH_CONTROLS=PASS");
}
