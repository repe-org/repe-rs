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
//! same cycle.
//!
//! Two body shapes are covered, both through `with_typed`: an echo of a
//! two-member object and a call that reads two numbers and returns one. This
//! used to compare a `serde_json::Value` round trip against a typed handler,
//! which is no longer a comparison anyone can make — there is no document
//! model, so every route reads straight into its declared parameter. What is
//! left to measure is the framing and dispatch cost itself, and how it moves
//! with the body.

use benchit::Bench;
use repe::server::Router;
use repe::{
    CallContext, HEADER_SIZE, Message, MessageView, QueryFormat, read_message, read_message_into,
    write_message, write_message_streaming,
};
use std::hint::black_box;
use std::io::{Cursor, Write};

#[derive(Default)]
struct SumIn {
    a: i64,
    b: i64,
}
structio::object!(SumIn { a, b });

#[derive(Default)]
struct SumOut {
    sum: i64,
}
structio::object!(SumOut { sum });

/// The echo route's body: one number and one string, so the measurement covers
/// both a scalar and an allocating member.
#[derive(Default)]
struct EchoBody {
    x: i64,
    y: String,
}
structio::object!(EchoBody { x, y });

fn frame<T: structio::json::Write + ?Sized>(path: &str, body: &T) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
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

fn main() {
    let mut bench = Bench::from_args();

    let echo_router = Router::new().with_typed("/echo", |v: EchoBody| Ok(v));
    let typed_router = Router::new()
        .with_typed::<SumIn, SumOut, _>("/sum", |i: SumIn| Ok(SumOut { sum: i.a + i.b }));

    let echo_wire = frame(
        "/echo",
        &EchoBody {
            x: 1,
            y: "hello".into(),
        },
    );
    let typed_wire = frame("/sum", &SumIn { a: 2, b: 3 });

    // Every case in a group is live at once, so a single shared response buffer
    // would be two simultaneous `&mut` borrows. One buffer per case instead:
    // each is still reused across that case's own iterations, which is the
    // property the benchmark is measuring.
    let mut echo_out = Vec::with_capacity(256);
    let mut typed_out = Vec::with_capacity(256);

    let mut group = bench.group("server_lifecycle");
    group.bench("json_echo", |b| {
        b.iter(|| {
            run_cycle(
                black_box(&echo_router),
                black_box(&echo_wire),
                "/echo",
                &mut echo_out,
            )
        })
    });
    group.bench("typed_sum", |b| {
        b.iter(|| {
            run_cycle(
                black_box(&typed_router),
                black_box(&typed_wire),
                "/sum",
                &mut typed_out,
            )
        })
    });
    group.finish();

    // Borrowing path: same work, reusable read buffer + MessageView dispatch.
    let mut json_buf = Vec::with_capacity(256);
    let mut typed_buf = Vec::with_capacity(256);

    let mut view_group = bench.group("server_lifecycle_view");
    view_group.bench("json_echo", |b| {
        b.iter(|| {
            run_cycle_view(
                black_box(&echo_router),
                black_box(&echo_wire),
                "/echo",
                &mut json_buf,
                &mut echo_out,
            )
        })
    });
    view_group.bench("typed_sum", |b| {
        b.iter(|| {
            run_cycle_view(
                black_box(&typed_router),
                black_box(&typed_wire),
                "/sum",
                &mut typed_buf,
                &mut typed_out,
            )
        })
    });
    view_group.finish();
}
