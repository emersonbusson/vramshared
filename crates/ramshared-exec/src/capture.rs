// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/capture.rs
//! Bounded output capture shared by controller and serving-process policies.

use crate::bounded_process::ProcessSpawnError;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Instant;

pub(crate) struct CaptureWorker {
    pub(crate) stream: &'static str,
    pub(crate) receiver: Receiver<Result<Vec<u8>, String>>,
    pub(crate) join: Option<JoinHandle<()>>,
}

pub(crate) type CapturedBytes = Result<Vec<u8>, String>;

impl CaptureWorker {
    pub(crate) fn spawn<R>(stream: &'static str, mut reader: R, limit: usize) -> io::Result<Self>
    where
        R: Read + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name(format!("ramshared-{stream}-capture"))
            .spawn(move || {
                let mut output = Vec::new();
                let mut buffer = [0_u8; 4096];
                let result = loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break Ok(output),
                        Ok(read) if output.len().saturating_add(read) <= limit => {
                            output.extend_from_slice(&buffer[..read]);
                        }
                        Ok(_) => break Err(format!("{stream} output exceeded {limit} bytes")),
                        Err(error) => break Err(format!("{stream} capture failed: {error}")),
                    }
                };
                let _ = sender.send(result);
            })?;
        Ok(Self {
            stream,
            receiver,
            join: Some(join),
        })
    }

    pub(crate) fn receive_until(&self, deadline: Instant) -> Result<Result<Vec<u8>, String>, ()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.receiver.recv_timeout(remaining).map_err(|_| ())
    }

    pub(crate) fn join(&mut self) -> Result<(), ProcessSpawnError> {
        self.join
            .take()
            .ok_or_else(|| ProcessSpawnError::new("capture worker joined twice"))?
            .join()
            .map_err(|_| ProcessSpawnError::new(format!("{} capture worker panicked", self.stream)))
    }
    pub(crate) fn try_receive(&self) -> Option<CapturedBytes> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(format!("{} capture disconnected", self.stream)))
            }
        }
    }

    pub(crate) fn finish_if_ready(&mut self) -> Option<Result<(), ProcessSpawnError>> {
        match self.join.as_ref() {
            Some(worker) if !worker.is_finished() => None,
            Some(_) => Some(self.join()),
            None => Some(Ok(())),
        }
    }
}
