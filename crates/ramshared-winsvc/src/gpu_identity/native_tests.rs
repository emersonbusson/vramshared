// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/native_tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
struct Api {
    mode: &'static str,
    calls: Vec<String>,
}
impl Api {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            calls: vec![],
        }
    }
}
impl AdapterApi for Api {
    fn open(&mut self, request: &mut OpenAdapter) -> i32 {
        self.calls.push("open".into());
        assert_eq!(request.luid.low, 42);
        assert_eq!(request.luid.high, 1);
        request.handle = if self.mode == "no-handle" { 0 } else { 17 };
        if self.mode == "open-scope" {
            request.luid.low = 43;
        }
        if self.mode == "open-error" { -1 } else { 0 }
    }
    fn query(&mut self, request: &mut QueryAdapter, output: &mut InterfaceBuffer) -> i32 {
        self.calls.push("query".into());
        assert_eq!(request.handle, 17);
        assert_eq!(request.kind, 79);
        assert_eq!(request.bytes, 520);
        assert_eq!(request.data, output.as_mut_ptr().cast());
        for (to, from) in output.iter_mut().zip("opaque-device\0".encode_utf16()) {
            *to = from;
        }
        match self.mode {
            "query-error" => return -2,
            "handle" => request.handle = 18,
            "kind" => request.kind = 78,
            "pointer" => request.data = core::ptr::null_mut(),
            "bytes" => request.bytes = 518,
            "unterminated" => output.fill(u16::MAX),
            "empty" => output[0] = 0,
            "utf16" => {
                output[0] = 0xd800;
                output[1] = 0;
            }
            "panic" => panic!("injected Rust query unwind"),
            _ => {}
        }
        0
    }
    fn close(&mut self, handle: u32) -> i32 {
        assert_eq!(handle, 17);
        self.calls.push("close".into());
        if self.mode == "close-error" { -3 } else { 0 }
    }
}
#[test]
fn native_interface_query_owns_and_closes_the_exact_opened_handle() {
    let mut api = Api::new("valid");
    assert_eq!(
        query_interface("00000001:0000002a", &mut api).unwrap(),
        "opaque-device"
    );
    assert_eq!(api.calls, ["open", "query", "close"]);
}
#[test]
fn native_interface_errors_and_rewritten_scope_close_before_refusal() {
    for mode in [
        "open-error",
        "open-scope",
        "query-error",
        "handle",
        "kind",
        "pointer",
        "bytes",
        "unterminated",
        "empty",
        "utf16",
        "close-error",
    ] {
        let mut api = Api::new(mode);
        assert!(
            query_interface("00000001:0000002a", &mut api).is_err(),
            "{mode}"
        );
        assert_eq!(api.calls.last().unwrap(), "close", "{mode}");
        assert_eq!(
            api.calls.iter().filter(|c| *c == "close").count(),
            1,
            "{mode}"
        );
        if mode.starts_with("open") {
            assert_eq!(api.calls, ["open", "close"]);
        }
    }
    let mut api = Api::new("no-handle");
    assert!(query_interface("00000001:0000002a", &mut api).is_err());
    assert_eq!(api.calls, ["open"]);
}
#[test]
fn native_interface_rust_unwind_retains_adapter_cleanup() {
    let mut api = Api::new("panic");
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| query_interface(
            "00000001:0000002a",
            &mut api
        )))
        .is_err()
    );
    assert_eq!(api.calls, ["open", "query", "close"]);
}
#[test]
fn malformed_luid_and_invalid_native_output_pointer_do_not_call_gdi() {
    for value in ["missing", "x:0000002a", "00000001:x"] {
        let mut api = Api::new("valid");
        assert!(query_interface(value, &mut api).is_err());
        assert!(api.calls.is_empty());
    }
    let mut output = [0; ramshared_vram::host_identity::INTERFACE_UTF16_CAPACITY];
    let mut request = QueryAdapter {
        handle: 0,
        kind: 79,
        data: core::ptr::null_mut(),
        bytes: 520,
    };
    assert_ne!(NativeGdi.query(&mut request, &mut output), 0);
}
