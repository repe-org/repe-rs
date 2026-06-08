//! End-to-end tests for the typed-numeric body fast path: the bulk
//! `body_typed_slice` / `body_complex_slice` encoders, the `decode_typed_slice` /
//! `decode_complex_slice` decoders, and the `write_message_typed_slice` framing
//! convenience. The central guarantees are that the bulk path is byte-for-byte
//! interchangeable with the serde (`body_beve`) path and that it round-trips.

use repe::constants::BodyFormat;
use repe::message::Message;
use repe::{Complex, Header, write_message, write_message_typed_slice};

#[test]
fn typed_slice_roundtrips_and_sets_beve_format() {
    let data = vec![1.0f64, -2.5, 3.25, 1e9, f64::MIN, f64::MAX];
    let msg = Message::builder().body_typed_slice(&data).build();

    assert_eq!(msg.header.body_format, BodyFormat::Beve as u16);
    let back: Vec<f64> = msg.decode_typed_slice().unwrap();
    assert_eq!(back, data);
}

#[test]
fn typed_slice_bytes_identical_to_serde_body() {
    // The bulk encoder must produce the same wire bytes as `body_beve(&Vec<T>)`,
    // so a bulk sender and a serde receiver (and vice versa) interoperate.
    macro_rules! check {
        ($t:ty, $vals:expr) => {{
            let data: Vec<$t> = $vals;
            let bulk = Message::builder().body_typed_slice(&data).build();
            let serde = Message::builder().body_beve(&data).unwrap().build();
            assert_eq!(
                bulk.body,
                serde.body,
                "body bytes differ for {}",
                std::any::type_name::<$t>()
            );
            // Cross-decode: bulk bytes via serde, serde bytes via the bulk reader.
            let via_serde: Vec<$t> = serde.beve_body().unwrap();
            assert_eq!(via_serde, data);
            let via_bulk: Vec<$t> = bulk.decode_typed_slice().unwrap();
            assert_eq!(via_bulk, data);
            let cross: Vec<$t> = serde.decode_typed_slice().unwrap();
            assert_eq!(cross, data);
        }};
    }
    check!(u8, vec![0, 1, 2, 255]);
    check!(u16, vec![0, 1024, 65535]);
    check!(u32, vec![0, 1, 1_000_000]);
    check!(u64, vec![0, 1, u64::MAX]);
    check!(i8, vec![-128, -1, 0, 127]);
    check!(i32, vec![-1, 0, 1_000_000]);
    check!(i64, vec![i64::MIN, -1, 0, i64::MAX]);
    check!(f32, vec![1.0, -2.5, 3.25, -0.0]);
    check!(f64, vec![1.0, -2.5, 3.25, 1e9]);
}

#[test]
fn empty_typed_slice_roundtrips() {
    let msg = Message::builder().body_typed_slice::<f64>(&[]).build();
    assert_eq!(msg.header.body_format, BodyFormat::Beve as u16);
    let back: Vec<f64> = msg.decode_typed_slice().unwrap();
    assert!(back.is_empty());
}

#[test]
fn complex_slice_roundtrips_and_matches_serde() {
    let data = vec![
        Complex {
            re: 1.0f64,
            im: -2.0,
        },
        Complex { re: 3.5, im: 4.25 },
        Complex { re: -0.0, im: 1e9 },
    ];
    let bulk = Message::builder().body_complex_slice(&data).build();
    let serde = Message::builder().body_beve(&data).unwrap().build();

    assert_eq!(bulk.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(bulk.body, serde.body, "complex body bytes differ");

    let back: Vec<Complex<f64>> = bulk.decode_complex_slice().unwrap();
    assert_eq!(back, data);
    // serde-encoded complex bytes decode through the bulk reader too.
    let cross: Vec<Complex<f64>> = serde.decode_complex_slice().unwrap();
    assert_eq!(cross, data);
}

#[test]
fn write_message_typed_slice_frames_identically_to_owned() {
    // The streaming framing helper must emit byte-for-byte the same wire frame as
    // building a `body_typed_slice` message and writing it with `write_message`.
    let data: Vec<f64> = (0..1000).map(|i| i as f64 * 0.5).collect();
    let query = b"/sensors/raw";

    let mut streamed = Vec::new();
    write_message_typed_slice(&mut streamed, Header::new(), query, &data).unwrap();

    let owned = Message::builder()
        .query_bytes(query.to_vec())
        .body_typed_slice(&data)
        .build();
    let mut framed = Vec::new();
    write_message(&mut framed, &owned).unwrap();

    assert_eq!(streamed, framed);

    // ...and the streamed frame parses back to the original values.
    let parsed = Message::from_slice(&streamed).unwrap();
    assert_eq!(parsed.query, query);
    let back: Vec<f64> = parsed.decode_typed_slice().unwrap();
    assert_eq!(back, data);
}

#[test]
fn decode_typed_slice_rejects_non_beve_body() {
    // A JSON body is not a BEVE typed array; decode must report the format
    // mismatch as `UnexpectedBodyFormat`, not attempt a bulk read.
    let msg = Message::builder()
        .body_json(&vec![1.0f64, 2.0])
        .unwrap()
        .build();
    let err = msg.decode_typed_slice::<f64>().unwrap_err();
    assert!(
        matches!(
            err,
            repe::RepeError::UnexpectedBodyFormat {
                expected: BodyFormat::Beve,
                ..
            }
        ),
        "expected UnexpectedBodyFormat, got {err:?}"
    );
}

#[test]
fn decode_typed_slice_rejects_wrong_element_type() {
    // Decoding an f64 array as f32 is a type mismatch (wrong width), surfaced as a
    // BEVE error rather than a silent reinterpretation.
    let msg = Message::builder()
        .body_typed_slice(&[1.0f64, 2.0, 3.0])
        .build();
    let err = msg.decode_typed_slice::<f32>().unwrap_err();
    assert!(
        matches!(err, repe::RepeError::Beve(_)),
        "expected Beve error, got {err:?}"
    );
}
