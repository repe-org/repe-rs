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
    CallContext, Complex, HEADER_SIZE, Header, Message, MessageView, QueryFormat, read_message,
    read_message_into, write_message, write_message_complex_slice, write_message_streaming,
    write_message_typed_slice,
};

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

fn request<T: structio::json::Write + ?Sized>(path: &str, body: &T) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .build()
        .to_vec()
}

/// The body every dispatch-cycle measurement sends. One small field, so the
/// allocation count is about the framing and dispatch path rather than the
/// payload.
#[derive(Default)]
struct Point {
    x: i64,
}
structio::object!(Point { x });

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
    resp.header.length = HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length;
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
    resp.header.length = HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length;
    resp
}

#[test]
fn framing_round_trip_allocates_only_query_and_body() {
    // The pure framing path, no dispatch: `read_message` allocates exactly one
    // `Vec` for the query and one for the body; the header is a stack array, and
    // `write_message` reuses the warmed `out` buffer. This is the framing budget
    // the zero-copy/borrowed-read work would later drive toward zero.
    let wire = request("/echo", &Point { x: 1 });
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
    // this fails after a structio update, re-baseline EXPECTED; if it fails
    // after a repe change, investigate the new (or removed) allocation.
    const EXPECTED: usize = 4;
    let router = Router::new().with_typed("/echo", |v: Point| Ok(v));
    let wire = request("/echo", &Point { x: 1 });
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
    // echoes the query as a borrowed slice — leaving only the payload work:
    //
    //   0  read_message_into (reuses `buf`) + MessageView (borrows)
    //   0  decode of `{"x":1}` into the declared `Point`
    //   2  the response body Vec, and its one growth
    //   0  query echo (borrowed from the view by the writer)
    //   0  write_message_streaming (reuses `out`)
    //   ----
    //   2   (down from 3 under serde, and from 5 on the owned path)
    //
    // The decode is what went to zero. A declared type is read *into* an
    // existing value, so `{"x":1}` reaching a `Point` allocates nothing at all;
    // under serde the same body cost two, building a `Value` before anything
    // looked at it.
    //
    // NOTE: this applies to `with_typed` routes, which override `handle_view`
    // to decode from the borrowed body. Context-aware, struct, registry, and
    // middleware-wrapped routes use the owning `handle_view` default
    // (`MessageView::to_message`) and keep the owned path's 2 read allocations
    // until they too are overridden.
    const EXPECTED: usize = 2;
    let router = Router::new().with_typed("/echo", |v: Point| Ok(v));
    let wire = request("/echo", &Point { x: 1 });
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
fn typed_slice_body_is_single_allocation() {
    // The typed-numeric body fast path: building a message with a bulk
    // `body_typed_slice` body allocates exactly once. `to_vec_typed_slice`
    // reserves the exact wire size up front (header byte + SIZE prefix + payload)
    // and fills it with one `copy_nonoverlapping`, so there are no growth
    // reallocs; the builder's unset query is an empty (heap-free) `Vec` and
    // `build()`'s `Header` is a stack value. This is the single-allocation claim
    // behind the fast path — the serde `body_beve` path reallocs as its encode
    // buffer grows.
    let data: Vec<f64> = (0..4096).map(|i| i as f64).collect();

    // Touch the path once so any one-time lazy initialization is unmeasured.
    let _ = black_box(Message::builder().body_typed_slice(&data).build());

    let (_, allocs) = count_allocs(|| {
        black_box(Message::builder().body_typed_slice(&data).build());
    });

    assert_eq!(
        allocs, 1,
        "body_typed_slice should allocate exactly the body Vec once (no growth reallocs)"
    );
}

#[test]
fn write_message_typed_slice_does_not_materialize_the_body() {
    // Streaming a typed-slice body never builds the body as a `Vec`: the header
    // is a stack array, the query a borrowed slice, and the payload the slice
    // reinterpreted as bytes (little-endian) handed straight to the sink in one
    // `write_all`. structio's sink writer stages small values through a fixed
    // 8 KiB buffer and bypasses it for any block at least that large, so what
    // framing costs is that one fixed buffer and nothing that scales.
    //
    // Stated as independence from the payload rather than as a zero, because a
    // zero is not what the property is: a hundredfold larger body must not cost
    // a byte more. A budget proportional to the slice would mean the body is
    // being materialized after all, which is exactly the regression this
    // guards.
    let query = b"/sensors/raw";
    let mut out = Vec::new();

    let mut measure = |len: usize| {
        let data: Vec<f64> = (0..len).map(|i| i as f64).collect();
        // Warm the sink so its growth isn't charged to the measured call.
        write_message_typed_slice(&mut out, Header::new(), query, &data).unwrap();
        let (_, allocs) = count_allocs(|| {
            out.clear();
            write_message_typed_slice(&mut out, Header::new(), query, &data).unwrap();
            black_box(&out);
        });
        allocs
    };

    let small = measure(64);
    let large = measure(65_536);
    assert_eq!(
        small, large,
        "framing cost must not scale with the payload (64 elements: {small}, \
         65536 elements: {large})"
    );
    assert!(
        small <= 1,
        "framing a typed-slice body should cost at most structio's fixed sink \
         buffer, got {small} allocations"
    );
}

#[test]
fn write_message_complex_slice_does_not_materialize_the_body() {
    // The complex counterpart of
    // [`write_message_typed_slice_does_not_materialize_the_body`], and the same
    // property: the interleaved `(re, im)` payload is the slice reinterpreted
    // as bytes and handed to the sink in one write, so framing costs a fixed
    // amount however long the slice is.
    let query = b"/spectra/iq";
    let mut out = Vec::new();

    let mut measure = |len: usize| {
        let data: Vec<Complex<f64>> = (0..len)
            .map(|i| Complex {
                re: i as f64,
                im: -(i as f64),
            })
            .collect();
        write_message_complex_slice(&mut out, Header::new(), query, &data).unwrap();
        let (_, allocs) = count_allocs(|| {
            out.clear();
            write_message_complex_slice(&mut out, Header::new(), query, &data).unwrap();
            black_box(&out);
        });
        allocs
    };

    let small = measure(64);
    let large = measure(65_536);
    assert_eq!(
        small, large,
        "framing cost must not scale with the payload (64 elements: {small}, \
         65536 elements: {large})"
    );
    assert!(
        small <= 1,
        "framing a complex-slice body should cost at most structio's fixed sink \
         buffer, got {small} allocations"
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
    //   3   (down from 4 under serde: the decode into a declared type is free)
    //
    // As with the TCP path, this applies to `with_typed` routes; other handlers
    // use the owning `handle_view` fallback.
    const EXPECTED: usize = 3;
    let router = Router::new().with_typed("/echo", |v: Point| Ok(v));
    let wire = request("/echo", &Point { x: 1 });

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

/// A struct-backed read: request with no body, so nothing on the inbound side
/// is decoded and the whole budget is the response encode.
fn request_empty(path: &str) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

/// A device status block: the access pattern the struct read path is sized for,
/// where a client polls the whole object (or one wide field) at high frequency.
#[derive(Default, repe::RepeStruct)]
struct Status {
    id: String,
    temperature: f64,
    state: String,
    #[repe(typed)]
    samples: [u32; 8],
}
structio::object!(Status {
    id,
    temperature,
    state,
    samples
});

fn status() -> Status {
    Status {
        id: "sensor-42".into(),
        temperature: 21.5,
        state: "online".into(),
        samples: [1, 2, 3, 4, 5, 6, 7, 8],
    }
}

/// Read → dispatch → write for one struct path, with the writer buffer warmed.
fn struct_read_allocs(router: &Router, path: &str) -> usize {
    let wire = request_empty(path);
    let mut out = Vec::new();
    dispatch_cycle(router, &wire, path, &mut out);
    let (_, allocs) = count_allocs(|| dispatch_cycle(router, &wire, path, &mut out));
    allocs
}

#[test]
fn struct_read_allocation_budget() {
    // Every struct read costs the same two allocations:
    //
    //   1  read_message: the query Vec (a read request has no body)
    //   1  the response body buffer, pre-sized for the wire prefix plus a small
    //      body allowance, so a leaf encode neither regrows nor re-reserves
    //   0  the encode itself — serialized straight into that buffer
    //   0  query echo (moved from the request)
    //   0  write_message (reuses the warmed `out` buffer)
    //   ----
    //   2
    //
    // The whole-object case exceeds the body allowance and regrows once, but
    // `Vec` doubling absorbs it within the initial allocation's own growth, so
    // the count holds. The `serde_json::Map` node, its per-key `String`s and the
    // `Value` per field are all gone; `encoding_in_place_beats_the_value_path`
    // pins that difference directly.
    const EXPECTED: usize = 2;
    let (router, _handle) = Router::new().with_struct("/status", status());

    for path in [
        "/status",         // the whole object
        "/status/state",   // a string leaf
        "/status/samples", // a `#[repe(typed)]` leaf, bulk-copied as BEVE
    ] {
        let allocs = struct_read_allocs(&router, path);
        assert_eq!(
            allocs, EXPECTED,
            "allocation budget for `{path}` changed (was {EXPECTED}, now {allocs}); \
             a reduction is good news — update EXPECTED; an increase is a regression"
        );
    }
}

#[test]
fn a_whole_object_read_writes_straight_into_the_response_buffer() {
    // The property the encode-in-place path exists for, stated as a budget
    // rather than as a comparison. There is no longer a `Value` form to
    // measure against: `repe_handle_into` is the only shape the trait has, and
    // a response is written into the caller's buffer as it is produced.
    //
    // One allocation, and it is the caller's own `Vec` growing past the
    // capacity it was given. Nothing between the struct's fields and the bytes
    // allocates: no map node, no `String` per key, no boxed value per field.
    use repe::structs::{RepeStruct, ResponseBody};

    let mut status = status();

    // Warm up: the first call settles any lazily-initialized state so the
    // measured one charges only the encode.
    {
        let mut buf = Vec::with_capacity(256);
        let mut out = ResponseBody::new(&mut buf);
        status.repe_handle_into(&[], None, &mut out).unwrap();
    }

    let (bytes, allocs) = count_allocs(|| {
        // Sized for the response, so the buffer is the one allocation and it is
        // not grown afterwards.
        let mut buf = Vec::with_capacity(256);
        let mut out = ResponseBody::new(&mut buf);
        status.repe_handle_into(&[], None, &mut out).unwrap();
        buf
    });

    assert_eq!(
        allocs, 1,
        "a whole-object read should allocate only the response buffer, got {allocs}"
    );
    // And the bytes are the listing, not an empty buffer that happened not to
    // allocate.
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.starts_with(r#"{"id":"sensor-42""#), "{text}");
    assert!(text.contains(r#""samples":[1,2,3,4,5,6,7,8]"#), "{text}");
}

/// A write through an `RwLock`-registered struct: the shared attempt declines
/// (a field write needs `&mut self`) and the exclusive retry serves it. Both
/// attempts run against one request, so anything the pair does twice shows up
/// here doubled.
///
/// The escaped path is the one that makes it visible. `with_segments` has a
/// fast path for pointers without `~`, but an escaped pointer falls back to
/// `json_pointer::parse`, which allocates a `Vec<String>` and runs
/// `str::replace` twice per token. Splitting once for both attempts rather
/// than once per attempt is the difference between paying that and paying it
/// twice, and it is invisible on the unescaped fast path.
#[test]
fn struct_write_under_a_shared_lock_splits_the_pointer_once() {
    use std::sync::{Arc, RwLock};

    #[derive(Default, repe::RepeStruct)]
    struct Escaped {
        #[repe(rename = "a/b")]
        a_b: u64,
        plain: u64,
    }
    structio::object!(Escaped {
        "a/b" => a_b,
        plain
    });

    let router = Router::new()
        .with_struct_shared::<Escaped, _>("/e", Arc::new(RwLock::new(Escaped::default())));

    for (path, expected) in [
        // Unescaped: the fast path splits into a stack buffer, so neither
        // attempt allocates for segments and only the request/response pair
        // shows up.
        ("/e/plain", 3usize),
        // Escaped: one `json_pointer::parse`, not two. Splitting per attempt
        // instead of once for both costs four more (10 rather than 6): the
        // `Vec<Cow<str>>`, the owned segment, its unescape, and the
        // `Vec<&str>`, all paid again to reach the identical segments.
        ("/e/a~1b", 6usize),
    ] {
        let wire = request(path, &7u64);
        let mut out = Vec::new();
        dispatch_cycle(&router, &wire, path, &mut out);
        let (_, allocs) = count_allocs(|| dispatch_cycle(&router, &wire, path, &mut out));
        assert_eq!(
            allocs, expected,
            "allocation budget for a write to `{path}` changed (was {expected}, now {allocs}); \
             an increase here means the declining shared attempt and the exclusive retry are \
             each doing work the other already did"
        );
    }
}
