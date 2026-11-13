use byteorder::{ByteOrder, NativeEndian};
use maya_nmh::{
    read_message, read_message_sync, write_message, write_message_sync, MAX_MESSAGE_SIZE,
};
use serde_json::json;
use std::io::Cursor;

#[test]
fn test_multiple_sequential_messages() {
    let mut stream = Vec::new();

    let msgs = vec![
        json!({"action": "ping"}),
        json!({"action": "download", "url": "https://example.com/file1.zip"}),
        json!({"action": "status", "id": "job-abc"}),
        json!({"action": "cancel", "id": "job-xyz"}),
    ];

    for msg in &msgs {
        write_message_sync(&mut stream, msg).expect("write failed");
    }

    let mut cursor = Cursor::new(stream);
    for expected in &msgs {
        let actual = read_message_sync(&mut cursor)
            .expect("read failed")
            .expect("expected message");
        assert_eq!(&actual, expected);
    }

    // Now at EOF
    let eof = read_message_sync(&mut cursor).expect("read at eof failed");
    assert_eq!(eof, None);
}

#[tokio::test]
async fn test_async_multiple_sequential_messages() {
    let mut stream = Vec::new();

    let msgs = vec![
        json!({"step": 1, "data": "hello"}),
        json!({"step": 2, "nested": {"key": [1, 2, 3]}}),
        json!({"step": 3, "done": true}),
    ];

    for msg in &msgs {
        write_message(&mut stream, msg).await.expect("write failed");
    }

    let mut slice = &stream[..];
    for expected in &msgs {
        let actual = read_message(&mut slice)
            .await
            .expect("read failed")
            .expect("expected message");
        assert_eq!(&actual, expected);
    }

    let eof = read_message(&mut slice).await.expect("eof check");
    assert_eq!(eof, None);
}

#[test]
fn test_large_message_payload() {
    let large_string = "x".repeat(256 * 1024); // 256 KB
    let msg = json!({
        "action": "download",
        "url": "https://example.com/large",
        "padding": large_string
    });

    let mut stream = Vec::new();
    write_message_sync(&mut stream, &msg).expect("write large failed");

    let mut cursor = Cursor::new(stream);
    let read_back = read_message_sync(&mut cursor)
        .expect("read large failed")
        .expect("expected message");

    assert_eq!(read_back["padding"], msg["padding"]);
}

#[test]
fn test_invalid_json_body() {
    let bad_json = b"{\"action\": [unclosed array";
    let mut stream = Vec::new();

    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, bad_json.len() as u32);
    stream.extend_from_slice(&len_buf);
    stream.extend_from_slice(bad_json);

    let mut cursor = Cursor::new(stream);
    let res = read_message_sync(&mut cursor);
    assert!(res.is_err());
}

#[test]
fn test_max_message_boundary() {
    // Exact limit should be accepted
    let exact_limit = MAX_MESSAGE_SIZE;
    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, exact_limit as u32);

    let mut cursor = Cursor::new(len_buf);
    // Should fail with UnexpectedEof because payload isn't there, NOT MessageTooLarge
    let res = read_message_sync(&mut cursor);
    assert!(matches!(res, Err(maya_nmh::NmhError::UnexpectedEof)));

    // 1 byte over limit should immediately reject with MessageTooLarge
    NativeEndian::write_u32(&mut len_buf, (exact_limit + 1) as u32);
    let mut cursor = Cursor::new(len_buf);
    let res = read_message_sync(&mut cursor);
    assert!(matches!(
        res,
        Err(maya_nmh::NmhError::MessageTooLarge(_, _))
    ));
}
