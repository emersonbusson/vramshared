// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/deadline.rs
//! One absolute budget for an entire stream exchange, including partial I/O.
use crate::vsock::VsockStreamTrait;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const STOP_POLL: Duration = Duration::from_millis(50);

/// An admitted borrowed transaction. Success requires `finish`; error or abandonment
/// attempts exact-stream shutdown. A caller must discard a failed connection.
/// Kernel scheduling is not real-time; late bytes are never accepted as success.
pub struct DeadlineIo<'a, S: VsockStreamTrait + ?Sized> {
    stream: &'a mut S,
    deadline: Instant,
    failed: bool,
    finished: bool,
    stop: Option<&'a AtomicBool>,
}
impl<'a, S: VsockStreamTrait + ?Sized> DeadlineIo<'a, S> {
    pub fn new(stream: &'a mut S, budget: Duration) -> io::Result<Self> {
        Self::create(stream, budget, None)
    }
    pub fn new_cancellable(
        stream: &'a mut S,
        budget: Duration,
        stop: &'a AtomicBool,
    ) -> io::Result<Self> {
        Self::create(stream, budget, Some(stop))
    }
    fn create(
        stream: &'a mut S,
        budget: Duration,
        stop: Option<&'a AtomicBool>,
    ) -> io::Result<Self> {
        let deadline = Instant::now()
            .checked_add(budget)
            .filter(|_| !budget.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid exchange deadline")
            })?;
        let mut exchange = Self {
            stream,
            deadline,
            failed: false,
            finished: false,
            stop,
        };
        let remaining = exchange.remaining()?;
        let wait = exchange.socket_wait(remaining);
        let result = exchange
            .stream
            .set_read_timeout(Some(wait))
            .and_then(|()| exchange.stream.set_write_timeout(Some(wait)));
        if let Err(error) = result {
            return Err(exchange.poison(error));
        }
        exchange.remaining()?;
        Ok(exchange)
    }
    fn poison(&mut self, error: io::Error) -> io::Error {
        if !self.failed {
            self.failed = true;
            let _ = self.stream.shutdown_both();
        }
        error
    }
    fn remaining(&mut self) -> io::Result<Duration> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "exchange is poisoned",
            ));
        }
        if self.stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            return Err(self.poison(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "control-plane stop requested",
            )));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.poison(io::Error::new(
                io::ErrorKind::TimedOut,
                "absolute exchange deadline expired",
            )));
        }
        Ok(remaining)
    }
    fn socket_wait(&self, remaining: Duration) -> Duration {
        if self.stop.is_some() {
            remaining.min(STOP_POLL)
        } else {
            remaining
        }
    }
    fn retry_poll<T>(&mut self, result: &io::Result<T>) -> io::Result<bool> {
        self.remaining()?;
        Ok(self.stop.is_some()
            && result.as_ref().is_err_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                )
            }))
    }
    fn account<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        self.remaining()?;
        match result {
            Err(error) if error.kind() != io::ErrorKind::Interrupted => Err(self.poison(error)),
            result => result,
        }
    }
    /// Final parsing/authentication must complete before committing the exchange.
    pub fn finish(mut self) -> io::Result<()> {
        self.remaining()?;
        self.finished = true;
        Ok(())
    }
    /// A response can shorten an idle-read budget, never extend the exchange.
    pub fn shorten(&mut self, budget: Duration) -> io::Result<()> {
        self.remaining()?;
        let candidate = Instant::now()
            .checked_add(budget)
            .filter(|_| !budget.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid shortened deadline")
            });
        match candidate {
            Ok(candidate) => self.deadline = self.deadline.min(candidate),
            Err(error) => return Err(self.poison(error)),
        }
        self.remaining().map(|_| ())
    }
}
impl<S: VsockStreamTrait + ?Sized> Read for DeadlineIo<'_, S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let remaining = self.remaining()?;
            let setup = self
                .stream
                .set_read_timeout(Some(self.socket_wait(remaining)));
            self.account(setup)?;
            let result = self.stream.read(buffer);
            if self.retry_poll(&result)? {
                continue;
            }
            let count = self.account(result)?;
            if count > buffer.len() {
                return Err(self.poison(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream read exceeds supplied buffer",
                )));
            }
            if count == 0 && !buffer.is_empty() {
                self.poison(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed during exchange",
                ));
            }
            return Ok(count);
        }
    }
}
impl<S: VsockStreamTrait + ?Sized> Write for DeadlineIo<'_, S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            let remaining = self.remaining()?;
            let setup = self
                .stream
                .set_write_timeout(Some(self.socket_wait(remaining)));
            self.account(setup)?;
            let result = self.stream.write(buffer);
            if self.retry_poll(&result)? {
                continue;
            }
            let count = self.account(result)?;
            if count > buffer.len() || count == 0 && !buffer.is_empty() {
                return Err(self.poison(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "stream made invalid write progress",
                )));
            }
            return Ok(count);
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        loop {
            let remaining = self.remaining()?;
            let setup = self
                .stream
                .set_write_timeout(Some(self.socket_wait(remaining)));
            self.account(setup)?;
            let result = self.stream.flush();
            if self.retry_poll(&result)? {
                continue;
            }
            return self.account(result);
        }
    }
}
impl<S: VsockStreamTrait + ?Sized> Drop for DeadlineIo<'_, S> {
    fn drop(&mut self) {
        if !self.finished && !self.failed {
            self.poison(io::Error::other(
                "exchange abandoned before final verification",
            ));
        }
    }
}
