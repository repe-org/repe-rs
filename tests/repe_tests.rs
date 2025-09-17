use repe::message::{create_error_message, create_error_response_like, create_response};
use repe::*;

#[test]
fn header_roundtrip_and_validation() {
    let mut h = Header::new();
    h.id = 123;
    h.query_length = 5;
    h.body_length = 7;
    h.length = HEADER_SIZE as u64 + h.query_length + h.body_length;
    h.query_format = QueryFormat::JsonPointer as u16;
    h.body_format = BodyFormat::Json as u16;
    let bytes = h.encode();
    let h2 = Header::decode(&bytes).unwrap();
    assert_eq!(h2, h);
}

#[test]
fn message_roundtrip_json() {
    let msg = Message::builder()
        .id(42)
        .query_str("/status")
        .query_format(QueryFormat::JsonPointer)
        .body_json(&serde_json::json!({"ping": true}))
        .unwrap()
        .build();
    let bytes = msg.to_vec();
    let parsed = Message::from_slice(&bytes).unwrap();
    assert_eq!(parsed.header.id, 42);
    let v: serde_json::Value = parsed.json_body().unwrap();
    assert_eq!(v["ping"], true);
}

#[test]
fn error_message_utf8() {
    let err = create_error_message(ErrorCode::MethodNotFound, "no such method");
    assert!(err.is_error());
    assert_eq!(err.error_code(), Some(ErrorCode::MethodNotFound));
    assert_eq!(err.error_message_utf8().as_deref(), Some("no such method"));
}

#[test]
fn message_new_rejects_length_mismatch() {
    let mut header = Header::new();
    header.query_length = 4;
    header.body_length = 2;
    header.length = HEADER_SIZE as u64 + header.query_length + header.body_length;
    let err = Message::new(header, b"abc".to_vec(), b"z".to_vec()).unwrap_err();
    assert!(matches!(err, RepeError::LengthMismatch { .. }));
}

#[test]
fn message_builder_defaults_and_notify_flag() {
    let msg = Message::builder()
        .id(99)
        .notify(true)
        .query_bytes(vec![0x01, 0x02])
        .body_bytes(vec![0xAA])
        .build();

    assert_eq!(msg.header.id, 99);
    assert_eq!(msg.header.notify, 1);
    assert_eq!(msg.header.query_format, QueryFormat::RawBinary as u16);
    assert_eq!(msg.header.body_format, BodyFormat::RawBinary as u16);
    assert_eq!(msg.header.length, HEADER_SIZE as u64 + 2 + 1);
    assert_eq!(msg.query, vec![0x01, 0x02]);
    assert_eq!(msg.body, vec![0xAA]);
}

#[test]
fn create_error_response_like_copies_request_context() {
    let request = Message::builder()
        .id(7)
        .query_str("/ping")
        .query_format(QueryFormat::JsonPointer)
        .body_utf8("ping")
        .build();

    let err = create_error_response_like(&request, ErrorCode::Timeout, "slow");
    assert_eq!(err.header.id, request.header.id);
    assert_eq!(err.query, request.query);
    assert!(err.is_error());
    assert_eq!(err.error_code(), Some(ErrorCode::Timeout));
    assert_eq!(err.error_message_utf8().as_deref(), Some("slow"));
    assert_eq!(
        err.header.length,
        HEADER_SIZE as u64 + err.header.query_length + err.header.body_length
    );
}

#[test]
fn create_response_raw_binary_serializes_json() {
    let request = Message::builder()
        .id(11)
        .query_str("/sum")
        .query_format(QueryFormat::JsonPointer)
        .body_utf8("{}")
        .build();

    let payload = serde_json::json!({"ok": true});
    let resp = create_response(&request, &payload, BodyFormat::RawBinary).unwrap();

    assert_eq!(resp.header.body_format, BodyFormat::RawBinary as u16);
    assert_eq!(resp.header.id, request.header.id);
    assert_eq!(resp.query, request.query);
    assert_eq!(resp.body, serde_json::to_vec(&payload).unwrap());
}

#[test]
fn message_from_slice_errors_on_truncated_buffers() {
    let err = Message::from_slice(&[0u8; 10]).unwrap_err();
    assert!(matches!(err, RepeError::InvalidHeaderLength(10)));

    let good = Message::builder()
        .id(1)
        .query_str("/x")
        .query_format(QueryFormat::JsonPointer)
        .body_utf8("body")
        .build()
        .to_vec();
    let truncated = &good[..HEADER_SIZE + 1];
    let err = Message::from_slice(truncated).unwrap_err();
    assert!(matches!(err, RepeError::BufferTooSmall { .. }));
}
