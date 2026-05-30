//! Per-request allocation budget for the server hot path.
//!
//! These tests pin the number of heap allocations on the inbound→outbound
//! request path so that a regression (an extra per-request allocation) fails
//! loudly, and a deliberate reduction shows up as a test that must be updated
//! downward. They are the regression guard behind the ongoing effort to drive
//! repe toward fewer per-request allocations.
//!
//! A thread-local counting [`GlobalAlloc`] wraps the system allocator. The
//! counter is per-thread, so the measured closure (which runs entirely on the
//! test thread) is unaffected by allocations on other threads in the test
//! binary. `dealloc` is intentionally not counted; we budget *allocation
//! events* (`alloc` + `realloc`), which is what scales with request volume.
//!
//! The dispatch path is composed from public APIs because the internal
//! `route_request` is `pub(crate)`. The query-echo move the servers perform at
//! the dispatch boundary is replicated here via the public `Message`/`Header`
//! fields so the budget matches what a real server pays.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::io::{Cursor, Write};

use repe::server::Router;
use repe::{
    CallContext, HEADER_SIZE, Message, MessageView, QueryFormat, read_message, read_message_into,
    write_message, write_message_streaming,
};
use serde_json::json;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

// SAFETY: each method forwards to the system allocator unchanged and only
// additionally bumps a thread-local `Cell<usize>`, whose access allocates
// nothing (a `const`-initialized thread-local is a plain static read), so no
// reentrancy into the allocator is introduced.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Run `f` and return its result alongside the number of allocation events it
/// made on the current thread.
fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let start = ALLOCS.with(Cell::get);
    let out = f();
    let end = ALLOCS.with(Cell::get);
    (out, end - start)
}

fn request(path: &str, body: serde_json::Value) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(&body)
        .unwrap()
        .build()
        .to_vec()
}

/// Replicates the read → dispatch → query-echo-move → write cycle a server runs
/// per request, using only public APIs. `out` is reused across calls so its
/// allocation is paid once (at warm-up), matching a per-connection writer.
fn dispatch_cycle(router: &Router, wire: &[u8], path: &str, out: &mut Vec<u8>) {
    let mut reader = Cursor::new(wire);
    let req = read_message(&mut reader).expect("read");
    let handler = router.get(path).expect("handler");
    let ctx = CallContext::detached(path);
    let mut resp = handler.handle_with_ctx(&req, &ctx).expect("dispatch");
    // The server moves the request query into the (query-less) handler response
    // at the dispatch boundary; replicate that here.
    resp.query = req.query;
    resp.header.query_length = resp.query.len() as u64;
    resp.header.length =
        HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length;
    out.clear();
    write_message(out, &resp).expect("write");
}

/// The borrowing path: read into a reused buffer, parse a `MessageView`,
/// dispatch via `handle_view`, and write the response framing the query as a
/// borrowed slice of the read buffer (`write_message_streaming`). No owned
/// request `Message`, so the read query/body `Vec`s never exist.
fn dispatch_cycle_view(
    router: &Router,
    wire: &[u8],
    path: &str,
    buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) {
    let mut reader = Cursor::new(wire);
    read_message_into(&mut reader, buf).expect("read");
    let view = MessageView::from_slice(buf).expect("view");
    let handler = router.get(path).expect("handler");
    let ctx = CallContext::detached(path);
    let resp = handler.handle_view(&view, &ctx).expect("dispatch");
    out.clear();
    // The response is query-less; echo the query straight from the borrowed
    // view, so no query buffer is allocated on the response side either.
    write_message_streaming(out, resp.header, view.query, resp.body.len() as u64, |w| {
        w.write_all(&resp.body)
    })
    .expect("write");
}

/// The WebSocket inline pattern: parse a borrowed `MessageView` from the
/// tungstenite payload, dispatch via `handle_view`, then copy the borrowed query
/// into the response because the outbound channel carries owned messages written
/// later by the writer task. Replicates the (pub(crate)) `stamp_response_query`
/// step via the public `Message`/`Header` fields.
fn dispatch_cycle_ws_inline(router: &Router, wire: &[u8], path: &str) -> Message {
    let view = MessageView::from_slice_exact(wire).expect("view");
    let handler = router.get(path).expect("handler");
    let ctx = CallContext::detached(path);
    let mut resp = handler.handle_view(&view, &ctx).expect("dispatch");
    resp.query = view.query.to_vec();
    resp.header.query_length = resp.query.len() as u64;
    resp.header.length =
        HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length;
    resp
}

