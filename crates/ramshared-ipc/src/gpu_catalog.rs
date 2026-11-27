// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/gpu_catalog.rs
//! Host-role authenticated metadata, bound to one negotiated lease and round.
//! This proves a host observation only; it cannot authorize GPU allocation.
use crate::{
    FrameError, HandshakeTranscript, HeartbeatAck, HostChallenge, HostMemoryTelemetry,
    IPC_VERSION_4, IPC_VERSION_5, IPC_VERSION_6, IPC_VERSION_7, MAX_GPU_CATALOG_PAYLOAD,
    compute_hmac, host_proof, verify_hmac,
};
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use ramshared_vram::host_identity::{HostGpuIdentityReport, HostGpuObservation};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

mod activity;
pub use activity::{AuthenticatedActivity, HostGpuActivityObservation};

const LABEL: &[u8] = b"ramshared-vsock-host-gpu-catalog-v1";
/// Maximum accepted age of a signed host-memory observation.
pub const HOST_MEMORY_MAX_AGE_MS: u64 = 5_000;
fn refuse(reason: &str) -> FrameError {
    FrameError::PayloadDeserialization(format!("GPU catalog refused: {reason}"))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    lease_id: u32,
    round: u64,
    ack: HeartbeatAck,
    observation: Option<HostGpuObservation>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<B> {
    body: B,
    proof: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSession {
    binding: [u8; 32],
    lease_id: u32,
    round: u64,
    version: u32,
}
/// Monotonic times bracketing the entire guest exchange, including writes.
pub struct CatalogTiming {
    pub started: Instant,
    pub received: Instant,
}
/// Verified metadata with immutable expiry. No deserialization or public mint.
#[derive(Clone, Debug)]
pub struct AuthenticatedCatalog {
    report: HostGpuIdentityReport,
    received: Instant,
    expires: Instant,
}

/// Host memory bound to a verified heartbeat and a bounded monotonic lease.
/// Only `CatalogSession` can mint this value after authenticating the frame.
/// A caller must also verify that the session has not been revoked early.
#[derive(Clone, Debug)]
pub struct AuthenticatedHostMemory {
    sample: HostMemoryTelemetry,
    received: Instant,
    expires: Instant,
}

impl AuthenticatedHostMemory {
    /// Both the original host timestamp and the local lease must remain fresh.
    pub fn sample(&self, now: Instant, now_unix_ms: u64) -> Option<&HostMemoryTelemetry> {
        if now < self.received || now >= self.expires {
            return None;
        }
        let age = now_unix_ms.checked_sub(self.sample.sampled_at_unix_ms)?;
        (age < HOST_MEMORY_MAX_AGE_MS).then_some(&self.sample)
    }
}

/// One completely authenticated heartbeat, including optional observations.
pub struct AuthenticatedHeartbeat {
    pub catalog: Option<AuthenticatedCatalog>,
    pub host_memory: Option<AuthenticatedHostMemory>,
    pub activity: Option<AuthenticatedActivity>,
}
impl AuthenticatedCatalog {
    pub fn report(&self, now: Instant) -> Option<&HostGpuIdentityReport> {
        (now >= self.received && now < self.expires).then_some(&self.report)
    }

    pub fn remaining_ms(&self, now: Instant) -> Option<u64> {
        if now < self.received || now >= self.expires {
            return None;
        }
        let millis = self.expires.saturating_duration_since(now).as_millis();
        (millis > 0).then(|| u64::try_from(millis).unwrap_or(u64::MAX))
    }
}
impl CatalogSession {
    /// Call only after mutual handshake proof verification on this connection.
    pub fn new(
        secret: &[u8],
        transcript: &HandshakeTranscript,
        challenge: &HostChallenge,
        lease_id: u32,
    ) -> Result<Self, FrameError> {
        if secret.is_empty()
            || lease_id == 0
            || !matches!(
                challenge.accepted_version,
                IPC_VERSION_4 | IPC_VERSION_5 | IPC_VERSION_6 | IPC_VERSION_7
            )
            || !(transcript.min_version..=transcript.max_version)
                .contains(&challenge.accepted_version)
        {
            return Err(refuse(
                "missing key/lease or supported protocol not negotiated",
            ));
        }
        let binding = host_proof(secret, transcript, challenge)
            .map_err(|error| refuse(&error.to_string()))?;
        Ok(Self {
            binding,
            lease_id,
            round: 4,
            version: challenge.accepted_version,
        })
    }
    pub fn round(&self) -> u64 {
        self.round
    }
    fn next_round(&self) -> Result<u64, FrameError> {
        self.round
            .checked_add(1)
            .ok_or_else(|| refuse("round exhausted"))
    }
    fn proof_bytes(&self, body: &impl Serialize) -> Result<Vec<u8>, FrameError> {
        let encoded = crate::json_payload::encode(body, MAX_GPU_CATALOG_PAYLOAD)?;
        let mut bytes = Vec::with_capacity(LABEL.len() + self.binding.len() + encoded.len());
        bytes.extend_from_slice(LABEL);
        bytes.extend_from_slice(&self.binding);
        bytes.extend_from_slice(&encoded);
        Ok(bytes)
    }

    fn encode_body(&self, secret: &[u8], body: impl Serialize) -> Result<Vec<u8>, FrameError> {
        let proof = compute_hmac(secret, &self.proof_bytes(&body)?);
        crate::json_payload::encode(&Envelope { body, proof }, MAX_GPU_CATALOG_PAYLOAD)
    }

    fn decode_body<B: DeserializeOwned + Serialize>(
        &self,
        secret: &[u8],
        encoded: &[u8],
    ) -> Result<B, FrameError> {
        let envelope: Envelope<B> =
            serde_json::from_slice(encoded).map_err(|error| refuse(&error.to_string()))?;
        if !verify_hmac(secret, &self.proof_bytes(&envelope.body)?, &envelope.proof) {
            return Err(refuse("host proof mismatch"));
        }
        Ok(envelope.body)
    }

    /// Seal once for the next round. A failed write requires discarding the
    /// connection, never rewinding this sequence to replay a partial exchange.
    pub fn seal(
        &mut self,
        secret: &[u8],
        ack: &HeartbeatAck,
        observation: Option<HostGpuObservation>,
    ) -> Result<Vec<u8>, FrameError> {
        self.seal_with_activity(secret, ack, observation, None)
    }

    /// Encode activity only in a connection negotiated for the extended catalog.
    pub fn seal_with_activity(
        &mut self,
        secret: &[u8],
        ack: &HeartbeatAck,
        observation: Option<HostGpuObservation>,
        activity: Option<HostGpuActivityObservation>,
    ) -> Result<Vec<u8>, FrameError> {
        let next = self.next_round()?;
        if let Some(sample) = &observation {
            sample.validate().map_err(|error| refuse(&error))?;
        }
        let body = Body {
            lease_id: self.lease_id,
            round: self.round,
            ack: ack.clone(),
            observation,
        };
        let encoded = if self.version >= IPC_VERSION_6 {
            self.encode_extended(
                secret,
                &body,
                activity.as_ref(),
                self.version >= IPC_VERSION_7,
            )?
        } else {
            self.encode_body(secret, body)?
        };
        self.round = next;
        Ok(encoded)
    }
    pub fn open(
        &mut self,
        secret: &[u8],
        ack: &HeartbeatAck,
        encoded: &[u8],
        correlation: u64,
        timing: CatalogTiming,
    ) -> Result<Option<AuthenticatedCatalog>, FrameError> {
        self.open_verified(secret, ack, encoded, correlation, timing)
            .map(|heartbeat| heartbeat.catalog)
    }

    /// Authenticate the full acknowledgement before exposing any observation.
    pub fn open_verified(
        &mut self,
        secret: &[u8],
        ack: &HeartbeatAck,
        encoded: &[u8],
        correlation: u64,
        timing: CatalogTiming,
    ) -> Result<AuthenticatedHeartbeat, FrameError> {
        let next = self.next_round()?;
        if encoded.len() > MAX_GPU_CATALOG_PAYLOAD as usize {
            return Err(refuse("envelope exceeds 64 KiB"));
        }
        let (body, activity) = if self.version >= IPC_VERSION_6 {
            self.decode_extended(secret, encoded, self.version >= IPC_VERSION_7)?
        } else {
            (self.decode_body(secret, encoded)?, None)
        };
        if correlation != self.round
            || body.round != self.round
            || body.lease_id != self.lease_id
            || &body.ack != ack
        {
            return Err(refuse("round, lease or heartbeat acknowledgement mismatch"));
        }
        let elapsed = timing
            .received
            .checked_duration_since(timing.started)
            .ok_or_else(|| refuse("monotonic clock moved backwards"))?;
        let elapsed_ms = u64::try_from(elapsed.as_nanos().div_ceil(1_000_000))
            .map_err(|_| refuse("elapsed age overflow"))?;
        let lease_remaining = ack
            .lease_remaining_ms
            .checked_sub(elapsed_ms)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| refuse("lease expired in transit"))?;
        let catalog = match body.observation {
            None => None,
            Some(sample) => {
                sample.validate().map_err(|error| refuse(&error))?;
                let age = sample
                    .age_ms
                    .checked_add(elapsed_ms)
                    .ok_or_else(|| refuse("observation age overflow"))?;
                // A valid sample may expire before receipt. Revoke the
                // catalog while retaining the independently verified beat.
                if age >= GPU_BUDGET_MAX_AGE_MS {
                    None
                } else {
                    let expires = timing
                        .received
                        .checked_add(Duration::from_millis(
                            (GPU_BUDGET_MAX_AGE_MS - age).min(lease_remaining),
                        ))
                        .ok_or_else(|| refuse("observation expiry overflow"))?;
                    Some(AuthenticatedCatalog {
                        report: sample.report,
                        received: timing.received,
                        expires,
                    })
                }
            }
        };
        let host_memory = ack.host_memory.as_ref().and_then(|sample| {
            if sample.sampled_at_unix_ms == 0
                || sample.total_bytes == 0
                || sample.available_bytes > sample.total_bytes
                || sample.commit_limit_bytes == 0
                || sample.commit_total_bytes > sample.commit_limit_bytes
                || !sample.page_size_bytes.is_power_of_two()
            {
                return None;
            }
            let duration = Duration::from_millis(lease_remaining.min(HOST_MEMORY_MAX_AGE_MS));
            let expires = timing.received.checked_add(duration)?;
            Some(AuthenticatedHostMemory {
                sample: sample.clone(),
                received: timing.received,
                expires,
            })
        });
        self.round = next;
        Ok(AuthenticatedHeartbeat {
            catalog,
            host_memory,
            activity: activity.and_then(|sample| {
                sample.authenticate(timing.received, elapsed_ms, lease_remaining)
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ramshared_vram::host_identity::{HostGpuIdentityReport, ProducerProcess};
    #[test]
    fn signed_reports_still_require_model_validation_and_rounds_never_wrap() {
        let secret = b"test-only-key";
        let base = CatalogSession {
            binding: [1; 32],
            lease_id: 7,
            round: 4,
            version: IPC_VERSION_4,
        };
        let now = Instant::now();
        let ack = HeartbeatAck {
            timestamp_ms: 100,
            lease_remaining_ms: 5000,
            host_memory: None,
        };
        let sample = HostGpuObservation {
            age_ms: 100,
            report: HostGpuIdentityReport {
                schema_version: 1,
                producer: ProducerProcess {
                    pid: 1,
                    creation_time_100ns: 2,
                },
                started_at_unix_ms: 100,
                elapsed_ms: 2,
                adapters: vec![],
            },
        };
        for field in ["schema", "age", "pid"] {
            let mut sample = sample.clone();
            match field {
                "schema" => sample.report.schema_version = 2,
                "age" => sample.age_ms = 0,
                "pid" => sample.report.producer.pid = 0,
                _ => unreachable!(),
            }
            let body = Body {
                lease_id: 7,
                round: 4,
                ack: ack.clone(),
                observation: Some(sample),
            };
            let proof = compute_hmac(secret, &base.proof_bytes(&body).unwrap());
            let encoded =
                crate::json_payload::encode(&Envelope { body, proof }, MAX_GPU_CATALOG_PAYLOAD)
                    .unwrap();
            let mut guest = base.clone();
            assert!(
                guest
                    .open(
                        secret,
                        &ack,
                        &encoded,
                        4,
                        CatalogTiming {
                            started: now,
                            received: now
                        }
                    )
                    .is_err()
            );
            assert_eq!(guest.round(), 4);
        }
        let mut exhausted = base;
        exhausted.round = u64::MAX;
        assert!(exhausted.seal(secret, &ack, None).is_err());
        assert!(
            exhausted
                .open(
                    secret,
                    &ack,
                    b"",
                    u64::MAX,
                    CatalogTiming {
                        started: now,
                        received: now
                    }
                )
                .is_err()
        );
        assert_eq!(exhausted.round(), u64::MAX);
    }
}
