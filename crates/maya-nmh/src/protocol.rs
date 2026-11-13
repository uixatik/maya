//! Chrome/Firefox Native Messaging Host standard protocol message framing.
//!
//! Native Messaging specification:
//! - Each message is serialized using JSON, UTF-8 encoded.
//! - Preceded by a 32-bit message length in native byte order.
//! - Maximum message length: 1 MB (Chrome) / up to 4 GB (Firefox); capped here at 4 MB for safety.

use byteorder::{ByteOrder, NativeEndian};
use std::io::{Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::NmhError;

/// Default maximum allowable message size (4 MB).
pub const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// Read a native-messaging framed JSON message from an asynchronous stream.
///
/// Returns `Ok(Some(value))` when a message is successfully parsed,
/// `Ok(None)` on clean EOF before length bytes are read,
/// or `Err(NmhError)` on premature EOF, framing errors, or invalid JSON.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<serde_json::Value>, NmhError> {
    let mut len_buf = [0u8; 4];
    let mut total_read = 0;

    // Read 4-byte length prefix
    while total_read < 4 {
        match reader.read(&mut len_buf[total_read..]).await {
            Ok(0) => {
                if total_read == 0 {
                    return Ok(None); // Clean EOF
                }
                return Err(NmhError::UnexpectedEof);
            }
            Ok(n) => total_read += n,
            Err(e) => return Err(NmhError::Io(e)),
        }
    }

    let length = NativeEndian::read_u32(&len_buf) as usize;
    if length > MAX_MESSAGE_SIZE {
        return Err(NmhError::MessageTooLarge(length, MAX_MESSAGE_SIZE));
    }

    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            NmhError::UnexpectedEof
        } else {
            NmhError::Io(e)
        }
    })?;

    let val = serde_json::from_slice(&payload)?;
    Ok(Some(val))
}

/// Write a native-messaging framed JSON message to an asynchronous stream.
///
/// Encodes `value` to UTF-8 JSON, prefixes with a 32-bit native-endian length,
/// writes both to `writer`, and flushes.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<(), NmhError> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(NmhError::MessageTooLarge(payload.len(), MAX_MESSAGE_SIZE));
    }

    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, payload.len() as u32);

    writer.write_all(&len_buf).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Synchronous reader for native-messaging frames.
///
/// Returns `Ok(Some(value))` on success, `Ok(None)` on clean EOF, or `Err(NmhError)`.
pub fn read_message_sync<R: Read>(reader: &mut R) -> Result<Option<serde_json::Value>, NmhError> {
    let mut len_buf = [0u8; 4];
    let mut total_read = 0;

    while total_read < 4 {
        match reader.read(&mut len_buf[total_read..]) {
            Ok(0) => {
                if total_read == 0 {
                    return Ok(None);
                }
                return Err(NmhError::UnexpectedEof);
            }
            Ok(n) => total_read += n,
            Err(e) => return Err(NmhError::Io(e)),
        }
    }

    let length = NativeEndian::read_u32(&len_buf) as usize;
    if length > MAX_MESSAGE_SIZE {
        return Err(NmhError::MessageTooLarge(length, MAX_MESSAGE_SIZE));
    }

    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            NmhError::UnexpectedEof
        } else {
            NmhError::Io(e)
        }
    })?;

    let val = serde_json::from_slice(&payload)?;
    Ok(Some(val))
}

/// Synchronous writer for native-messaging frames.
pub fn write_message_sync<W: Write>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<(), NmhError> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(NmhError::MessageTooLarge(payload.len(), MAX_MESSAGE_SIZE));
    }

    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, payload.len() as u32);

    writer.write_all(&len_buf)?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_sync_roundtrip() {
        let input = serde_json::json!({
            "action": "download",
            "url": "https://example.com/file.zip",
            "connections": 8
        });

        let mut buffer = Vec::new();
        write_message_sync(&mut buffer, &input).expect("write failed");

        assert!(buffer.len() >= 4);
        let len_prefix = NativeEndian::read_u32(&buffer[..4]) as usize;
        assert_eq!(len_prefix, buffer.len() - 4);

        let mut cursor = Cursor::new(buffer);
        let output = read_message_sync(&mut cursor)
            .expect("read failed")
            .expect("expected message");

        assert_eq!(input, output);
    }

    #[test]
    fn test_clean_eof() {
        let mut cursor = Cursor::new(Vec::new());
        let res = read_message_sync(&mut cursor).expect("read failed");
        assert_eq!(res, None);
    }

    #[test]
    fn test_unexpected_eof_in_header() {
        let mut cursor = Cursor::new(vec![0x01, 0x02]);
        let res = read_message_sync(&mut cursor);
        assert!(matches!(res, Err(NmhError::UnexpectedEof)));
    }

    #[test]
    fn test_unexpected_eof_in_body() {
        let mut buffer = vec![0x10, 0x00, 0x00, 0x00]; // 16 bytes expected
        buffer.extend_from_slice(b"{\"action\":"); // only 10 bytes
        let mut cursor = Cursor::new(buffer);
        let res = read_message_sync(&mut cursor);
        assert!(matches!(res, Err(NmhError::UnexpectedEof)));
    }

    #[test]
    fn test_oversized_message_rejected() {
        let mut buffer = [0u8; 4];
        NativeEndian::write_u32(&mut buffer, (MAX_MESSAGE_SIZE + 1) as u32);
        let mut cursor = Cursor::new(buffer);
        let res = read_message_sync(&mut cursor);
        assert!(matches!(res, Err(NmhError::MessageTooLarge(_, _))));
    }

    #[tokio::test]
    async fn test_async_roundtrip() {
        let input = serde_json::json!({
            "action": "status",
            "id": "job-12345"
        });

        let mut buffer = Vec::new();
        write_message(&mut buffer, &input)
            .await
            .expect("async write failed");

        let mut slice = &buffer[..];
        let output = read_message(&mut slice)
            .await
            .expect("async read failed")
            .expect("expected message");

        assert_eq!(input, output);
    }

    #[tokio::test]
    async fn test_async_clean_eof() {
        let mut empty = &[][..];
        let res = read_message(&mut empty).await.expect("async read failed");
        assert_eq!(res, None);
    }
}