#[test]
fn framing_round_trip_allocates_only_query_and_body() {
    // The pure framing path, no dispatch: `read_message` allocates exactly one
    // `Vec` for the query and one for the body; the header is a stack array, and
    // `write_message` reuses the warmed `out` buffer. This is the framing budget
    // the zero-copy/borrowed-read work would later drive toward zero.
    let wire = request("/echo", json!({"x": 1}));
    let mut out = Vec::new();

    // Warm up `out` so its growth isn't charged to the measured call.
    {
        let mut reader = Cursor::new(&wire);
        let req = read_message(&mut reader).unwrap();
        write_message(&mut out, &req).unwrap();
    }

    let (_, allocs) = count_allocs(|| {
        let mut reader = Cursor::new(&wire);
        let req = read_message(&mut reader).unwrap();
        out.clear();
        write_message(&mut out, &req).unwrap();
        black_box(&out);
    });

    assert_eq!(
        allocs, 2,
        "read_message should allocate exactly the query + body Vecs; write reuses `out`"
    );
}

#[test]
fn json_dispatch_allocation_budget() {
    // Full read → dispatch → write for a small JSON echo (`{"x":1}`). Budget:
    //
    //   2  read_message: the query Vec + the body Vec
    //   2  serde_json decode of `{"x":1}` to Value: the map node + the "x" key
    //   1  serde_json encode of the response body to a Vec
    //   0  query echo (moved from the request, not cloned — PR #20)
    //   0  write_message (reuses the warmed `out` buffer)
    //   ----
    //   5
    //
    // The count includes serde_json's payload (de)serialization, so it is
    // payload-shaped and may shift if serde_json's allocation behavior changes;
    // what this guards is that repe's *framework* contribution does not grow. If
    // this fails after a serde_json update, re-baseline EXPECTED; if it fails
    // after a repe change, investigate the new (or removed) allocation.
    const EXPECTED: usize = 5;
    let router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
    let wire = request("/echo", json!({"x": 1}));
    let mut out = Vec::new();

    // Warm up the reused writer buffer.
    dispatch_cycle(&router, &wire, "/echo", &mut out);

    let (_, allocs) = count_allocs(|| dispatch_cycle(&router, &wire, "/echo", &mut out));

    assert_eq!(
        allocs, EXPECTED,
        "per-request allocation budget changed (was {EXPECTED}, now {allocs}); \
         a reduction is good news — update EXPECTED; an increase is a regression"
    );
}

#[test]
fn json_dispatch_view_allocation_budget() {
    // The borrowing read path for the same `{"x":1}` echo. The two read-side
    // Vecs (query + body) are gone — the read buffer is reused and the response
    // echoes the query as a borrowed slice — leaving only serde_json's payload
    // work:
    //
    //   0  read_message_into (reuses `buf`) + MessageView (borrows)
    //   2  serde_json decode of `{"x":1}` to Value
    //   1  serde_json encode of the response body
    //   0  query echo (borrowed from the view by the writer)
    //   0  write_message_streaming (reuses `out`)
    //   ----
    //   3   (down from 5 on the owned path)
    //
    // NOTE: this 5→3 win applies to `with_json`/`with_typed` routes, which
    // override `handle_view` to decode from the borrowed body. Context-aware,
    // struct, registry, and middleware-wrapped routes use the owning
    // `handle_view` default (`MessageView::to_message`) and keep the owned
    // path's 2 read allocations until they too are overridden.
    const EXPECTED: usize = 3;
    let router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
    let wire = request("/echo", json!({"x": 1}));
    let mut buf = Vec::new();
    let mut out = Vec::new();

    // Warm up both reused buffers.
    dispatch_cycle_view(&router, &wire, "/echo", &mut buf, &mut out);

    let (_, allocs) =
        count_allocs(|| dispatch_cycle_view(&router, &wire, "/echo", &mut buf, &mut out));

    assert_eq!(
        allocs, EXPECTED,
        "borrowing-path allocation budget changed (was {EXPECTED}, now {allocs})"
    );
}

#[test]
fn websocket_inline_dispatch_allocation_budget() {
    // The WebSocket inline path borrows the request body (no read-side body Vec),
    // but unlike the TCP/async path it must copy the query into the response
    // because the outbound channel carries owned messages framed later. So:
    //
    //   0  MessageView::from_slice_exact (borrows the payload)
    //   2  serde_json decode of `{"x":1}`
    //   1  serde_json encode of the response body
    //   1  query copy for the owned response (outbound channel)
    //   ----
    //   4   (down from 5 on the owned WebSocket path — the read-side body Vec)
    //
    // As with the TCP path, this applies to with_json/with_typed routes; other
    // handlers use the owning handle_view fallback.
    const EXPECTED: usize = 4;
    let router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
    let wire = request("/echo", json!({"x": 1}));

    // Warm up (router.get + handler internals touch no per-call statics here,
    // but keep the shape identical to the other budgets).
    let _ = dispatch_cycle_ws_inline(&router, &wire, "/echo");

    let (_, allocs) = count_allocs(|| {
        black_box(dispatch_cycle_ws_inline(&router, &wire, "/echo"));
    });

    assert_eq!(
        allocs, EXPECTED,
        "websocket-inline allocation budget changed (was {EXPECTED}, now {allocs})"
    );
}
