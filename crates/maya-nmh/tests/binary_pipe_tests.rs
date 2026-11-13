use byteorder::{ByteOrder, NativeEndian};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[test]
fn test_binary_stdio_piped_ping() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_maya-nmh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn maya-nmh");

    let req = serde_json::json!({ "action": "ping" });
    let req_bytes = serde_json::to_vec(&req).expect("serialize req");

    let mut stdin_data = Vec::new();
    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, req_bytes.len() as u32);
    stdin_data.extend_from_slice(&len_buf);
    stdin_data.extend_from_slice(&req_bytes);

    {
        let mut stdin = child.stdin.take().expect("stdin take");
        stdin.write_all(&stdin_data).expect("write to stdin");
        stdin.flush().expect("flush stdin");
        // Dropping stdin sends EOF to maya-nmh
    }

    let mut stdout = child.stdout.take().expect("stdout take");
    let mut resp_len_buf = [0u8; 4];
    stdout
        .read_exact(&mut resp_len_buf)
        .expect("read length prefix from stdout");
    let resp_len = NativeEndian::read_u32(&resp_len_buf) as usize;
    assert!(resp_len > 0);

    let mut resp_payload = vec![0u8; resp_len];
    stdout
        .read_exact(&mut resp_payload)
        .expect("read response payload");

    let resp_val: serde_json::Value =
        serde_json::from_slice(&resp_payload).expect("parse response json");

    assert_eq!(resp_val["status"], "ok");
    assert_eq!(resp_val["success"], true);
    assert_eq!(resp_val["data"]["service"], "maya-nmh");
    assert_eq!(resp_val["data"]["status"], "ready");

    let status = child.wait().expect("wait for child process");
    assert!(status.success());
}

#[test]
fn test_binary_stdio_invalid_action() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_maya-nmh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn maya-nmh");

    let req = serde_json::json!({ "action": "totally_unknown_action_test" });
    let req_bytes = serde_json::to_vec(&req).expect("serialize req");

    let mut stdin_data = Vec::new();
    let mut len_buf = [0u8; 4];
    NativeEndian::write_u32(&mut len_buf, req_bytes.len() as u32);
    stdin_data.extend_from_slice(&len_buf);
    stdin_data.extend_from_slice(&req_bytes);

    {
        let mut stdin = child.stdin.take().expect("stdin take");
        stdin.write_all(&stdin_data).expect("write to stdin");
        stdin.flush().expect("flush stdin");
    }

    let mut stdout = child.stdout.take().expect("stdout take");
    let mut resp_len_buf = [0u8; 4];
    stdout
        .read_exact(&mut resp_len_buf)
        .expect("read length prefix");
    let resp_len = NativeEndian::read_u32(&resp_len_buf) as usize;

    let mut resp_payload = vec![0u8; resp_len];
    stdout.read_exact(&mut resp_payload).expect("read response");

    let resp_val: serde_json::Value = serde_json::from_slice(&resp_payload).expect("parse json");
    assert_eq!(resp_val["status"], "error");
    assert_eq!(resp_val["success"], false);

    let status = child.wait().expect("wait for child");
    assert!(status.success());
}
