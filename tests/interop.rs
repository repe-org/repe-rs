//! Cross-language wire-compatibility tests against the canonical C++ REPE
//! implementation (Glaze).
//!
//! The fixtures under `interop/fixtures/` are authentic REPE v1 frames emitted
//! by Glaze (see `interop/cpp/` and `docs/interop.md`); these tests assert that
//! `repe` agrees with bytes it did not author. For each fixture the manifest
//! records the expected decode, and we check three tiers plus an optional
//! fourth:
//!
//! 1. **Parse parity** — `Message::from_slice` reproduces every header field,
//!    the query, and both format codes.
//! 2. **Body decode** — the body decodes to the expected value through the
//!    format-appropriate API (JSON / BEVE object / BEVE typed numeric / UTF-8).
//! 3. **Byte-identity round-trip** — re-encoding the parsed message with
//!    `to_vec` reproduces the original frame exactly.
//! 4. **Encoder parity (from scratch)** — for fixtures whose byte layout is
//!    protocol-defined (not formatting-dependent), build the message purely
//!    from logical content and assert it equals the Glaze bytes. This is *not*
//!    asserted for JSON bodies: JSON whitespace / key order / number formatting
//!    is not fixed by REPE, so JSON parity is covered semantically by tiers 1-3
//!    rather than by demanding byte-identical output from serde_json.

use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::message::Message;
use repe::{REPE_SPEC, REPE_VERSION};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Manifest {
    repe_version: String,
    #[allow(dead_code)]
    glaze_version: String,
    #[allow(dead_code)]
    note: String,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    #[allow(dead_code)]
    description: String,
    id: u64,
    notify: u64,
    ec: u64,
    query_format: u64,
    body_format: u64,
    query_length: u64,
    body_length: u64,
    length: u64,
    query: String,
    /// "json" | "beve_struct" | "beve_f64" | "utf8" | "none"
    body_kind: String,
    /// Canonical JSON of the expected body (json / beve_struct / beve_f64).
    body_json: String,
    /// UTF-8 message (utf8 error fixtures).
    body_text: String,
    /// Whether tier 4 (from-scratch byte identity) should be asserted.
    encoder_parity: bool,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("interop/fixtures")
}

