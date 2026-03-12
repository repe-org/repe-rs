#![cfg(not(target_arch = "wasm32"))]

use repe::{AsyncClient, BodyFormat, Message, QueryFormat, read_message, write_message};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct ReadPayload {
    kind: String,
    n: i64,
}

fn json_response_for(req: &Message, body: &Value) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format(
            QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary),
        )
        .body_json(body)
        .expect("json body")
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_call_message_and_registry_read_send_empty_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = BufWriter::new(stream);

        let first = read_message(&mut reader).expect("first request");
        assert_eq!(first.query_utf8(), "/first");
        assert_eq!(first.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(first.header.body_format, BodyFormat::RawBinary as u16);
        assert_eq!(first.header.body_length, 0);
        assert!(first.body.is_empty());

        let first_response = json_response_for(&first, &json!({"kind": "message"}));
        write_message(&mut writer, &first_response).expect("write first response");
        writer.flush().expect("flush first response");

        let second = read_message(&mut reader).expect("second request");
        assert_eq!(second.query_utf8(), "/second");
        assert_eq!(second.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(second.header.body_format, BodyFormat::RawBinary as u16);
        assert_eq!(second.header.body_length, 0);
        assert!(second.body.is_empty());

        let second_response = json_response_for(&second, &json!({"kind": "read", "n": 5}));
        write_message(&mut writer, &second_response).expect("write second response");
        writer.flush().expect("flush second response");

        let third = read_message(&mut reader).expect("third request");
        assert_eq!(third.query_utf8(), "/third");
        assert_eq!(third.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(third.header.body_format, BodyFormat::RawBinary as u16);
        assert_eq!(third.header.body_length, 0);
        assert!(third.body.is_empty());

        let third_response = json_response_for(&third, &json!({"kind": "typed", "n": 7}));
        write_message(&mut writer, &third_response).expect("write third response");
        writer.flush().expect("flush third response");
    });

    let client = AsyncClient::connect(addr).await.expect("connect client");

    let message = client.call_message("/first").await.expect("call_message");
    let decoded = message.json_body::<Value>().expect("decode JSON");
    assert_eq!(decoded["kind"], "message");

    let read_value = client
        .registry_read("/second")
        .await
        .expect("registry_read");
    assert_eq!(read_value["kind"], "read");
    assert_eq!(read_value["n"], 5);

    let typed: ReadPayload = client
        .registry_read_typed_with_timeout("/third", Duration::from_secs(1))
        .await
        .expect("registry_read_typed_with_timeout");
    assert_eq!(typed.kind, "typed");
    assert_eq!(typed.n, 7);

    tokio::task::spawn_blocking(move || server.join().expect("join server"))
        .await
        .expect("await server join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_call_with_formats_sets_custom_wire_codes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = BufWriter::new(stream);

        let request = read_message(&mut reader).expect("request");
        assert_eq!(request.query_utf8(), "/custom");
        assert_eq!(request.header.query_format, 0x2222);
        assert_eq!(request.header.body_format, 0x3333);
        assert_eq!(request.body, vec![1, 2, 3]);

        let response = Message::builder()
            .id(request.header.id)
            .query_bytes(request.query.clone())
            .query_format_code(request.header.query_format)
            .body_bytes(vec![9, 8])
            .body_format(BodyFormat::RawBinary)
            .build();
        write_message(&mut writer, &response).expect("write response");
        writer.flush().expect("flush response");
    });

    let client = AsyncClient::connect(addr).await.expect("connect client");
    let response = client
        .call_with_formats("/custom", 0x2222, Some(&[1, 2, 3]), 0x3333)
        .await
        .expect("call_with_formats");

    assert_eq!(response.header.body_format, BodyFormat::RawBinary as u16);
    assert_eq!(response.body, vec![9, 8]);

    tokio::task::spawn_blocking(move || server.join().expect("join server"))
        .await
        .expect("await server join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_registry_read_typed_deserializes_target_type() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = BufWriter::new(stream);

        let request = read_message(&mut reader).expect("request");
        assert_eq!(request.query_utf8(), "/typed");
        assert_eq!(request.header.body_length, 0);

        let response = json_response_for(&request, &json!({"kind": "typed", "n": 11}));
        write_message(&mut writer, &response).expect("write response");
        writer.flush().expect("flush response");
    });

    let client = AsyncClient::connect(addr).await.expect("connect client");
    let typed: ReadPayload = client
        .registry_read_typed("/typed")
        .await
        .expect("registry_read_typed");
    assert_eq!(typed.kind, "typed");
    assert_eq!(typed.n, 11);

    tokio::task::spawn_blocking(move || server.join().expect("join server"))
        .await
        .expect("await server join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_notify_with_formats_sets_notify_and_empty_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream);

        let request = read_message(&mut reader).expect("notify request");
        assert_eq!(request.query_utf8(), "/notify");
        assert_eq!(request.header.notify, 1);
        assert_eq!(request.header.query_format, 0x4444);
        assert_eq!(request.header.body_format, 0x5555);
        assert_eq!(request.header.body_length, 0);
        assert!(request.body.is_empty());
    });

    let client = AsyncClient::connect(addr).await.expect("connect client");
    client
        .notify_with_formats("/notify", 0x4444, None, 0x5555)
        .await
        .expect("notify_with_formats");

    tokio::task::spawn_blocking(move || server.join().expect("join server"))
        .await
        .expect("await server join");
}
