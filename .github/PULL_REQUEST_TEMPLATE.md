<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: .github/PULL_REQUEST_TEMPLATE.md
-->

## Summary

Describe what changed and why.

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace -- --test-threads=1`
- [ ] Kernel style/ABI gates (when touching `drivers/block/ramshared/`)

## Rollback Trigger

State the observable condition that warrants reverting this change (required for
changes to locking, DMA, memory management, swap teardown, or release trust).
