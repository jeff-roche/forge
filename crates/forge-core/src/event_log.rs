use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Duration;

use crate::event::Event;
use crate::workspace;
use crate::{ForgeError, Result};

const SCHEMA_HEADER: &str = r#"{"schema_version":1}"#;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Per-line byte cap for `events.jsonl` / transcript readers.
///
/// Legitimate events are far smaller than this (the orchestrator chunks
/// assistant deltas). The cap exists so a malformed or adversarial log
/// cannot force readers to buffer an unbounded line before parsing
/// (see CWE-770 — Allocation of Resources Without Limits).
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

pub struct EventLog {
    writer: Arc<Mutex<BufWriter<tokio::fs::File>>>,
    flush_task: JoinHandle<()>,
}

impl EventLog {
    pub async fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Some(root) = forge_dir_parent(path) {
            workspace::ensure_gitignore(root).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .await?;
        let mut writer = BufWriter::new(file);
        writer.write_all(SCHEMA_HEADER.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(Self::with_writer(writer))
    }

    pub async fn open(path: &Path) -> Result<Self> {
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await?;

        // Validate schema header using a single file handle to avoid TOCTOU.
        let expected = format!("{SCHEMA_HEADER}\n");
        let mut buf = vec![0u8; expected.len()];
        file.read_exact(&mut buf).await.map_err(|_| {
            ForgeError::Other(anyhow::anyhow!(
                "events.jsonl too short or missing schema header"
            ))
        })?;
        if buf != expected.as_bytes() {
            return Err(ForgeError::Other(anyhow::anyhow!(
                "events.jsonl schema header mismatch: expected {SCHEMA_HEADER:?}"
            )));
        }

        // Seek to end on the same handle before wrapping in BufWriter.
        file.seek(std::io::SeekFrom::End(0)).await?;
        Ok(Self::with_writer(BufWriter::new(file)))
    }

    fn with_writer(writer: BufWriter<tokio::fs::File>) -> Self {
        let shared = Arc::new(Mutex::new(writer));
        let flush_handle = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
                loop {
                    ticker.tick().await;
                    let _ = shared.lock().await.flush().await;
                }
            })
        };
        Self {
            writer: shared,
            flush_task: flush_handle,
        }
    }

    pub async fn append(&mut self, event: &Event) -> Result<()> {
        // F-572: serialize directly into a single buffer with the trailing
        // newline so the Mutex covers exactly one `write_all` await instead
        // of two. Halves the lock-held await count under streaming-delta
        // load, which is what the 50ms background flusher contends against.
        //
        // Crash safety is unchanged: the BufWriter still buffers in memory
        // and the durability boundary remains the explicit `flush()` /
        // background flush_task tick, exactly as before.
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut w = self.writer.lock().await;
        w.write_all(&line).await?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.writer.lock().await.flush().await?;
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        self.flush_task.abort();
        self.flush().await
    }
}

/// Outcome of a single bounded-line read.
enum BoundedLine {
    /// A full line was read; terminating `\n` has been consumed but is not
    /// included in the caller's buffer.
    Line,
    /// EOF was hit before any bytes were read.
    Eof,
    /// EOF was hit mid-line (no trailing `\n`). Buffer holds the partial line.
    EofNoNewline,
    /// The per-line byte cap was exhausted before a `\n` was seen.
    Exceeded,
}

