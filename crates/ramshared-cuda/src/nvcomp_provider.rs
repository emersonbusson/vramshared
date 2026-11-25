// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/nvcomp_provider.rs
// Optional hardened nvCOMP provider (SPEC gpu-vram-hardware-compression ITEM-4).
//
// The provider never loads a relative path, a symlink, or a shared object that
// another user can rewrite. nvCOMP does not verify CRC32 before decompressing,
// so the embedded PTX pre-decode gate (represented here by the deterministic
// reference CRC32) always runs before the backend call.

use std::path::{Component, Path, PathBuf};

use ramshared_vram::codec::crc32;
use ramshared_vram::device_codec::{CodecError, PAGE_BYTES};
use ramshared_vram::slab_pool::VramSlabTicket;

pub const DEFAULT_NVCOMP_PATH: &str = "/usr/lib/x86_64-linux-gnu/libnvcomp.so.4";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NvcompError {
    RelativePath,
    Symlink,
    WritableByOthers,
    NotRootOwned,
    UntrustedPath(String),
    Missing,
    LoadFailed(String),
    MissingSymbol(&'static str),
}

impl std::fmt::Display for NvcompError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelativePath => formatter.write_str("nvcomp path must be absolute"),
            Self::Symlink => formatter.write_str("nvcomp path must not be a symlink"),
            Self::WritableByOthers => {
                formatter.write_str("nvcomp path is writable by another user")
            }
            Self::NotRootOwned => formatter.write_str("nvcomp path is not owned by uid 0"),
            Self::UntrustedPath(error) => {
                write!(formatter, "nvcomp path authority refused: {error}")
            }
            Self::Missing => formatter.write_str("nvcomp path does not exist"),
            Self::LoadFailed(reason) => write!(formatter, "nvcomp dlopen failed: {reason}"),
            Self::MissingSymbol(symbol) => {
                write!(formatter, "nvcomp required symbol missing: {symbol}")
            }
        }
    }
}

impl std::error::Error for NvcompError {}

/// A validated absolute shared-object path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NvcompPath {
    path: PathBuf,
}

impl NvcompPath {
    /// Production validation: absolute, canonical (no parent symlink), root
    /// owned and not writable by group/other.
    pub fn validate(path: &Path) -> Result<Self, NvcompError> {
        Self::validate_with_owner(path, true)
    }

