#![allow(deprecated)] // F-652: tests/benches still drive the deprecated bare read_frame helpers.
use forge_core::{types::SessionPersistence, Event};
use forge_ipc::{ClientInfo, Hello, IpcEvent, IpcMessage, Subscribe, PROTO_VERSION};
use forge_providers::MockProvider;
use forge_session::{server::serve_with_session, session::Session};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use tokio::net::UnixStream;

fn session_started_event() -> Event {
    Event::SessionStarted {
        at: chrono::Utc::now(),
        workspace: PathBuf::from("/tmp/ws"),
        agent: None,
        persistence: SessionPersistence::Ephemeral,
    }
}

async fn connect_with_retry(path: &PathBuf) -> UnixStream {
    for _ in 0..20 {
        match UnixStream::connect(path).await {
            Ok(s) => return s,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    UnixStream::connect(path)
        .await
        .expect("server did not start in time")
}

async fn do_handshake(stream: &mut UnixStream) -> u64 {
    let hello = IpcMessage::Hello(Hello {
        proto: PROTO_VERSION,
        client: ClientInfo {
            kind: "test".into(),
            pid: std::process::id(),
            user: "tester".into(),
        },
        schema_version: forge_ipc::SCHEMA_VERSION,
    });
    forge_ipc::write_frame(stream, &hello).await.unwrap();
    let response = forge_ipc::read_frame(stream).await.unwrap();
    let IpcMessage::HelloAck(ack) = response else {
        panic!("expected HelloAck, got {:?}", response);
    };
    ack.event_seq
}

#[tokio::test]
async fn subscribe_mid_stream_receives_historical_then_live_events() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let sock_path = dir.path().join("test.sock");
    // Isolate the daemon's user-scope `~/.mcp.json` loader from the
    // developer's real home; otherwise a populated host `~/.mcp.json`
    // adds an extra `Event::McpState` and `event_seq` reaches 4.
    let user_home = dir.path().join("user_home");
    std::fs::create_dir_all(&user_home).unwrap();

    // Create session and pre-write 3 events (seq 1, 2, 3)
    let session = Arc::new(Session::create(log_path).await.unwrap());
    session.emit(session_started_event()).await.unwrap(); // seq 1
    session.emit(session_started_event()).await.unwrap(); // seq 2
    session.emit(session_started_event()).await.unwrap(); // seq 3

    // Start server (provider unused in this test)
    let server_session = Arc::clone(&session);
    let server_sock = sock_path.clone();
    let provider = Arc::new(MockProvider::with_default_path());
    let server_user_home = user_home.clone();
    tokio::spawn(async move {
        serve_with_session(
            &server_sock,
            server_session,
            provider,
            false,
            false,
            None,
            None,
            None, // F-587: keyless test wiring
            None, // F-601: no active agent — memory off in this test
            Some(server_user_home),
            None, // F-752
        )
        .await
        .unwrap();
    });

    let mut stream = connect_with_retry(&sock_path).await;

    // Handshake — event_seq should reflect 3 written events
    let event_seq = do_handshake(&mut stream).await;
    assert_eq!(
        event_seq, 3,
        "HelloAck.event_seq should equal number of written events"
    );

    // Subscribe since: 1 — want events at seq 2 and 3 (historical), then live
    let sub = IpcMessage::Subscribe(Subscribe { since: 1 });
    forge_ipc::write_frame(&mut stream, &sub).await.unwrap();

    // Read 2 historical events
    let frame2 = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::Event(IpcEvent { seq: s2, .. }) = frame2 else {
        panic!("expected Event frame, got {:?}", frame2);
    };
    assert_eq!(s2, 2, "first historical event should be seq 2");

    let frame3 = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::Event(IpcEvent { seq: s3, .. }) = frame3 else {
        panic!("expected Event frame, got {:?}", frame3);
    };
    assert_eq!(s3, 3, "second historical event should be seq 3");

    // Emit a live event (seq 4)
    session.emit(session_started_event()).await.unwrap();

    // Client should receive it
    let frame4 = forge_ipc::read_frame(&mut stream).await.unwrap();
    let IpcMessage::Event(IpcEvent { seq: s4, .. }) = frame4 else {
        panic!("expected live Event frame, got {:?}", frame4);
    };
    assert_eq!(s4, 4, "live event should be seq 4");
}
