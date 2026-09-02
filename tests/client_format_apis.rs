#![cfg(not(target_arch = "wasm32"))]

use repe::{BodyFormat, Client, Message, QueryFormat, read_message, write_message};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

// ---- wire fixtures ----

/// A tagged response, for tests driving several routes through one server.
#[derive(Default, Debug, PartialEq)]
struct Kind {
    kind: String,
    n: i64,
}
structio::object!(Kind { kind, n });

#[derive(Default, Debug)]
struct ReadPayload {
    kind: String,
    n: i64,
}
structio::object!(ReadPayload { kind, n });

fn json_response_for<T: structio::json::Write + ?Sized>(req: &Message, body: &T) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format(
            QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary),
        )
        .body_json(body)
        .build()
}

#[test]
fn sync_call_message_and_registry_read_send_empty_body() {
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

        let first_response = json_response_for(
            &first,
            &Kind {
                kind: "message".into(),
                n: 0,
            },
        );
        write_message(&mut writer, &first_response).expect("write first response");
        writer.flush().expect("flush first response");

        let second = read_message(&mut reader).expect("second request");
        assert_eq!(second.query_utf8(), "/second");
        assert_eq!(second.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(second.header.body_format, BodyFormat::RawBinary as u16);
        assert_eq!(second.header.body_length, 0);
        assert!(second.body.is_empty());

        let second_response = json_response_for(
            &second,
            &Kind {
                kind: "read".into(),
                n: 5,
            },
        );
        write_message(&mut writer, &second_response).expect("write second response");
        writer.flush().expect("flush second response");

        let third = read_message(&mut reader).expect("third request");
        assert_eq!(third.query_utf8(), "/third");
        assert_eq!(third.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(third.header.body_format, BodyFormat::RawBinary as u16);
        assert_eq!(third.header.body_length, 0);
        assert!(third.body.is_empty());

        let third_response = json_response_for(
            &third,
            &Kind {
                kind: "typed".into(),
                n: 7,
            },
        );
        write_message(&mut writer, &third_response).expect("write third response");
        writer.flush().expect("flush third response");
    });

    let client = Client::connect(addr).expect("connect client");

    let message = client.call_message("/first").expect("call_message");
    let decoded = message.json_body::<Kind>().expect("decode JSON");
    assert_eq!(decoded.kind, "message");

    let read_value: Kind = client
        .registry_read_typed("/second")
        .expect("registry_read_typed");
    assert_eq!(read_value.kind, "read");
    assert_eq!(read_value.n, 5);

    let typed: ReadPayload = client
        .registry_read_typed_with_timeout("/third", Duration::from_secs(1))
        .expect("registry_read_typed_with_timeout");
    assert_eq!(typed.kind, "typed");
    assert_eq!(typed.n, 7);

    server.join().expect("join server");
}

#[test]
fn sync_call_with_formats_sets_custom_wire_codes() {
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

    let client = Client::connect(addr).expect("connect client");
    let response = client
        .call_with_formats("/custom", 0x2222, Some(&[1, 2, 3]), 0x3333)
        .expect("call_with_formats");

    assert_eq!(response.header.body_format, BodyFormat::RawBinary as u16);
    assert_eq!(response.body, vec![9, 8]);

    server.join().expect("join server");
}

#[test]
fn sync_registry_read_typed_deserializes_target_type() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = BufWriter::new(stream);

        let request = read_message(&mut reader).expect("request");
        assert_eq!(request.query_utf8(), "/typed");
        assert_eq!(request.header.body_length, 0);

        let response = json_response_for(
            &request,
            &Kind {
                kind: "typed".into(),
                n: 11,
            },
        );
        write_message(&mut writer, &response).expect("write response");
        writer.flush().expect("flush response");
    });

    let client = Client::connect(addr).expect("connect client");
    let typed: ReadPayload = client
        .registry_read_typed("/typed")
        .expect("registry_read_typed");
    assert_eq!(typed.kind, "typed");
    assert_eq!(typed.n, 11);

    server.join().expect("join server");
}

#[test]
fn sync_notify_with_formats_sets_notify_and_empty_body() {
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

    let client = Client::connect(addr).expect("connect client");
    client
        .notify_with_formats("/notify", 0x4444, None, 0x5555)
        .expect("notify_with_formats");

    server.join().expect("join server");
}
