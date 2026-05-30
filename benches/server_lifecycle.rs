//! Full server request-lifecycle throughput.
//!
//! Measures the per-request cost a server pays end-to-end, minus the socket
//! syscalls: read a framed request off an in-memory cursor, route + dispatch
//! it, echo the query, and write the framed response into a reused buffer.
//!
//! Composed from public APIs because the internal `route_request` is
//! `pub(crate)`; the dispatch-boundary query-echo move (the server moves the
//! request query into the handler's query-less response) is replicated via the
//! public `Message`/`Header` fields so the measured work matches a real server.
//!
//! Pairs with `tests/allocations.rs`, which pins the allocation count of the
//! same cycle. Two handler shapes are covered: a `serde_json::Value` echo
//! (`with_json`, the Value round-trip path) and a typed handler (`with_typed`,
//! which deserializes straight into a struct and skips the Value round-trip).

use criterion::{Criterion, criterion_group, criterion_main};
use repe::server::Router;
use repe::{
    CallContext, HEADER_SIZE, Message, MessageView, QueryFormat, read_message, read_message_into,
    write_message, write_message_streaming,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::hint::black_box;
use std::io::{Cursor, Write};

#[derive(Serialize, Deserialize)]
struct SumIn {
    a: i64,
    b: i64,
}

#[derive(Serialize, Deserialize)]
struct SumOut {
    sum: i64,
}

fn frame(path: &str, body: &serde_json::Value) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .unwrap()
        .build()
        .to_vec()
}

fn run_cycle(router: &Router, wire: &[u8], path: &str, out: &mut Vec<u8>) {
    let mut reader = Cursor::new(wire);
    let req = read_message(&mut reader).expect("read");
    let handler = router.get(path).expect("handler");
    let ctx = CallContext::detached(path);
    let mut resp = handler.handle_with_ctx(&req, &ctx).expect("dispatch");
    // Replicate the dispatch-boundary query-echo move.
    resp.query = req.query;
    resp.header.query_length = resp.query.len() as u64;
    resp.header.length = HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length;
    out.clear();
    write_message(out, &resp).expect("write");
}

/// The borrowing path: read into a reused buffer, parse a `MessageView`,
/// dispatch via `handle_view`, and frame the response with the query echoed as
/// a borrowed slice of the read buffer. No owned request `Message`.
fn run_cycle_view(router: &Router, wire: &[u8], path: &str, buf: &mut Vec<u8>, out: &mut Vec<u8>) {
    let mut reader = Cursor::new(wire);
    read_message_into(&mut reader, buf).expect("read");
    let view = MessageView::from_slice(buf).expect("view");
    let handler = router.get(path).expect("handler");
    let ctx = CallContext::detached(path);
    let resp = handler.handle_view(&view, &ctx).expect("dispatch");
    out.clear();
    write_message_streaming(out, resp.header, view.query, resp.body.len() as u64, |w| {
        w.write_all(&resp.body)
    })
    .expect("write");
}

fn bench_server_lifecycle(c: &mut Criterion) {
    let json_router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
    let typed_router = Router::new()
        .with_typed::<SumIn, SumOut, _>("/sum", |i: SumIn| Ok(SumOut { sum: i.a + i.b }));

    let json_wire = frame("/echo", &json!({"x": 1, "y": "hello"}));
    let typed_wire = frame("/sum", &json!({"a": 2, "b": 3}));
    let mut out = Vec::with_capacity(256);

    let mut group = c.benchmark_group("server_lifecycle");
    group.bench_function("json_echo", |b| {
        b.iter(|| run_cycle(black_box(&json_router), black_box(&json_wire), "/echo", &mut out))
    });
    group.bench_function("typed_sum", |b| {
        b.iter(|| run_cycle(black_box(&typed_router), black_box(&typed_wire), "/sum", &mut out))
    });
    group.finish();

    // Borrowing path: same work, reusable read buffer + MessageView dispatch.
    let mut buf = Vec::with_capacity(256);
    let mut view_group = c.benchmark_group("server_lifecycle_view");
    view_group.bench_function("json_echo", |b| {
        b.iter(|| {
            run_cycle_view(
                black_box(&json_router),
                black_box(&json_wire),
                "/echo",
                &mut buf,
                &mut out,
            )
        })
    });
    view_group.bench_function("typed_sum", |b| {
        b.iter(|| {
            run_cycle_view(
                black_box(&typed_router),
                black_box(&typed_wire),
                "/sum",
                &mut buf,
                &mut out,
            )
        })
    });
    view_group.finish();
}

criterion_group!(benches, bench_server_lifecycle);
criterion_main!(benches);
