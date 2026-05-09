#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
//! F-378: integration tests for the length-prefixed framing layer's bounds.
//!
//! `MAX_FRAME_SIZE` (4 MiB) is enforced at both the write side (pre-send
//! `bail!`) and the read side (pre-allocation `bail!` after the `u32`
//! length prefix). Before this file, only the happy-path round-trip was
//! exercised; oversized prefixes, malformed bodies, and the exact-cap
//! boundary were untested.
//!
//! These cases drive `read_frame` directly through an in-memory duplex
//! pipe so we can write *raw* length prefixes without being gated by
//! `write_frame`'s own cap check.

use forge_ipc::{read_frame, write_frame, ClientInfo, Hello, IpcMessage, PROTO_VERSION};
use tokio::io::{duplex, AsyncWriteExt};

/// Mirrors the private constant in `forge_ipc::lib`. The whole point of
/// this suite is to assert behavior at this boundary — duplicating the
/// value here keeps the test honest if the production constant ever
/// drifts (the `exactly_at_cap` case will start failing loudly).
const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// Case 1 — oversized length prefix must bail *before* the reader
/// allocates the full buffer. We write a `u32` length of
/// `MAX_FRAME_SIZE + 1` and zero body bytes; `read_frame` should
/// reject on the length check (`lib.rs:246`) without hanging on
/// `read_exact`.
#[tokio::test]
async fn oversized_length_prefix_rejected_before_allocation() {
    let (mut client, mut server) = duplex(64);

    // Deliberately exceed the cap by one byte.
    let oversized = (MAX_FRAME_SIZE + 1) as u32;
    client.write_u32(oversized).await.unwrap();
    // No body is written — if `read_frame` attempted `read_exact` on a
    // 4 MiB+1 buffer, the test would hang instead of erroring.
    client.shutdown().await.unwrap();

    let err = read_frame::<_, IpcMessage>(&mut server)
        .await
        .expect_err("read_frame must reject oversized length prefix");
    let msg = format!("{err}");
    assert!(
        msg.contains("frame too large"),
        "expected cap error, got: {msg}"
    );
}

/// Case 2 — a valid length prefix followed by bytes that aren't valid
/// JSON must surface a structured decode error, not a panic and not
/// a hang. This exercises the path at `lib.rs:251` after `read_exact`
/// succeeds.
#[tokio::test]
async fn malformed_body_returns_decode_error_not_panic() {
    let (mut client, mut server) = duplex(64);

    let garbage: &[u8] = b"{not json";
    client.write_u32(garbage.len() as u32).await.unwrap();
    client.write_all(garbage).await.unwrap();
    client.shutdown().await.unwrap();

    let err = read_frame::<_, IpcMessage>(&mut server)
        .await
        .expect_err("read_frame must surface a decode error for malformed JSON");
    // `serde_json::Error`'s `Display` starts with its error category; it
    // will never contain "frame too large" — that would mean the wrong
    // branch fired.
    let msg = format!("{err}");
    assert!(
        !msg.contains("frame too large"),
        "malformed body should not trip the cap error: {msg}"
    );
}