/// Read one line into `buf` with a hard upper bound of `max` bytes.
///
/// Uses `fill_buf`/`consume` directly so memory use is bounded to at most
/// `max` regardless of how the adversarial input is shaped. The terminating
/// `\n` is consumed from the reader but is not appended to `buf`.
/// Outcome of a single `poll_fill_buf` pass.
enum PassOutcome {
    /// Reader returned an empty slice — EOF.
    Eof,
    /// Chunk contained a newline and the preceding content fit under `max`.
    LineReady,
    /// The per-line cap was exhausted before a newline was reached.
    Exceeded,
    /// Chunk had no newline; content was appended. Loop again.
    KeepReading,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<BoundedLine> {
    use std::future::poll_fn;
    use std::task::Poll;

    buf.clear();
    loop {
        let outcome = poll_fn(|cx| {
            let chunk = match Pin::new(&mut *reader).poll_fill_buf(cx) {
                Poll::Ready(Ok(c)) => c,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            if chunk.is_empty() {
                return Poll::Ready(Ok(PassOutcome::Eof));
            }
            match chunk.iter().position(|b| *b == b'\n') {
                Some(pos) => {
                    let consume = pos + 1;
                    let outcome = if buf.len().saturating_add(pos) > max {
                        PassOutcome::Exceeded
                    } else {
                        buf.extend_from_slice(&chunk[..pos]);
                        PassOutcome::LineReady
                    };
                    Pin::new(&mut *reader).consume(consume);
                    Poll::Ready(Ok(outcome))
                }
                None => {
                    let len = chunk.len();
                    let outcome = if buf.len().saturating_add(len) > max {
                        PassOutcome::Exceeded
                    } else {
                        buf.extend_from_slice(chunk);
                        PassOutcome::KeepReading
                    };
                    Pin::new(&mut *reader).consume(len);
                    Poll::Ready(Ok(outcome))
                }
            }
        })
        .await?;

        match outcome {
            PassOutcome::Eof => {
                return Ok(if buf.is_empty() {
                    BoundedLine::Eof
                } else {
                    BoundedLine::EofNoNewline
                });
            }
            PassOutcome::LineReady => return Ok(BoundedLine::Line),
            PassOutcome::Exceeded => return Ok(BoundedLine::Exceeded),
            PassOutcome::KeepReading => continue,
        }
    }
}

/// Reads events from the log at `path` that have sequence number greater than `since`.
///
/// Events are 1-indexed: the first event in the file is seq 1. Sending `since: 0`
/// returns all events; `since: N` returns events N+1 onward.
///
/// Every line is bounded to [`MAX_LINE_BYTES`]. An `events.jsonl` with a line that
/// exceeds the cap is rejected with an error rather than buffered into memory.
pub async fn read_since(path: &Path, since: u64) -> Result<Vec<(u64, Event)>> {
    let file = tokio::fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::new();

    match read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BYTES).await? {
        BoundedLine::Eof => {
            return Err(ForgeError::Other(anyhow::anyhow!("event log is empty")));
        }
        BoundedLine::Exceeded => {
            return Err(ForgeError::Other(anyhow::anyhow!(
                "events.jsonl header line exceeds {MAX_LINE_BYTES} bytes"
            )));
        }
        BoundedLine::Line | BoundedLine::EofNoNewline => {}
    }
    let header = std::str::from_utf8(&line_buf).map_err(|_| {
        ForgeError::Other(anyhow::anyhow!("events.jsonl header is not valid UTF-8"))
    })?;
    if header != SCHEMA_HEADER {
        return Err(ForgeError::Other(anyhow::anyhow!(
            "schema header mismatch: expected {SCHEMA_HEADER:?}, got {header:?}"
        )));
    }

    let mut events = Vec::new();
    let mut seq = 0u64;
    loop {
        match read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::Exceeded => {
                return Err(ForgeError::Other(anyhow::anyhow!(
                    "events.jsonl line at seq {next} exceeds {MAX_LINE_BYTES} bytes",
                    next = seq + 1
                )));
            }
            BoundedLine::Line | BoundedLine::EofNoNewline => {
                seq += 1;
                if seq > since {
                    let line = std::str::from_utf8(&line_buf).map_err(|_| {
                        ForgeError::Other(anyhow::anyhow!(
                            "events.jsonl line at seq {seq} is not valid UTF-8"
                        ))
                    })?;
                    let event: Event = serde_json::from_str(line).map_err(|e| {
                        ForgeError::Other(anyhow::anyhow!("bad event at seq {seq}: {e}"))
                    })?;
                    events.push((seq, event));
                }
            }
        }
    }
    Ok(events)
}

/// Summary of a corruption-tolerant scan of `events.jsonl`.
///
/// F-748 review fix: used by `Session::resume` to recover from a daemon
/// that crashed mid-write — the tail line may be truncated (no `\n`) or
/// otherwise unparseable. The strict [`read_since`] errors out on the bad
/// line; this scanner instead reports how far the file is well-formed so
/// the caller can truncate the corrupt suffix and resume cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredTail {
    /// Number of well-formed events parsed before the first parse error
    /// (or EOF, when the file is fully clean). This is the seq of the
    /// last good event under the positional 1-indexed convention
    /// [`read_since`] uses (events count, header excluded).
    pub last_good_seq: u64,
    /// Byte offset of the end of the last well-formed line (immediately
    /// after the line's terminating `\n`). When the file is fully clean,
    /// equals the file's byte length. When the file is corrupt, equals
    /// the byte length to truncate to so the next [`EventLog::open`]
    /// reader sees a valid log with no garbage suffix.
    pub safe_byte_len: u64,
}

