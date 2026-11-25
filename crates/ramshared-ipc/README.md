# ramshared-ipc
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-ipc/README.md
-->

## Scope & Responsibility

Shared framing, authentication and native vsock transport for the host/guest
control plane. The service and guest daemon own lease and storage effects.

The 24-byte frame header carries magic `0x52414D53`, version, payload length,
message type and correlation ID. Negotiation supports versions 2 through 4.
Bootstrap/control headers retain version 3; the GPU catalog requires version 4.
The protocol has 23 message IDs, including the mutual-handshake finish and GPU
catalog. Unsupported versions, types and flag bits are refused before body reads.

## Workspace Dependencies

Dependencies: the pure identity model in `ramshared-vram`, `serde`/`serde_json`,
`sha2`, and platform socket bindings. Native GPU calls remain outside this crate.

## Safety Invariants

Ordinary controls are limited to 4 KiB; manifests and complete GPU catalog
envelopes each have their own 64 KiB cap. The generic framing ceiling is 1 MiB.
Readers check declared lengths before allocation. Control/catalog serialization
stops at its cap. Product exchanges compose one absolute deadline with required
shrinking socket waits, fragment progress and failed-stream poisoning.

The mutual HMAC-SHA256 handshake authenticates the peer transcript. Protocol 4
signs each catalog under a separate host-role label, binding both handshake
nonces, negotiated claims/cadence, lease, round and heartbeat acknowledgement.
Replay, malformed identity and stale age are refused. Original parent age and
whole-exchange elapsed time bound immutable expiry, including lease expiry.
Authenticated unavailability removes the observation. This metadata does not
prove native VM/guest/worker correspondence or authorize GPU allocation. Local
sealed-origin authority is independent of the peer lease.

Windows Hyper-V streams query their actual kernel peer VM/service address and
compare it with the requested endpoint before returning a connection. The
transport exposes this observation without inventing VM scope for TCP/Unix
streams. Endpoint routing and HMAC authentication remain separate from GPU
allocation correspondence.

## Testing

Linux/Windows socket tests do not substitute for a connected Hyper-V/WSL or
installed occupied-tier qualification.

```bash
cargo test -p ramshared-ipc
```
