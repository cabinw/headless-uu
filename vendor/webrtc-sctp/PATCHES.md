# Local patches to webrtc-sctp 0.9.1

This is the crates.io `webrtc-sctp` 0.9.1 source package from
[webrtc-rs/sctp](https://github.com/webrtc-rs/sctp), retained under its original
MIT OR Apache-2.0 license. Both license texts and the published `Cargo.toml` and
`Cargo.toml.orig` are preserved.

Published package checksum:
`df75ec042002fe995194712cbeb2029107a60a7eab646f1b789eb1be94d0e367`.
The package's `.cargo_vcs_info.json` identifies upstream commit
`4c58e963685dfa35505057e0e3e21a6dddb8a87a`, path `sctp`.
The local Cargo cache marker `.cargo-ok` is omitted. The application uses its root
lockfile; the library's original lockfile is not used for dependency resolution.

## Delayed ACK timer rearm and cancellation (2026-11-11)

`src/timer/ack_timer.rs` is the only runtime source changed.

The published timer retained its cancellation sender after natural expiry.
`start()` checked whether that sender existed, so a second isolated DATA packet
could fail to arm a new delayed ACK. Another packet or retransmission could make
an immediate ACK happen later, masking the missing timer. The timer also stopped
observing cancellation after its deadline fired: a callback waiting for the
association mutex could run after `stop()` or after a replacement timer started.

The patch treats a closed cancellation receiver as a completed generation. It
continues to observe cancellation while acquiring the observer mutex and checks
cancellation again after acquiring it. After a callback returns, it closes only
its own receiver while still holding the observer mutex, before a new inbound
packet can rearm the timer. It never calls a shared observer's `stop()` from an
old callback, which could otherwise stop a newer generation.

The interval remains **200 ms**. [RFC 9260 section 6.2](https://www.rfc-editor.org/rfc/rfc9260.html#section-6.2)
recommends a delayed ACK within 200 ms and requires it within 500 ms. This patch
repairs timer lifecycle behavior; it changes no queue limit, congestion policy,
receive window, data-channel framing or mapping FIN behavior. It does not claim
to explain or fix the separately observed official-client relay concurrency or
TCP tail-loss failures.

Root `tests/sctp_ack_timer.rs` compiles this actual timer source with `#[path]`
and uses Tokio's fake clock without opening network sockets. Coverage includes
natural expiry and repeated rearming, cancellation while waiting for the observer,
replacement generation isolation, ordinary start/stop, owner drop, and completion
of an already-entered callback after a replacement starts. Against the unchanged
published timer, six of these seven tests fail; all seven pass with the patch.
The root test suite exercises them without enabling the vendor crate's unrelated
development dependencies or making it a workspace member.

An eighth regression uses a real SCTP `Association` over an in-memory packet
pipe, with Normal ACK mode and three isolated one-byte messages. The published
crate stops draining after the second message; the patched crate drains each
message after advancing 200 ms, with exactly three DATA and three SACK packets
and no retransmission. This test does not involve sockets, ICE, DTLS or an
official client. See the [validation record](../../docs/sctp-ack-timer-2026-11-11.md).
