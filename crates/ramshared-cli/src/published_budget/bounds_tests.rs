// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/published_budget/bounds_tests.rs
//! Actual encoded read limits, including a stream growing past its metadata.
#![allow(clippy::unwrap_used)]
use super::*;
use std::io::Read;

struct Counted {
    input: std::io::Cursor<Vec<u8>>,
    bytes: std::rc::Rc<std::cell::Cell<usize>>,
}
impl Read for Counted {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(out)?;
        self.bytes.set(self.bytes.get() + count);
        Ok(count)
    }
}
#[test]
fn status_stream_is_capped_even_if_it_grows_after_metadata() {
    let limit = ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES;
    let bytes = std::rc::Rc::new(std::cell::Cell::new(0));
    let reader = Counted {
        input: std::io::Cursor::new(vec![b' '; limit * 2]),
        bytes: bytes.clone(),
    };
    assert!(read_bounded_document(reader).is_none());
    assert_eq!(
        bytes.get(),
        limit + 1,
        "status reader consumed beyond its byte budget"
    );
    let exact = format!("\"{}\"", "x".repeat(limit - 2));
    assert!(read_bounded_document(exact.as_bytes()).is_some());
    assert!(read_bounded_document([0xff].as_slice()).is_none());
}
#[test]
fn oversized_regular_status_is_refused_before_json_admission() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-status-bound-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("status.json");
    std::fs::write(
        &path,
        format!(
            "\"{}\"",
            "x".repeat(ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES)
        ),
    )
    .unwrap();
    let refused = read_status_document(&path).is_none();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(&root).unwrap();
    assert!(refused, "oversized status was admitted");
}
