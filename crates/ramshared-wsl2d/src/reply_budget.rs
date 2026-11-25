// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/reply_budget.rs
//! Per-connection reply credits. Saturation waits in the reader, never the worker.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

static NEXT_CREDIT: AtomicU64 = AtomicU64::new(1);

/// Process-unique authority for one admitted reply.
#[derive(Debug, Eq, PartialEq)]
pub struct ReplyCredit(u64);

fn allocate_credit(counter: &AtomicU64) -> Option<ReplyCredit> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .ok()
        .map(ReplyCredit)
}

#[derive(Default)]
struct Credits {
    reserved: VecDeque<(u64, usize)>,
    bytes: usize,
    closed: bool,
    writer_finished: bool,
}

/// Bounds replies already admitted to the shared worker and their payload bytes.
/// Each reply carries its own credit, regardless of completion order.
pub struct ReplyBudget {
    credits: Mutex<Credits>,
    available: Condvar,
    max_replies: usize,
    max_bytes: usize,
}

impl ReplyBudget {
    pub fn new(max_replies: usize, max_bytes: usize) -> Self {
        Self {
            credits: Mutex::new(Credits::default()),
            available: Condvar::new(),
            max_replies,
            max_bytes,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Credits> {
        self.credits.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reserve(&self, state: &mut Credits, bytes: usize) -> Option<ReplyCredit> {
        let total = state.bytes.checked_add(bytes)?;
        if state.closed || state.reserved.len() >= self.max_replies || total > self.max_bytes {
            return None;
        }
        let credit = allocate_credit(&NEXT_CREDIT)?;
        state.reserved.push_back((credit.0, bytes));
        state.bytes = total;
        Some(credit)
    }

    /// Try once. `None` may mean temporary saturation or terminal closure.
    pub fn admit(&self, bytes: usize) -> Option<ReplyCredit> {
        self.reserve(&mut self.lock(), bytes)
    }

    /// Wait for credit in this reader only. Impossible requests and writer exit refuse.
    /// Idle established swap connections have no arbitrary request-phase deadline.
    pub fn admit_wait(&self, bytes: usize) -> Option<ReplyCredit> {
        if self.max_replies == 0 || bytes > self.max_bytes {
            return None;
        }
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(credit) = self.reserve(&mut state, bytes) {
                return Some(credit);
            }
            if NEXT_CREDIT.load(Ordering::Relaxed) == u64::MAX {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(crate) fn contains(&self, credit: &ReplyCredit) -> bool {
        self.lock().reserved.iter().any(|(id, _)| *id == credit.0)
    }

    /// Return only the exact admitted reply; unknown or replayed IDs refuse.
    pub fn release(&self, credit: ReplyCredit) -> bool {
        let mut state = self.lock();
        let Some(index) = state.reserved.iter().position(|(id, _)| *id == credit.0) else {
            return false;
        };
        let Some((_, bytes)) = state.reserved.remove(index) else {
            return false;
        };
        state.bytes -= bytes;
        self.available.notify_all();
        true
    }

    /// Stop admission and wake every waiting reader. Does not free pending reply data.
    pub fn close(&self) {
        self.lock().closed = true;
        self.available.notify_all();
    }

    pub fn reserved_bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn pending(&self) -> usize {
        self.lock().reserved.len()
    }

    /// The writer owns this guard until its thread returns or unwinds.
    pub fn writer_guard(self: &Arc<Self>) -> ReplyWriterGuard {
        ReplyWriterGuard(Arc::clone(self))
    }

    pub(crate) fn wait_writer_finished(&self) {
        let mut state = self.lock();
        while !state.writer_finished {
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

pub struct ReplyWriterGuard(Arc<ReplyBudget>);

impl Drop for ReplyWriterGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.closed = true;
        state.writer_finished = true;
        self.0.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    #[test]
    fn reply_budget_waits_for_count_and_byte_credit() {
        for (count, bytes) in [(1, 100), (10, 10)] {
            let budget = Arc::new(ReplyBudget::new(count, bytes));
            let close = budget.writer_guard();
            let first = budget.admit(10).unwrap();
            let (tx, rx) = channel();
            let waiting = Arc::clone(&budget);
            let thread = std::thread::spawn(move || tx.send(waiting.admit_wait(1)).unwrap());
            let blocked = rx.recv_timeout(Duration::from_millis(50));
            assert!(budget.release(first));
            assert!(matches!(blocked, Err(RecvTimeoutError::Timeout)));
            let second = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            thread.join().unwrap();
            assert_eq!(budget.reserved_bytes(), 1);
            assert_eq!(budget.pending(), 1);
            let replay = ReplyCredit(second.0);
            assert!(budget.release(second));
            assert!(!budget.release(replay));
            assert_eq!(budget.reserved_bytes(), 0);
            drop(close);
        }
    }

    #[test]
    fn reply_budget_writer_exit_wakes_waiting_admission() {
        let budget = Arc::new(ReplyBudget::new(1, 10));
        let close = budget.writer_guard();
        assert!(budget.admit(10).is_some());
        let (tx, rx) = channel();
        let waiting = Arc::clone(&budget);
        let thread = std::thread::spawn(move || tx.send(waiting.admit_wait(1)).unwrap());
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(close);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(None));
        thread.join().unwrap();
        assert_eq!(budget.pending(), 1);
        assert!(budget.admit(0).is_none());
    }

    #[test]
    fn reply_budget_impossible_request_and_closed_admission_refuse() {
        assert!(ReplyBudget::new(0, 10).admit_wait(1).is_none());
        assert!(ReplyBudget::new(1, 10).admit_wait(11).is_none());
        let budget = ReplyBudget::new(2, usize::MAX);
        let credit = budget.admit(usize::MAX).unwrap();
        assert!(budget.admit(1).is_none());
        budget.close();
        assert!(budget.admit_wait(0).is_none());
        assert!(budget.admit(0).is_none());
        assert!(budget.release(credit));
        assert_eq!(budget.reserved_bytes(), 0);
    }

    #[test]
    fn reply_budget_recovers_poisoned_accounting() {
        let budget = Arc::new(ReplyBudget::new(2, 10));
        let cloned = Arc::clone(&budget);
        assert!(
            std::thread::spawn(move || {
                let _lock = cloned.lock();
                panic!("manufactured accounting poison");
            })
            .join()
            .is_err()
        );
        let credit = budget.admit_wait(1).unwrap();
        assert_eq!(budget.reserved_bytes(), 1);
        assert!(budget.release(credit));
        assert_eq!(budget.pending(), 0);
    }

    #[test]
    fn reply_budget_writer_panic_closes_admission() {
        let budget = Arc::new(ReplyBudget::new(2, 10));
        let cloned = Arc::clone(&budget);
        assert!(
            std::thread::spawn(move || {
                let _close = cloned.writer_guard();
                panic!("manufactured writer unwind");
            })
            .join()
            .is_err()
        );
        assert!(budget.admit_wait(1).is_none());
    }

    #[test]
    fn exact_credits_refuse_replay_foreign_and_exhaustion() {
        let budget = ReplyBudget::new(3, 100);
        let other = ReplyBudget::new(1, 100);
        let first = budget.admit(40).unwrap();
        let second = budget.admit(40).unwrap();
        let foreign = other.admit(20).unwrap();
        assert!(!budget.release(ReplyCredit(foreign.0)));
        assert_eq!(budget.reserved_bytes(), 80);
        let replay = ReplyCredit(second.0);
        assert!(budget.release(second));
        assert!(!budget.release(replay));
        assert_eq!(budget.reserved_bytes(), 40);
        assert!(budget.release(first));
        assert!(other.release(foreign));
        let exhausted = AtomicU64::new(u64::MAX);
        assert!(allocate_credit(&exhausted).is_none());
    }
}