/// Walks `events.jsonl` line-by-line, parsing each event after the schema
/// header. Returns the last well-formed event's seq plus the byte offset
/// at which truncation should occur to leave the file in a clean state.
///
/// Unlike [`read_since`], this function does NOT error on a malformed
/// line; it stops at the first parse / framing failure and reports what
/// was already accepted. The caller is expected to act on the report —
/// typically by truncating the file via [`tokio::fs::File::set_len`].
///
/// `Err` is reserved for unrecoverable cases: I/O failure on read,
/// missing / corrupt schema header, a line exceeding [`MAX_LINE_BYTES`]
/// (CWE-770 — we won't buffer unbounded), or non-UTF-8 bytes in the
/// header. A malformed event-body line is recoverable; everything above
/// it on this list is not.
pub async fn recover_tail(path: &Path) -> Result<RecoveredTail> {
    let file = tokio::fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::new();

    let mut consumed_bytes: u64 = 0;

    // Header line — mandatory; a missing / mismatched header is not
    // recoverable because the file is not an `events.jsonl` we wrote.
    match read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BYTES).await? {
        BoundedLine::Eof => {
            return Err(ForgeError::Other(anyhow::anyhow!("event log is empty")));
        }
        BoundedLine::Exceeded => {
            return Err(ForgeError::Other(anyhow::anyhow!(
                "events.jsonl header line exceeds {MAX_LINE_BYTES} bytes"
            )));
        }
        BoundedLine::Line => {
            // Header is well-formed and terminated by '\n'.
            consumed_bytes += line_buf.len() as u64 + 1;
        }
        BoundedLine::EofNoNewline => {
            return Err(ForgeError::Other(anyhow::anyhow!(
                "events.jsonl header is missing trailing newline"
            )));
        }
    }
    let header = std::str::from_utf8(&line_buf).map_err(|_| {
        ForgeError::Other(anyhow::anyhow!("events.jsonl header is not valid UTF-8"))
    })?;
    if header != SCHEMA_HEADER {
        return Err(ForgeError::Other(anyhow::anyhow!(
            "schema header mismatch: expected {SCHEMA_HEADER:?}, got {header:?}"
        )));
    }

    let mut last_good_seq: u64 = 0;
    let mut safe_byte_len: u64 = consumed_bytes;
    loop {
        match read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::Exceeded => {
                // Adversarial / corrupt: a line over the cap is not a
                // recoverable shape — the kernel never wrote a partial
                // line bigger than `MAX_LINE_BYTES` from us. Stop at the
                // last clean offset.
                break;
            }
            BoundedLine::EofNoNewline => {
                // Truncated final line — no trailing '\n'. The next
                // emit would append after the missing newline and
                // produce malformed JSONL forever. Stop here and
                // signal truncation back to `safe_byte_len`.
                break;
            }
            BoundedLine::Line => {
                let line_len = line_buf.len() as u64 + 1; // include '\n'
                let line_str = match std::str::from_utf8(&line_buf) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                if serde_json::from_str::<Event>(line_str).is_err() {
                    break;
                }
                last_good_seq += 1;
                consumed_bytes += line_len;
                safe_byte_len = consumed_bytes;
            }
        }
    }

    Ok(RecoveredTail {
        last_good_seq,
        safe_byte_len,
    })
}

/// Returns the workspace root if `path` is nested under a `.forge` directory.
/// Returns `None` (and silently skips gitignore creation) for paths outside `.forge/`.
/// Production callers are expected to always nest EventLog files under `.forge/`.
fn forge_dir_parent(path: &Path) -> Option<&Path> {
    let mut cur = path.parent()?;
    loop {
        if cur.file_name()? == ".forge" {
            return cur.parent();
        }
        cur = cur.parent()?;
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        self.flush_task.abort();
    }
}