    /// The `require_root == false` form exists for the unit fixtures that
    /// cannot create a uid-0 file; production callers use [`Self::validate`].
    fn validate_with_owner(path: &Path, require_root: bool) -> Result<Self, NvcompError> {
        #[cfg(not(unix))]
        let _ = require_root;
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(NvcompError::RelativePath);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|_| NvcompError::Missing)?;
        if metadata.file_type().is_symlink() {
            return Err(NvcompError::Symlink);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let canonical = std::fs::canonicalize(path).map_err(|_| NvcompError::Missing)?;
            if canonical != path {
                return Err(NvcompError::Symlink);
            }
            if require_root && metadata.uid() != 0 {
                return Err(NvcompError::NotRootOwned);
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(NvcompError::WritableByOthers);
            }
            if require_root {
                crate::trusted_library::validate(path).map_err(NvcompError::UntrustedPath)?;
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

/// The optional decompressor. The real implementation calls
/// `nvcompBatchedLZ4DecompressAsync`; tests inject a deterministic backend.
pub trait NvcompBackend {
    fn decompress(&mut self, payload: &[u8], out: &mut [u8; PAGE_BYTES]) -> Result<(), String>;
}

/// C ABI entry point exported by `libnvcomp.so.4` for batched LZ4 decompression.
pub type NvcompBatchedLz4DecompressAsyncFn = unsafe extern "C" fn(
    device_compressed_ptrs: *const *const u8,
    device_compressed_bytes: *const usize,
    device_uncompressed_bytes: *const usize,
    device_actual_uncompressed_bytes: *mut usize,
    batch_size: usize,
    device_temp_ptr: *mut core::ffi::c_void,
    temp_bytes: usize,
    device_uncompressed_ptrs: *const *mut u8,
    device_statuses: *mut i32,
    stream: *mut core::ffi::c_void,
) -> i32;

/// Dynamically loaded `libnvcomp.so` backend backed by [`crate::loader`].
pub struct DlopenNvcompBackend {
    _lib: crate::driver::Lib,
    decompress_async: NvcompBatchedLz4DecompressAsyncFn,
}

impl DlopenNvcompBackend {
    /// Opens a validated [`NvcompPath`] via [`crate::loader::open`] and resolves
    /// `nvcompBatchedLZ4DecompressAsync`.
    pub fn open(path: &NvcompPath) -> Result<Self, NvcompError> {
        let c_path =
            std::ffi::CString::new(path.as_path().to_string_lossy().as_bytes()).map_err(|e| {
                NvcompError::UntrustedPath(format!("nvcomp path contains NUL byte: {e}"))
            })?;
        // SAFETY: c_path is a valid NUL-terminated path validated by NvcompPath.
        let handle = unsafe { crate::loader::open(c_path.as_ptr()) };
        if handle.is_null() {
            return Err(NvcompError::LoadFailed(crate::loader::error()));
        }
        let lib = crate::driver::Lib(handle);
        // SAFETY: lib.0 is a live dlopen handle and the symbol name is NUL-terminated.
        let raw_sym =
            unsafe { crate::loader::sym(lib.0, c"nvcompBatchedLZ4DecompressAsync".as_ptr()) };
        if raw_sym.is_null() {
            return Err(NvcompError::MissingSymbol(
                "nvcompBatchedLZ4DecompressAsync",
            ));
        }
        // SAFETY: raw_sym is non-null and matches the nvCOMP batched LZ4 C ABI.
        let decompress_async: NvcompBatchedLz4DecompressAsyncFn =
            unsafe { core::mem::transmute(raw_sym) };
        Ok(Self {
            _lib: lib,
            decompress_async,
        })
    }

    #[cfg(test)]
    fn from_raw_fn(decompress_async: NvcompBatchedLz4DecompressAsyncFn) -> Self {
        Self {
            _lib: crate::driver::Lib(core::ptr::null_mut()),
            decompress_async,
        }
    }
}

impl NvcompBackend for DlopenNvcompBackend {
    fn decompress(&mut self, payload: &[u8], out: &mut [u8; PAGE_BYTES]) -> Result<(), String> {
        let src_ptr = payload.as_ptr();
        let src_len = payload.len();
        let expected_len = PAGE_BYTES;
        let mut actual_len = 0usize;
        let dst_ptr = out.as_mut_ptr();
        let mut status = -1i32;
        // SAFETY: pointers reference live stack/slice memory for the synchronous 1-page call.
        let rc = unsafe {
            (self.decompress_async)(
                &raw const src_ptr,
                &raw const src_len,
                &raw const expected_len,
                &raw mut actual_len,
                1,
                core::ptr::null_mut(),
                0,
                &raw const dst_ptr,
                &raw mut status,
                core::ptr::null_mut(),
            )
        };
        if rc != 0 || status != 0 || actual_len != PAGE_BYTES {
            return Err(format!(
                "nvcompBatchedLZ4DecompressAsync failed: rc={rc} status={status} actual_len={actual_len}"
            ));
        }
        Ok(())
    }
}

pub struct NvcompDynamicCodec<B: NvcompBackend> {
    path: NvcompPath,
    backend: B,
    decompress_calls: u64,
    crc_refusals: u64,
}

impl NvcompDynamicCodec<DlopenNvcompBackend> {
    /// Validates `path` and opens `libnvcomp.so` via [`DlopenNvcompBackend::open`].
    pub fn open_dlopen(path: &Path) -> Result<Self, NvcompError> {
        let validated = NvcompPath::validate(path)?;
        let backend = DlopenNvcompBackend::open(&validated)?;
        Ok(Self::new(validated, backend))
    }
}

impl<B: NvcompBackend> NvcompDynamicCodec<B> {
    pub fn new(path: NvcompPath, backend: B) -> Self {
        Self {
            path,
            backend,
            decompress_calls: 0,
            crc_refusals: 0,
        }
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub fn decompress_calls(&self) -> u64 {
        self.decompress_calls
    }

    pub fn crc_refusals(&self) -> u64 {
        self.crc_refusals
    }

    /// Mandatory gate: verify the compressed bytes before calling nvCOMP.
    pub fn decompress_page(
        &mut self,
        ticket: &VramSlabTicket,
        payload: &[u8],
        out: &mut [u8; PAGE_BYTES],
    ) -> Result<(), CodecError> {
        if crc32(payload, 0) != ticket.payload_crc32 {
            self.crc_refusals = self.crc_refusals.saturating_add(1);
            return Err(CodecError::CorruptPayload { page: 0 });
        }
        self.decompress_calls = self.decompress_calls.saturating_add(1);
        self.backend
            .decompress(payload, out)
            .map_err(CodecError::BackendFailure)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ramshared_vram::slab_pool::{SlabClass, SlotEncoding};

    struct MockBackend {
        calls: u64,
    }

    impl NvcompBackend for MockBackend {
        fn decompress(
            &mut self,
            _payload: &[u8],
            out: &mut [u8; PAGE_BYTES],
        ) -> Result<(), String> {
            self.calls += 1;
            out.fill(0x11);
            Ok(())
        }
    }

    fn ticket(crc: u32) -> VramSlabTicket {
        VramSlabTicket {
            frame_idx: 0,
            sub_slot: 0,
            class: SlabClass::B512,
            encoding: SlotEncoding::Lz4,
            payload_len: 4,
            payload_crc32: crc,
            epoch: 1,
        }
    }

    #[test]
    fn nvcomp_rejects_relative_and_untrusted_library_paths() {
        assert_eq!(
            NvcompPath::validate(Path::new("libnvcomp.so")),
            Err(NvcompError::RelativePath)
        );
        assert_eq!(
            NvcompPath::validate(Path::new("../libnvcomp.so")),
            Err(NvcompError::RelativePath)
        );
        let missing = std::env::temp_dir()
            .join(format!(
                "ramshared-missing-nvcomp-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("libnvcomp.so");
        assert!(!missing.exists());
        assert_eq!(NvcompPath::validate(&missing), Err(NvcompError::Missing));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let root =
                std::env::temp_dir().join(format!("ramshared-nvcomp-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let writable = root.join("libnvcomp.so");
            std::fs::write(&writable, b"x").unwrap();
            std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o666)).unwrap();
            assert_eq!(
                NvcompPath::validate_with_owner(&writable, false),
                Err(NvcompError::WritableByOthers)
            );
            let target = root.join("target.so");
            std::fs::write(&target, b"x").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            let link = root.join("link.so");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                NvcompPath::validate_with_owner(&link, false),
                Err(NvcompError::Symlink)
            );
            let clean = root.join("clean.so");
            std::fs::write(&clean, b"x").unwrap();
            std::fs::set_permissions(&clean, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(NvcompPath::validate_with_owner(&clean, false).is_ok());
            let euid = std::fs::metadata("/proc/self")
                .map(|m| m.uid())
                .unwrap_or(0);
            if euid != 0 {
                assert_eq!(NvcompPath::validate(&clean), Err(NvcompError::NotRootOwned));
            }
            let real = root.join("real");
            std::fs::create_dir(&real).unwrap();
            let parent_target = real.join("lib.so");
            std::fs::write(&parent_target, b"x").unwrap();
            std::fs::set_permissions(&parent_target, std::fs::Permissions::from_mode(0o644))
                .unwrap();
            let parent_link = root.join("parent-link");
            std::os::unix::fs::symlink(&real, &parent_link).unwrap();
            assert_eq!(
                NvcompPath::validate_with_owner(&parent_link.join("lib.so"), false),
                Err(NvcompError::Symlink)
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn nvcomp_error_display_and_path_accessor_are_total() {
        for error in [
            NvcompError::RelativePath,
            NvcompError::Symlink,
            NvcompError::WritableByOthers,
            NvcompError::NotRootOwned,
            NvcompError::UntrustedPath("bad".into()),
            NvcompError::Missing,
            NvcompError::LoadFailed("dlopen".into()),
            NvcompError::MissingSymbol("nvcompBatchedLZ4DecompressAsync"),
        ] {
            assert!(!error.to_string().is_empty());
        }
        let path = NvcompPath {
            path: PathBuf::from(DEFAULT_NVCOMP_PATH),
        };
        assert_eq!(path.as_path(), Path::new(DEFAULT_NVCOMP_PATH));
    }

    #[test]
    fn nvcomp_enforces_predecode_crc32_before_decompress() {
        let path = NvcompPath {
            path: PathBuf::from("/usr/lib/x86_64-linux-gnu/libnvcomp.so.4"),
        };
        let backend = MockBackend { calls: 0 };
        let mut codec = NvcompDynamicCodec::new(path, backend);
        let payload = [1u8; 4];
        let expected = crc32(&payload, 0);
        let mut out = [0u8; PAGE_BYTES];
        let mut good = ticket(expected);
        codec.decompress_page(&good, &payload, &mut out).unwrap();
        assert_eq!(codec.decompress_calls(), 1);
        good.payload_crc32 = expected ^ 1;
        assert!(matches!(
            codec.decompress_page(&good, &payload, &mut out),
            Err(CodecError::CorruptPayload { .. })
        ));
        assert_eq!(codec.decompress_calls(), 1);
        assert_eq!(codec.crc_refusals(), 1);
    }

    unsafe extern "C" fn fake_nvcomp_ok(
        device_compressed_ptrs: *const *const u8,
        device_compressed_bytes: *const usize,
        device_uncompressed_bytes: *const usize,
        device_actual_uncompressed_bytes: *mut usize,
        batch_size: usize,
        _device_temp_ptr: *mut core::ffi::c_void,
        _temp_bytes: usize,
        device_uncompressed_ptrs: *const *mut u8,
        device_statuses: *mut i32,
        _stream: *mut core::ffi::c_void,
    ) -> i32 {
        if batch_size != 1 {
            return -1;
        }
        // SAFETY: test harness passes valid 1-element pointer arrays.
        unsafe {
            let src =
                core::slice::from_raw_parts(*device_compressed_ptrs, *device_compressed_bytes);
            let dst = core::slice::from_raw_parts_mut(
                *device_uncompressed_ptrs,
                *device_uncompressed_bytes,
            );
            dst.fill(src.first().copied().unwrap_or(0));
            *device_actual_uncompressed_bytes = *device_uncompressed_bytes;
            *device_statuses = 0;
        }
        0
    }

    unsafe extern "C" fn fake_nvcomp_err(
        _device_compressed_ptrs: *const *const u8,
        _device_compressed_bytes: *const usize,
        _device_uncompressed_bytes: *const usize,
        device_actual_uncompressed_bytes: *mut usize,
        _batch_size: usize,
        _device_temp_ptr: *mut core::ffi::c_void,
        _temp_bytes: usize,
        _device_uncompressed_ptrs: *const *mut u8,
        device_statuses: *mut i32,
        _stream: *mut core::ffi::c_void,
    ) -> i32 {
        // SAFETY: test harness passes valid 1-element pointer arrays.
        unsafe {
            *device_actual_uncompressed_bytes = 0;
            *device_statuses = 2;
        }
        1
    }

    #[test]
    fn nvcomp_dlopen_backend_validates_symbols_and_executes_c_abi() {
        #[cfg(unix)]
        {
            for candidate in [
                "/usr/lib/x86_64-linux-gnu/libm.so.6",
                "/usr/lib/aarch64-linux-gnu/libm.so.6",
                "/usr/lib64/libm.so.6",
                "/lib64/libm.so.6",
            ] {
                let p = Path::new(candidate);
                if p.exists() && NvcompPath::validate(p).is_ok() {
                    let res = NvcompDynamicCodec::open_dlopen(p);
                    assert_eq!(
                        res.err(),
                        Some(NvcompError::MissingSymbol(
                            "nvcompBatchedLZ4DecompressAsync"
                        ))
                    );
                    break;
                }
            }
            let invalid_so = NvcompPath {
                path: PathBuf::from("/nonexistent/root/libnvcomp.so.4"),
            };
            assert!(matches!(
                DlopenNvcompBackend::open(&invalid_so),
                Err(NvcompError::LoadFailed(_))
            ));
        }

        let path = NvcompPath {
            path: PathBuf::from(DEFAULT_NVCOMP_PATH),
        };
        let bad_nul = NvcompPath {
            path: PathBuf::from("/usr/lib/libnvcomp\0.so.4"),
        };
        assert!(matches!(
            DlopenNvcompBackend::open(&bad_nul),
            Err(NvcompError::UntrustedPath(_))
        ));
        let mut ok_codec = NvcompDynamicCodec::new(
            path.clone(),
            DlopenNvcompBackend::from_raw_fn(fake_nvcomp_ok),
        );
        assert_eq!(ok_codec.path(), Path::new(DEFAULT_NVCOMP_PATH));
        assert_eq!(ok_codec.crc_refusals(), 0);
        let payload = [0x5Au8; 8];
        let crc = crc32(&payload, 0);
        let mut out = [0u8; PAGE_BYTES];
        ok_codec
            .decompress_page(&ticket(crc), &payload, &mut out)
            .unwrap();
        assert_eq!(out, [0x5Au8; PAGE_BYTES]);
        assert_eq!(ok_codec.decompress_calls(), 1);
        assert!(matches!(
            ok_codec.decompress_page(&ticket(crc ^ 1), &payload, &mut out),
            Err(CodecError::CorruptPayload { page: 0 })
        ));
        assert_eq!(ok_codec.crc_refusals(), 1);

        let mut err_codec =
            NvcompDynamicCodec::new(path, DlopenNvcompBackend::from_raw_fn(fake_nvcomp_err));
        let err = err_codec
            .decompress_page(&ticket(crc), &payload, &mut out)
            .unwrap_err();
        assert!(matches!(err, CodecError::BackendFailure(msg) if msg.contains("rc=1")));
    }

    #[test]
    #[ignore = "requires uid 0 and cc; builds a root-owned /run shared library and exercises open_dlopen"]
    fn nvcomp_dlopen_root_trusted_library_roundtrip_and_refusals() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            assert_eq!(
                std::fs::metadata("/proc/self").unwrap().uid(),
                0,
                "root fixture prerequisite"
            );
            struct RootDir(PathBuf);
            impl Drop for RootDir {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let dir =
                Path::new("/run").join(format!("ramshared-nvcomp-root-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir).unwrap();
            let guard = RootDir(dir);
            std::fs::set_permissions(&guard.0, std::fs::Permissions::from_mode(0o755)).unwrap();

            let src_c = guard.0.join("nvcomp_fixture.c");
            let so_path = guard.0.join("libnvcomp.so.4");
            std::fs::write(
                &src_c,
                b"#include <stddef.h>\n\
                  #include <stdint.h>\n\
                  #include <string.h>\n\
                  int nvcompBatchedLZ4DecompressAsync(\n\
                      const uint8_t* const* comp_ptrs,\n\
                      const size_t* comp_bytes,\n\
                      const size_t* uncomp_bytes,\n\
                      size_t* actual_uncomp_bytes,\n\
                      size_t batch_size,\n\
                      void* temp_ptr,\n\
                      size_t temp_bytes,\n\
                      uint8_t* const* uncomp_ptrs,\n\
                      int32_t* statuses,\n\
                      void* stream) {\n\
                      (void)temp_ptr; (void)temp_bytes; (void)stream;\n\
                      if (batch_size != 1 || comp_bytes[0] == 0) return -1;\n\
                      memset(uncomp_ptrs[0], comp_ptrs[0][0], uncomp_bytes[0]);\n\
                      actual_uncomp_bytes[0] = uncomp_bytes[0];\n\
                      statuses[0] = 0;\n\
                      return 0;\n\
                  }\n",
            )
            .unwrap();
            let status = std::process::Command::new("cc")
                .args(["-shared", "-fPIC", "-O2"])
                .arg(&src_c)
                .arg("-o")
                .arg(&so_path)
                .status()
                .expect("invoke cc for nvcomp fixture");
            assert!(status.success(), "cc failed");
            std::fs::set_permissions(&so_path, std::fs::Permissions::from_mode(0o444)).unwrap();

            let mut codec = NvcompDynamicCodec::open_dlopen(&so_path).expect("open_dlopen as root");
            let payload = [0xA7u8; 16];
            let crc = crc32(&payload, 0);
            let mut out = [0u8; PAGE_BYTES];
            codec
                .decompress_page(&ticket(crc), &payload, &mut out)
                .expect("dlopen nvcomp decompress_page");
            assert_eq!(out, [0xA7u8; PAGE_BYTES]);
            assert_eq!(codec.decompress_calls(), 1);
            assert_eq!(codec.crc_refusals(), 0);

            let bad_ticket = ticket(crc ^ 0xFFFF_FFFF);
            assert!(matches!(
                codec.decompress_page(&bad_ticket, &payload, &mut out),
                Err(CodecError::CorruptPayload { page: 0 })
            ));
            assert_eq!(codec.decompress_calls(), 1);
            assert_eq!(codec.crc_refusals(), 1);
            eprintln!("NVCOMP_ROOT_DLOPEN_E2E=PASS path={}", so_path.display());
        }
    }
}
