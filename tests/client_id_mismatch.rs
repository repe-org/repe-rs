use repe::*;
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::thread;

#[test]
fn client_detects_id_mismatch() {
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
    });

    let mut client = client::Client::connect(addr).unwrap();
    let err = client
        .call_json("/x", &serde_json::json!({"a":1}))
        .unwrap_err();
    match err {
        RepeError::ResponseIdMismatch { .. } => {}
        other => panic!("unexpected: {other:?}"),
    }
    let _ = srv.join();
}

#[test]
fn client_notify_sets_flag_and_does_not_wait_for_response() {
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
        // No response sent back; ensure the client was a pure notify.
    });

    let mut client = client::Client::connect(addr).unwrap();
    client
        .notify_json("/notify", &serde_json::json!({"ok": true}))
        .unwrap();

    drop(client);
    srv.join().unwrap();
}
