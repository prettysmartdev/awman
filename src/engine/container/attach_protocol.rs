//! The one attach wire protocol, shared by every attach server and client.
//!
//! Two attach endpoints exist and both speak this protocol:
//! - the builtin runtime's launcher-hosted attach server
//!   (`builtin/exec_bridge.rs`), which keeps stdout and stderr apart and ends
//!   a session with an exit status;
//! - the Apple rendezvous socket (`attach_socket.rs`), which serves a PTY and
//!   so only ever sends merged output.
//!
//! Frames are symmetric: `tag: u8`, `len: u32 LE`, `payload: [u8; len]`.
//!
//! | Tag | [`Tag`]    | Direction        | Payload                                |
//! |-----|------------|------------------|----------------------------------------|
//! | 0   | `Output`   | server → client  | merged PTY output bytes                |
//! | 1   | `Stdin`    | client → server  | raw stdin bytes                        |
//! | 2   | `Resize`   | client → server  | `cols: u16 LE, rows: u16 LE`           |
//! | 3   | `Stderr`   | server → client  | stderr bytes (builtin only)            |
//! | 4   | `Exit`     | server → client  | `i32 LE` exit status (builtin only)    |
//! | 5   | `Failed`   | server → client  | empty; the stream failed, no exit code |
//! | 6   | `Lagged`   | server → client  | empty; this client's output is gapped  |
//!
//! Tag numbers are defined once, here, and never reused. Builtin output is
//! sent as `Output` tag 0 (the builtin server's historical "stdout" tag, so
//! its wire format is unchanged). A lagging builtin client is told `Failed`,
//! never a clean `Exit`; the Apple server tells a lagging client `Lagged`.
//!
//! ## Compatibility
//!
//! Tags 0–5 keep the numbers the builtin protocol always used. The Apple
//! protocol's old "lagged" tag was 3, which is now `Stderr`; its lag tag moves
//! to 6. Endpoints are per-process, per-launch sockets, so a mismatch only
//! occurs when two awman builds of different versions meet. Every such
//! mismatch fails closed rather than misreading the stream: an old Apple
//! client reading the new `Lagged` (6) sees an invalid frame and errors; a new
//! Apple client reading an old server's lag frame (3, empty) sees `Stderr`,
//! which it also rejects as an invalid output frame. Builtin frames are
//! unchanged on the wire. Neither mismatch can end in a fabricated clean exit.
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound on one frame's payload. A larger frame is a protocol violation:
/// the reader refuses it before allocating and the writer refuses to send it.
pub(crate) const MAX_FRAME: usize = 1 << 20;
/// How long a server may spend writing one frame to a client before it gives
/// up on that client. Keeps one stalled client from pinning its task forever.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Frame discriminator. See the module table for numbers and directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Tag {
    Output = 0,
    Stdin = 1,
    Resize = 2,
    Stderr = 3,
    Exit = 4,
    Failed = 5,
    Lagged = 6,
}

impl Tag {
    pub(crate) fn from_u8(value: u8) -> Option<Tag> {
        Some(match value {
            0 => Tag::Output,
            1 => Tag::Stdin,
            2 => Tag::Resize,
            3 => Tag::Stderr,
            4 => Tag::Exit,
            5 => Tag::Failed,
            6 => Tag::Lagged,
            _ => return None,
        })
    }
}

fn invalid(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// The `Resize` payload: `cols: u16 LE, rows: u16 LE`.
pub(crate) fn resize_payload(cols: u16, rows: u16) -> [u8; 4] {
    let mut payload = [0u8; 4];
    payload[..2].copy_from_slice(&cols.to_le_bytes());
    payload[2..].copy_from_slice(&rows.to_le_bytes());
    payload
}

/// Parse a `Resize` payload; `None` when it is not exactly four bytes.
pub(crate) fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let [c0, c1, r0, r1] = *payload else {
        return None;
    };
    Some((u16::from_le_bytes([c0, c1]), u16::from_le_bytes([r0, r1])))
}

/// Write one frame. A payload above [`MAX_FRAME`] is refused so a peer is
/// never sent something its reader must reject.
pub(crate) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    tag: Tag,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(invalid("attach frame exceeds bound"));
    }
    writer.write_u8(tag as u8).await?;
    writer.write_u32_le(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

/// [`write_frame`] bounded by `timeout`.
pub(crate) async fn write_frame_bounded(
    writer: &mut (impl AsyncWrite + Unpin),
    tag: Tag,
    payload: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    tokio::time::timeout(timeout, write_frame(writer, tag, payload))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "attach client write timed out",
            )
        })?
}

/// Send `bytes` as one or more bounded frames of at most [`MAX_FRAME`] bytes.
/// An empty slice sends a single empty frame (terminal markers carry none).
pub(crate) async fn write_chunked_bounded(
    writer: &mut (impl AsyncWrite + Unpin),
    tag: Tag,
    bytes: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    if bytes.is_empty() {
        return write_frame_bounded(writer, tag, &[], timeout).await;
    }
    for chunk in bytes.chunks(MAX_FRAME) {
        write_frame_bounded(writer, tag, chunk, timeout).await?;
    }
    Ok(())
}

/// Read one frame. An unknown tag or a length above [`MAX_FRAME`] is an
/// `InvalidData` error, reported before any payload is allocated or read, so
/// callers can tell a protocol violation from a closed connection.
pub(crate) async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<(Tag, Vec<u8>)> {
    let tag = reader.read_u8().await?;
    let length = reader.read_u32_le().await? as usize;
    let tag = Tag::from_u8(tag).ok_or_else(|| invalid("unknown attach frame tag"))?;
    if length > MAX_FRAME {
        return Err(invalid("attach frame exceeds bound"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok((tag, bytes))
}

/// Whether a [`read_frame`] failure was a protocol violation (as opposed to
/// the connection simply ending).
pub(crate) fn is_protocol_violation(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::InvalidData
}