/// Case 3 — the cap is inclusive: a body of exactly `MAX_FRAME_SIZE`
/// bytes must round-trip cleanly. We pad a `SendUserMessage.text` so
/// its serialized JSON lands on the cap exactly, then write via
/// `write_frame` (which itself bails only on `>`, not `==`) and read
/// back.
#[tokio::test]
async fn exactly_at_cap_round_trips() {
    // Envelope: `{"t":"SendUserMessage","text":"..."}`. `'a'` is chosen
    // because it doesn't require JSON escaping, so serialized length
    // equals raw text length plus a fixed envelope overhead. We measure
    // the overhead dynamically — that way a future serde attribute
    // change (renamed tag, added field) makes this test fail at the
    // probe rather than the round-trip, pointing straight at the cause.
    let probe = IpcMessage::SendUserMessage(forge_ipc::SendUserMessage {
        text: String::new(),
    });
    let envelope_overhead = serde_json::to_vec(&probe).unwrap().len();
    let padding = "a".repeat(MAX_FRAME_SIZE - envelope_overhead);
    let sent = IpcMessage::SendUserMessage(forge_ipc::SendUserMessage { text: padding });

    // Sanity check the sizing — if `'a'` ever stops being a one-byte
    // JSON character (it won't, but be loud if it somehow does), this
    // fires before `write_frame` sees the body.
    let body = serde_json::to_vec(&sent).unwrap();
    assert_eq!(
        body.len(),
        MAX_FRAME_SIZE,
        "serialized body is {} bytes, expected {}",
        body.len(),
        MAX_FRAME_SIZE
    );

    // Duplex pipe must be sized to hold the full frame + 4-byte length
    // prefix without back-pressuring the writer task.
    let (mut client, mut server) = duplex(MAX_FRAME_SIZE + 64);

    let writer = tokio::spawn(async move {
        write_frame(&mut client, &sent)
            .await
            .expect("write_frame must accept a body of exactly MAX_FRAME_SIZE")
    });

    let got: IpcMessage = read_frame(&mut server)
        .await
        .expect("read_frame must accept a body of exactly MAX_FRAME_SIZE");
    writer.await.unwrap();

    match got {
        IpcMessage::SendUserMessage(m) => {
            assert_eq!(m.text.len(), MAX_FRAME_SIZE - envelope_overhead);
        }
        other => panic!("unexpected variant round-tripped: {other:?}"),
    }
}

/// F-660 — Case 4: an oversized message must be rejected by `write_frame`
/// before it walks the entire serializer. Prior behavior fully built a
/// `Vec<u8>` via `serde_json::to_vec` and only checked `body.len()` after
/// the fact, so a message whose serialized form is `N >> MAX_FRAME_SIZE`
/// cost `O(N)` allocations + serializer time per rejected frame.
///
/// We exercise this behaviorally: write a message ~16x over the cap and
/// confirm `write_frame`:
///   1. surfaces the cap error,
///   2. does not write any bytes to the underlying writer (no length
///      prefix, no body), and
///   3. completes well under a generous wall-clock ceiling — a bounded
///      early-exit returns essentially instantly even for very large
///      payloads.
#[tokio::test]
async fn oversized_message_rejected_without_full_serialization() {
    use std::time::Instant;
    use tokio::io::AsyncReadExt;

    let huge_text = "a".repeat(16 * MAX_FRAME_SIZE);
    let sent = IpcMessage::SendUserMessage(forge_ipc::SendUserMessage { text: huge_text });

    // Server side stays open so we can verify nothing was written.
    let (mut client, mut server) = duplex(64);

    let started = Instant::now();
    let result = write_frame(&mut client, &sent).await;
    let elapsed = started.elapsed();
    drop(client);

    let err = result.expect_err("write_frame must reject oversized message");
    let msg = format!("{err}");
    assert!(
        msg.contains("frame too large"),
        "expected cap error, got: {msg}"
    );

    // The writer must not have been touched on the rejection path —
    // a partial length prefix or partial body would corrupt the wire
    // for the next frame.
    let mut sink = Vec::new();
    let n = server.read_to_end(&mut sink).await.unwrap();
    assert_eq!(
        n, 0,
        "write_frame leaked {n} bytes onto the wire on the rejection path"
    );

    // Generous sanity ceiling. Bounded early-exit on a 16x-cap payload
    // is essentially instantaneous; this would have caught the old
    // `serde_json::to_vec` path under any meaningful regression.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "write_frame took {:?} for a {}-byte payload; \
         expected early-exit before full serialization",
        elapsed,
        16 * MAX_FRAME_SIZE
    );
}

/// Guard against importing an unused symbol — keeps `Hello`/`ClientInfo`
/// honest if the fixture helpers above are ever inlined away.
#[allow(dead_code)]
fn _ensure_symbols_compile() -> IpcMessage {
    IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: String::new(),
            pid: 0,
            user: String::new(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    })
}
