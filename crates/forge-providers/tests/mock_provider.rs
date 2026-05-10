use forge_providers::{ChatChunk, MockProvider};
use futures::StreamExt;
use std::io::Write;
use tempfile::NamedTempFile;

#[tokio::test]
async fn streams_delta_chunks() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, r#"{{"delta":"Hello "}}"#).unwrap();
    writeln!(file, r#"{{"delta":"world"}}"#).unwrap();

    let provider = MockProvider::new(file.path());
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 2);
    assert!(matches!(&chunks[0], ChatChunk::TextDelta(s) if s == "Hello "));
    assert!(matches!(&chunks[1], ChatChunk::TextDelta(s) if s == "world"));
}

#[tokio::test]
async fn streams_tool_call_chunks() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"{{"tool_call":{{"name":"fs.read","args":{{"path":"README.md"}}}}}}"#
    )
    .unwrap();

    let provider = MockProvider::new(file.path());
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 1);
    match &chunks[0] {
        ChatChunk::ToolCall { id, name, args } => {
            // The mock NDJSON shape did not carry an id in this script;
            // the optional field defaults to an empty string.
            assert!(id.is_empty(), "id defaults to empty when absent: {id:?}");
            assert_eq!(name, "fs.read");
            assert_eq!(args["path"], "README.md");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_tool_call_with_explicit_id_preserves_id() {
    // Mock NDJSON callers can opt into an explicit id; it must round-trip
    // onto the emitted [`ChatChunk::ToolCall`] so headless tests can
    // exercise the same wire-id round-trip path as the real providers.
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"{{"tool_call":{{"id":"toolu_xyz","name":"fs.read","args":{{"path":"README.md"}}}}}}"#
    )
    .unwrap();

    let provider = MockProvider::new(file.path());
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 1);
    match &chunks[0] {
        ChatChunk::ToolCall { id, name, args } => {
            assert_eq!(id, "toolu_xyz");
            assert_eq!(name, "fs.read");
            assert_eq!(args["path"], "README.md");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_done_chunk() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, r#"{{"done":"end_turn"}}"#).unwrap();

    let provider = MockProvider::new(file.path());
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 1);
    assert!(matches!(&chunks[0], ChatChunk::Done(s) if s == "end_turn"));
}

#[tokio::test]
async fn streams_full_scripted_turn() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, r#"{{"delta":"Hello "}}"#).unwrap();
    writeln!(file, r#"{{"delta":"world"}}"#).unwrap();
    writeln!(
        file,
        r#"{{"tool_call":{{"name":"fs.read","args":{{"path":"README.md"}}}}}}"#
    )
    .unwrap();
    writeln!(file, r#"{{"done":"tool_use"}}"#).unwrap();

    let provider = MockProvider::new(file.path());
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 4);
    assert!(matches!(&chunks[0], ChatChunk::TextDelta(_)));
    assert!(matches!(&chunks[1], ChatChunk::TextDelta(_)));
    assert!(matches!(&chunks[2], ChatChunk::ToolCall { .. }));
    assert!(matches!(&chunks[3], ChatChunk::Done(_)));
}

#[tokio::test]
async fn path_is_configurable() {
    let dir = tempfile::tempdir().unwrap();
    let custom_path = dir.path().join("my_script.json");
    std::fs::write(&custom_path, "{\"delta\":\"hi\"}\n").unwrap();

    let provider = MockProvider::new(&custom_path);
    let chunks: Vec<ChatChunk> = provider.stream().await.unwrap().collect().await;

    assert_eq!(chunks.len(), 1);
    assert!(matches!(&chunks[0], ChatChunk::TextDelta(s) if s == "hi"));
}

#[tokio::test]
async fn missing_file_returns_error() {
    let provider = MockProvider::new("/nonexistent/path/mock.json");
    let result = provider.stream().await;
    assert!(result.is_err());
}
