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

/// Encode one frame to bytes, for asserting the exact wire layout.
#[cfg(test)]
pub(crate) fn encode_frame(tag: Tag, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag as u8);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_numbers_are_unique_and_stable() {
        let all = [
            (Tag::Output, 0),
            (Tag::Stdin, 1),
            (Tag::Resize, 2),
            (Tag::Stderr, 3),
            (Tag::Exit, 4),
            (Tag::Failed, 5),
            (Tag::Lagged, 6),
        ];
        for (tag, number) in all {
            assert_eq!(tag as u8, number);
            assert_eq!(Tag::from_u8(number), Some(tag));
        }
        assert_eq!(Tag::from_u8(7), None);
        assert_eq!(Tag::from_u8(255), None);
    }

    #[test]
    fn frames_are_length_prefixed_little_endian() {
        assert_eq!(
            encode_frame(Tag::Stdin, b"hi"),
            vec![1, 2, 0, 0, 0, b'h', b'i']
        );
        assert_eq!(resize_payload(0x0102, 0x0304), [2, 1, 4, 3]);
        assert_eq!(parse_resize(&[2, 1, 4, 3]), Some((0x0102, 0x0304)));
        assert_eq!(parse_resize(&[1, 2, 3]), None);
    }

    #[tokio::test]
    async fn frames_round_trip_every_tag_including_binary_and_empty_payloads() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let cases = [
            (Tag::Output, vec![0, 255, 13, 10]),
            (Tag::Stdin, vec![1]),
            (Tag::Resize, resize_payload(80, 24).to_vec()),
            (Tag::Stderr, b"err".to_vec()),
            (Tag::Exit, 37_i32.to_le_bytes().to_vec()),
            (Tag::Failed, Vec::new()),
            (Tag::Lagged, Vec::new()),
        ];
        for (tag, payload) in &cases {
            write_frame(&mut a, *tag, payload).await.unwrap();
            assert_eq!(read_frame(&mut b).await.unwrap(), (*tag, payload.clone()));
        }
    }

    #[tokio::test]
    async fn a_frame_at_the_cap_is_accepted_and_above_it_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(MAX_FRAME + 16);
        write_frame(&mut a, Tag::Output, &vec![7; MAX_FRAME])
            .await
            .unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap().1.len(), MAX_FRAME);

        // Writer refuses.
        let error = write_frame(&mut a, Tag::Output, &vec![0; MAX_FRAME + 1])
            .await
            .unwrap_err();
        assert!(is_protocol_violation(&error));

        // Reader refuses before reading a payload.
        a.write_u8(Tag::Output as u8).await.unwrap();
        a.write_u32_le((MAX_FRAME + 1) as u32).await.unwrap();
        let error = read_frame(&mut b).await.unwrap_err();
        assert!(is_protocol_violation(&error), "{error}");
    }

    #[tokio::test]
    async fn an_unknown_tag_is_a_protocol_violation_not_a_closed_connection() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[99, 0, 0, 0, 0]).await.unwrap();
        let error = read_frame(&mut b).await.unwrap_err();
        assert!(is_protocol_violation(&error));
        drop(a);
        let error = read_frame(&mut b).await.unwrap_err();
        assert!(!is_protocol_violation(&error), "EOF is not a violation");
    }

    #[tokio::test]
    async fn chunked_writes_split_at_the_cap_and_empty_sends_one_marker() {
        let (mut a, mut b) = tokio::io::duplex(2 * MAX_FRAME + 64);
        write_chunked_bounded(
            &mut a,
            Tag::Output,
            &vec![1; MAX_FRAME + 5],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap().1.len(), MAX_FRAME);
        assert_eq!(read_frame(&mut b).await.unwrap().1.len(), 5);
        write_chunked_bounded(&mut a, Tag::Failed, &[], Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), (Tag::Failed, Vec::new()));
    }

    #[tokio::test]
    async fn a_stalled_peer_times_out_a_bounded_write() {
        let (mut a, _b) = tokio::io::duplex(8);
        let error =
            write_frame_bounded(&mut a, Tag::Output, &[42; 1024], Duration::from_millis(20))
                .await
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}
