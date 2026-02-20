use repe::*;
use serde::{Deserialize, Serialize};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn client_unknown_response_id_fails_pending_request_without_hanging() {
    // Server: echoes response with wrong id (id + 1)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = thread::spawn(move || {
        let (stream, _addr) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let req = read_message(&mut reader).unwrap();
        let mut resp = Message::builder()
            .id(req.header.id + 1)
            .query_bytes(req.query.clone())
            .query_format(QueryFormat::try_from(req.header.query_format).unwrap())
            .error_code(ErrorCode::Ok)
            .body_json(&serde_json::json!({"ok": true}))
            .unwrap()
            .build();
        // Fix length field
        resp.header.length =
            (HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length) as u64;
        let mut writer = BufWriter::new(stream);
        write_message(&mut writer, &resp).unwrap();
        writer.flush().unwrap();
        thread::sleep(Duration::from_millis(200));
    });

    let client = client::Client::connect(addr).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = client.call_json("/x", &serde_json::json!({"a": 1}));
        let _ = done_tx.send(result);
    });

    let result = done_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("call_json should fail quickly instead of hanging");
    let err = result.unwrap_err();
    match err {
        RepeError::Io(io_err) => {
            assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                io_err.to_string().contains("unknown request id"),
                "error should mention unknown id, got: {io_err}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    worker.join().unwrap();
    srv.join().unwrap();
}

#[test]
fn client_preserves_structured_fatal_response_loop_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = thread::spawn(move || {
        let (stream, _addr) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let req = read_message(&mut reader).unwrap();

        let mut resp = Message::builder()
            .id(req.header.id)
            .query_bytes(req.query.clone())
            .query_format(QueryFormat::try_from(req.header.query_format).unwrap())
            .error_code(ErrorCode::Ok)
            .body_json(&serde_json::json!({"ok": true}))
            .unwrap()
            .build();
        resp.header.spec = 0;

        let mut writer = BufWriter::new(stream);
        write_message(&mut writer, &resp).unwrap();
        writer.flush().unwrap();
    });

    let client = client::Client::connect(addr).unwrap();
    let err = client
        .call_json("/fatal", &serde_json::json!({"a": 1}))
        .unwrap_err();
    match err {
        RepeError::InvalidSpec(0) => {}
        other => panic!("unexpected: {other:?}"),
    }

    srv.join().unwrap();
}

#[test]
fn client_notify_sets_flag_and_does_not_wait_for_response() {
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct NotifyPayload {
        ok: bool,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct BevePayload {
        value: i32,
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = thread::spawn(move || {
        let (stream, _addr) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let msg = read_message(&mut reader).unwrap();
        assert_eq!(msg.header.notify, 1);
        assert_eq!(msg.query_utf8(), "/notify");
        let body: serde_json::Value = serde_json::from_slice(&msg.body).unwrap();
        assert_eq!(body["ok"], true);
        let typed_msg = read_message(&mut reader).unwrap();
        assert_eq!(typed_msg.header.notify, 1);
        assert_eq!(typed_msg.query_utf8(), "/notify_typed_json");
        let typed_body: NotifyPayload = serde_json::from_slice(&typed_msg.body).unwrap();
        assert_eq!(typed_body, NotifyPayload { ok: true });

        let beve_msg = read_message(&mut reader).unwrap();
        assert_eq!(beve_msg.header.notify, 1);
        assert_eq!(beve_msg.header.body_format, BodyFormat::Beve as u16);
        assert_eq!(beve_msg.query_utf8(), "/notify_beve");
        let beve_body: BevePayload = beve_msg.beve_body().unwrap();
        assert_eq!(beve_body, BevePayload { value: 42 });
        // No response sent back; ensure the client was a pure notify for every request.
    });

    let client = client::Client::connect(addr).unwrap();
    client
        .notify_json("/notify", &serde_json::json!({"ok": true}))
        .unwrap();

    client
        .notify_typed_json("/notify_typed_json", &NotifyPayload { ok: true })
        .unwrap();

    client
        .notify_typed_beve("/notify_beve", &BevePayload { value: 42 })
        .unwrap();

    drop(client);
    srv.join().unwrap();
}
