//! A header's advertised lengths are attacker-controlled, and the framed total
//! derived from them is used as a slice bound. These pin that the derivation is
//! checked.
//!
//! Before this was bounded, `query_length = u64::MAX - 47` made
//! `48 + query_length + body_length` wrap to `0` — which the header's own
//! `length` field could then be set to match, so the frame decoded. Slicing the
//! query out of it computed `buf[48..0]` and panicked. Forty-eight bytes, no
//! body, and the connection thread was gone: the TCP and async servers do not
//! catch unwinds, and a plugin host built with `panic = "abort"` loses the whole
//! process.

use repe::constants::HEADER_SIZE;
use repe::{Header, Message, MessageView, RepeError};

/// A header whose `length` field is made to agree with whatever
/// `query_length` and `body_length` say, so the only thing left to reject the
/// frame is the overflow check itself.
fn self_consistent_header(query_length: u64, body_length: u64) -> [u8; HEADER_SIZE] {
    let mut header = Header::new();
    header.query_length = query_length;
    header.body_length = body_length;
    header.length = (HEADER_SIZE as u64)
        .wrapping_add(query_length)
        .wrapping_add(body_length);
    header.encode()
}

/// The lengths that make the framed total wrap. Each is paired with the
/// wrapped value it produces, so the case is legible rather than magic.
fn wrapping_lengths() -> Vec<(u64, u64)> {
    vec![
        // 48 + (u64::MAX - 47) == 0
        (u64::MAX - 47, 0),
        (0, u64::MAX - 47),
        // Split across both fields, so a check on either one alone misses it.
        (u64::MAX - 47, u64::MAX),
        // Wraps past zero to a small positive total.
        (u64::MAX, 48),
    ]
}

#[test]
fn a_wrapping_frame_total_is_refused_by_the_header() {
    for (query_length, body_length) in wrapping_lengths() {
        let bytes = self_consistent_header(query_length, body_length);
        match Header::decode(&bytes) {
            Err(RepeError::FrameLengthOverflow {
                query_length: q,
                body_length: b,
            }) => {
                assert_eq!((q, b), (query_length, body_length));
            }
            other => panic!(
                "expected FrameLengthOverflow for ({query_length}, {body_length}), got {other:?}"
            ),
        }
    }
}

#[test]
fn the_borrowing_parser_refuses_instead_of_slicing_backwards() {
    for (query_length, body_length) in wrapping_lengths() {
        let bytes = self_consistent_header(query_length, body_length);
        assert!(
            matches!(
                MessageView::from_slice(&bytes),
                Err(RepeError::FrameLengthOverflow { .. })
            ),
            "({query_length}, {body_length}) reached the slicing code"
        );
    }
}

#[test]
fn the_owning_parser_refuses_instead_of_slicing_backwards() {
    for (query_length, body_length) in wrapping_lengths() {
        let bytes = self_consistent_header(query_length, body_length);
        assert!(
            matches!(
                Message::from_slice(&bytes),
                Err(RepeError::FrameLengthOverflow { .. })
            ),
            "({query_length}, {body_length}) reached the slicing code"
        );
    }
}

#[test]
fn a_refusal_carries_the_invalid_header_code() {
    let bytes = self_consistent_header(u64::MAX - 47, 0);
    let err = Header::decode(&bytes).unwrap_err();
    assert_eq!(
        err.to_error_code(),
        repe::constants::ErrorCode::InvalidHeader
    );
}

#[test]
fn a_total_that_fits_u64_but_not_the_address_space_is_refused() {
    // Only reachable on a 32-bit target (wasm32 is the one that matters here);
    // on 64-bit every `u64` total that did not wrap is addressable, and the
    // frame is rejected later for being longer than the buffer instead.
    let query_length = u64::from(u32::MAX) + 1;
    let bytes = self_consistent_header(query_length, 0);
    let decoded = Header::decode(&bytes);
    if usize::BITS < 64 {
        assert!(matches!(
            decoded,
            Err(RepeError::FrameLengthOverflow { .. })
        ));
    } else {
        assert_eq!(decoded.unwrap().query_length, query_length);
    }
}

#[test]
fn an_ordinary_frame_still_round_trips() {
    let query = b"/counter".to_vec();
    let body = b"42".to_vec();
    let mut header = Header::new();
    header.query_length = query.len() as u64;
    header.body_length = body.len() as u64;
    header.length = HEADER_SIZE as u64 + header.query_length + header.body_length;

    let bytes = Message::new(header, query.clone(), body.clone())
        .unwrap()
        .to_vec();
    let view = MessageView::from_slice(&bytes).unwrap();
    assert_eq!(view.query, &query[..]);
    assert_eq!(view.body, &body[..]);
}

#[test]
fn a_mismatched_length_still_reports_a_mismatch() {
    // The overflow check runs first, so this pins that it did not swallow the
    // ordinary disagreement case it sits in front of.
    let mut header = Header::new();
    header.query_length = 8;
    header.body_length = 0;
    header.length = 999;
    assert!(matches!(
        Header::decode(&header.encode()),
        Err(RepeError::LengthMismatch { .. })
    ));
}

#[test]
fn advertising_lengths_that_would_wrap_reports_a_mismatch_rather_than_panicking() {
    // `Message::new` computes the framed total only to describe the caller's
    // own error. That arithmetic must not itself be the failure.
    let mut header = Header::new();
    header.query_length = u64::MAX;
    header.body_length = u64::MAX;
    assert!(matches!(
        Message::new(header, Vec::new(), Vec::new()),
        Err(RepeError::LengthMismatch { .. })
    ));
}
