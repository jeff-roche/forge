//! F-608 step 6 sidecar-side: receive `Credentials` frame, stash, no
//! log leakage.
//!
//! The integration test in [`forged_agent_lifecycle.rs`] already proves
//! handshake / heartbeat / stub-RunTurn / shutdown end-to-end. This file
//! adds a second pass that:
//!
//! 1. drives a real `forged-agent` child through handshake,
//! 2. pushes a [`SidecarMessage::Credentials`] frame onto the daemon →
//!    sidecar leg with a known test secret,
//! 3. shuts the child down cleanly,
//! 4. captures the child's stderr (a JSON-lines `tracing` stream when
//!    `FORGE_LOG=trace`) and asserts the secret value never appears at
//!    any log level — even with the trace-grade filter the architecture
//!    doc §10 specifies.
//!
//! Captured stderr stays in the test output on failure so triage
//! doesn't have to reproduce locally.

use std::path::PathBuf;
use std::time::Duration;

use forge_core::ProviderId;
use forge_ipc::sidecar::{
    SecretBytes, SidecarCredentials, SidecarHelloAck, SidecarMessage, SidecarShutdown,
    SIDECAR_SCHEMA_VERSION,
};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

const TEST_SECRET_VALUE: &str = "sk-ant-step6-sidecar-leakcanary-XYZZY";

fn forged_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_forged-agent"))
}

async fn read_one(s: &mut UnixStream) -> SidecarMessage {
    forge_ipc::read_frame_with_deadline::<_, SidecarMessage>(s, Duration::from_secs(10))
        .await
        .expect("read frame")
}

#[tokio::test]
async fn credentials_frame_stashed_without_log_leakage() {
    let tmp = TempDir::new().expect("tmp");
    let socket = tmp.path().join("agent.sock");
    let listener = UnixListener::bind(&socket).expect("bind uds");

    // Crank the sidecar's tracing layer to `trace` so the credential
    // stash emission hits stderr — that's the path we want to audit.
    let mut child = tokio::process::Command::new(forged_agent_path())
        .arg("--socket")
        .arg(&socket)
        .arg("--instance-id")
        .arg("inst-cred-test")
        .env("FORGE_LOG", "trace")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn forged-agent");

    let stderr = child.stderr.take().expect("captured stderr");

    let (mut conn, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept failed");

    // Hello → HelloAck handshake. The sidecar bails on a missing ack
    // so we have to drive both sides.
    let hello = read_one(&mut conn).await;
    match hello {
        SidecarMessage::Hello(h) => {
            assert_eq!(h.instance_id.to_string(), "inst-cred-test");
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    let ack = SidecarMessage::HelloAck(SidecarHelloAck {
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
        schema_version: SIDECAR_SCHEMA_VERSION,
    });
    forge_ipc::write_frame(&mut conn, &ack)
        .await
        .expect("write ack");

    // Push the credential frame the daemon side would emit before
    // `RunTurn`. Keyless providers never read the stash; this test
    // is purely about the receive + stash + no-leak guarantee.
    let cred = SidecarMessage::Credentials(SidecarCredentials {
        provider_id: ProviderId::from_string("anthropic".into()),
        secret: SecretBytes::new(TEST_SECRET_VALUE.as_bytes().to_vec()),
    });
    forge_ipc::write_frame(&mut conn, &cred)
        .await
        .expect("write Credentials");

    // Give the sidecar a beat to dispatch the frame and emit any
    // tracing line it would produce. 250 ms is comfortably more than
    // the dispatch loop's per-frame cost (single-digit microseconds on
    // tokio UDS).
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Cooperative shutdown — drains and exits clean.
    let shutdown = SidecarMessage::Shutdown(SidecarShutdown { grace_ms: 1000 });
    forge_ipc::write_frame(&mut conn, &shutdown)
        .await
        .expect("write shutdown");

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("child did not exit within 15s of Shutdown")
        .expect("wait failed");
    assert!(
        status.success(),
        "forged-agent exited with non-zero status: {status:?}"
    );

    // Drain the captured stderr to a single buffer and audit it.
    let mut stderr_buf = Vec::new();
    let mut reader = tokio::io::BufReader::new(stderr);
    let _ = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut stderr_buf))
        .await
        .expect("read stderr");
    let stderr_text = String::from_utf8_lossy(&stderr_buf);

    // The hardline assertion — the secret value must not appear in
    // any captured log line, at any level. Print the captured text
    // on failure so triage has the smoking gun.
    assert!(
        !stderr_text.contains(TEST_SECRET_VALUE),
        "credential value leaked into sidecar stderr:\n{stderr_text}"
    );

    // Belt-and-braces: the architecture-doc-mandated trace line MUST
    // appear, since its presence proves the stash code path actually
    // ran (the negative assertion above can't be vacuously satisfied
    // by a no-op match arm).
    assert!(
        stderr_text.contains("stashed credential"),
        "expected 'stashed credential' trace emission to appear; \
         absence implies the stash arm didn't run:\n{stderr_text}"
    );
    assert!(
        stderr_text.contains("anthropic"),
        "non-secret provider_id should appear in trace output:\n{stderr_text}"
    );
}