fn load_manifest() -> Manifest {
    let path = fixtures_dir().join("manifest.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn load_frame(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(format!("{name}.repe"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn glaze_fixtures_roundtrip_and_decode() {
    let manifest = load_manifest();
    assert_eq!(
        manifest.repe_version,
        REPE_VERSION.to_string(),
        "fixtures target a different REPE version than this crate"
    );
    assert!(!manifest.fixtures.is_empty(), "manifest has no fixtures");

    for f in &manifest.fixtures {
        let bytes = load_frame(&f.name);
        assert_eq!(
            bytes.len() as u64,
            f.length,
            "{}: fixture file size != header length",
            f.name
        );

        // Tier 1: parse parity.
        let msg = Message::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{}: from_slice failed: {e:?}", f.name));
        let h = &msg.header;
        assert_eq!(h.spec, REPE_SPEC, "{}: spec", f.name);
        assert_eq!(h.version, REPE_VERSION, "{}: version", f.name);
        assert_eq!(h.reserved, 0, "{}: reserved", f.name);
        assert_eq!(h.id, f.id, "{}: id", f.name);
        assert_eq!(u64::from(h.notify), f.notify, "{}: notify", f.name);
        assert_eq!(u64::from(h.ec), f.ec, "{}: ec", f.name);
        assert_eq!(
            u64::from(h.query_format),
            f.query_format,
            "{}: query_format",
            f.name
        );
        assert_eq!(
            u64::from(h.body_format),
            f.body_format,
            "{}: body_format",
            f.name
        );
        assert_eq!(h.query_length, f.query_length, "{}: query_length", f.name);
        assert_eq!(h.body_length, f.body_length, "{}: body_length", f.name);
        assert_eq!(h.length, f.length, "{}: length", f.name);
        assert_eq!(msg.query, f.query.as_bytes(), "{}: query bytes", f.name);

        // Tier 2: body decode.
        decode_and_check_body(f, &msg);

        // Tier 3: byte-identity round-trip.
        assert_eq!(
            msg.to_vec(),
            bytes,
            "{}: re-encoding the parsed message did not reproduce the fixture",
            f.name
        );

        // Tier 4: encoder parity from scratch (protocol-defined layouts only).
        if f.encoder_parity {
            let built = build_equivalent(f);
            assert_eq!(
                built.to_vec(),
                bytes,
                "{}: from-scratch repe-rs encode did not match the Glaze frame",
                f.name
            );
        }
    }
}

fn decode_and_check_body(f: &Fixture, msg: &Message) {
    match f.body_kind.as_str() {
        "none" => assert!(msg.body.is_empty(), "{}: expected empty body", f.name),
        "json" => {
            let got: serde_json::Value = msg
                .json_body()
                .unwrap_or_else(|e| panic!("{}: json_body: {e:?}", f.name));
            let want: serde_json::Value =
                serde_json::from_str(&f.body_json).expect("manifest body_json");
            assert_eq!(got, want, "{}: JSON body value mismatch", f.name);
        }
        "beve_struct" => {
            let got: serde_json::Value = msg
                .beve_body()
                .unwrap_or_else(|e| panic!("{}: beve_body: {e:?}", f.name));
            let want: serde_json::Value =
                serde_json::from_str(&f.body_json).expect("manifest body_json");
            assert_eq!(got, want, "{}: BEVE object body value mismatch", f.name);
        }
        "beve_f64" => {
            let got: Vec<f64> = msg
                .decode_typed_slice()
                .unwrap_or_else(|e| panic!("{}: decode_typed_slice: {e:?}", f.name));
            let want: Vec<f64> = serde_json::from_str(&f.body_json).expect("manifest body_json");
            assert_eq!(got, want, "{}: BEVE numeric body mismatch", f.name);
        }
        "utf8" => {
            assert!(msg.is_error(), "{}: expected an error message", f.name);
            assert_eq!(
                msg.error_message_utf8().as_deref(),
                Some(f.body_text.as_str()),
                "{}: error message body",
                f.name
            );
            assert_eq!(
                msg.error_code().map(u32::from).map(u64::from),
                Some(f.ec),
                "{}: error code mapping",
                f.name
            );
        }
        other => panic!("{}: unknown body_kind {other}", f.name),
    }
}

/// Rebuild a fixture's logical content with repe-rs's own builder, for the
/// fixtures whose wire bytes are fully protocol-defined.
fn build_equivalent(f: &Fixture) -> Message {
    let mut b = Message::builder().id(f.id);
    if f.notify == 1 {
        b = b.notify(true);
    }
    // query_format == 1 is JsonPointer; 0 (RawBinary) is the builder default.
    if f.query_format == u64::from(u16::from(QueryFormat::JsonPointer)) {
        b = b.query_format(QueryFormat::JsonPointer);
    }
    if !f.query.is_empty() {
        b = b.query_bytes(f.query.clone().into_bytes());
    }
    match f.body_kind.as_str() {
        "none" => {}
        "beve_f64" => {
            let nums: Vec<f64> = serde_json::from_str(&f.body_json).expect("manifest body_json");
            b = b.body_typed_slice(&nums);
        }
        "utf8" => {
            let code = ErrorCode::try_from(f.ec as u32)
                .unwrap_or_else(|v| panic!("{}: unmapped error code {v}", f.name));
            b = b
                .error_code(code)
                .body_bytes(f.body_text.clone().into_bytes())
                .body_format(BodyFormat::Utf8);
        }
        other => panic!(
            "{}: encoder_parity set for body_kind {other}, which has no protocol-defined encode",
            f.name
        ),
    }
    b.build()
}

/// A server built against an older version of a request schema: it declares
/// only the fields it knew about. The Glaze-authored `*_unknown_key` fixtures
/// carry these fields plus keys this struct never declared.
#[derive(Debug, Deserialize, PartialEq)]
struct DemoBodyV1 {
    name: String,
    count: i32,
    ratio: f64,
    active: bool,
    values: Vec<i32>,
}

/// The version-skew guarantee, pinned against bytes repe did not author: a
/// request body a newer client produced with object keys an older server never
/// declared decodes into that older server's struct with the unknown keys
/// ignored, not rejected. The `json_request_unknown_key` / `beve_request_unknown_key`
/// fixtures carry an interleaved scalar (`region`, between two known fields) and
/// a trailing nested object (`meta`), so both a mid-object skip-and-resync and a
/// whole-sub-object skip are exercised — over BEVE as well as JSON, where
/// skipping an unknown key is the non-trivial binary-format path. See
/// `docs/protocol.md` ("Schema Evolution").
#[test]
fn glaze_unknown_key_body_decodes_into_older_struct() {
    let expected = DemoBodyV1 {
        name: "sensor-7".into(),
        count: 42,
        ratio: -3.5,
        active: true,
        values: vec![1, 2, 3],
    };

    let json_msg = Message::from_slice(&load_frame("json_request_unknown_key"))
        .expect("parse json_request_unknown_key");
    let from_json: DemoBodyV1 = json_msg
        .json_body()
        .expect("older struct must decode a newer JSON body, ignoring unknown keys");
    assert_eq!(from_json, expected, "JSON unknown-key decode");

    let beve_msg = Message::from_slice(&load_frame("beve_request_unknown_key"))
        .expect("parse beve_request_unknown_key");
    let from_beve: DemoBodyV1 = beve_msg
        .beve_body()
        .expect("older struct must decode a newer BEVE body, ignoring unknown keys");
    assert_eq!(from_beve, expected, "BEVE unknown-key decode");
}

/// Every `.repe` file on disk must have a manifest entry and vice versa, so a
/// newly generated fixture can't silently escape coverage.
#[test]
fn manifest_and_fixture_files_agree() {
    let manifest = load_manifest();
    let mut manifest_names: Vec<&str> = manifest.fixtures.iter().map(|f| f.name.as_str()).collect();
    manifest_names.sort_unstable();

    let mut disk_names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("repe"))
                .then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .collect();
    disk_names.sort_unstable();

    assert_eq!(
        manifest_names,
        disk_names.iter().map(String::as_str).collect::<Vec<_>>(),
        "manifest.json and the .repe files on disk are out of sync"
    );
}
