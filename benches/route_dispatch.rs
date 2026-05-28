//! Router dispatch throughput.
//!
//! Measures the per-request work of resolving a path to a handler and
//! optionally invoking it, across the three handler shapes the router
//! supports (plain HashMap route, [`Registry`]-backed route, [`RepeStruct`]
//! struct route), each with and without an attached middleware.
//!
//! Before this refactor, registry and struct lookups paid a per-request
//! `String` + `Arc::new(...RequestHandler { ... })`, and any router with at
//! least one middleware paid an extra `Arc::new(MiddlewarePipeline)` per
//! lookup even on routes that did not need it. Post-refactor, each lookup is
//! a prefix check (or HashMap probe) plus a single `Arc::clone` of a handler
//! `Arc` built at registration time. The `middleware` variants of these
//! benches surface that hoisting directly.

use criterion::{Criterion, criterion_group, criterion_main};
use repe::registry::{Registry, RegistryCallable};
use repe::server::{Middleware, Next, Router};
use repe::{BodyFormat, Message, QueryFormat};
use serde_json::{Value, json};
use std::hint::black_box;
use std::sync::{Arc, Mutex};

#[derive(Default, serde::Serialize, serde::Deserialize, repe::RepeStruct)]
#[repe(methods(get_number(&self) -> i32))]
struct BenchStruct {
    counter: i32,
}

impl BenchStruct {
    fn get_number(&self) -> i32 {
        self.counter
    }
}

/// Trivial pass-through middleware: just forwards to the next layer.
/// Realistic instrumented middleware would do more work, but this is enough
/// to populate `Router::middlewares` and exercise the wrap-on-registration
/// path versus the prior wrap-on-lookup path.
struct PassThrough;

impl Middleware for PassThrough {
    fn handle(&self, req: &Message, next: Next<'_>) -> Result<Message, repe::error::RepeError> {
        next.run(req)
    }
}

struct BenchCallable;

impl RegistryCallable for BenchCallable {
    fn call(
        &self,
        _ctx: &repe::peer::CallContext,
        _body: Option<Value>,
    ) -> Result<Value, (repe::ErrorCode, String)> {
        Ok(json!({"ok": true}))
    }
}

fn build_plain_router(with_middleware: bool) -> Router {
    let router = Router::new().with_json("/sum", |_v: Value| Ok(json!({"sum": 0})));
    if with_middleware {
        router.with_middleware(PassThrough)
    } else {
        router
    }
}

fn build_registry_router(with_middleware: bool) -> Router {
    let registry = Arc::new(Registry::new());
    registry.register_function("/echo", BenchCallable).unwrap();
    let router = Router::new().with_registry("/api", registry);
    if with_middleware {
        router.with_middleware(PassThrough)
    } else {
        router
    }
}

fn build_struct_router(with_middleware: bool) -> Router {
    let shared = Arc::new(Mutex::new(BenchStruct { counter: 7 }));
    let router = Router::new().with_struct_shared::<BenchStruct, _>("/svc", shared);
    if with_middleware {
        router.with_middleware(PassThrough)
    } else {
        router
    }
}

fn build_request(path: &str) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(&json!({}))
        .unwrap()
        .body_format(BodyFormat::Json)
        .build()
}

fn bench_router_get(c: &mut Criterion) {
    let plain = build_plain_router(false);
    let plain_mw = build_plain_router(true);
    let registry = build_registry_router(false);
    let registry_mw = build_registry_router(true);
    let struct_router = build_struct_router(false);
    let struct_router_mw = build_struct_router(true);

    let mut group = c.benchmark_group("router_get");
    group.bench_function("plain", |b| {
        b.iter(|| black_box(plain.get(black_box("/sum"))))
    });
    group.bench_function("plain_with_middleware", |b| {
        b.iter(|| black_box(plain_mw.get(black_box("/sum"))))
    });
    group.bench_function("registry", |b| {
        b.iter(|| black_box(registry.get(black_box("/api/echo"))))
    });
    group.bench_function("registry_with_middleware", |b| {
        b.iter(|| black_box(registry_mw.get(black_box("/api/echo"))))
    });
    group.bench_function("struct", |b| {
        b.iter(|| black_box(struct_router.get(black_box("/svc/get_number"))))
    });
    group.bench_function("struct_with_middleware", |b| {
        b.iter(|| black_box(struct_router_mw.get(black_box("/svc/get_number"))))
    });
    group.finish();
}

fn bench_full_dispatch(c: &mut Criterion) {
    let plain = build_plain_router(false);
    let plain_mw = build_plain_router(true);
    let registry = build_registry_router(false);
    let registry_mw = build_registry_router(true);
    let struct_router = build_struct_router(false);
    let struct_router_mw = build_struct_router(true);

    let plain_req = build_request("/sum");
    let registry_req = build_request("/api/echo");
    let struct_req = build_request("/svc/get_number");

    let mut group = c.benchmark_group("router_dispatch");
    group.bench_function("plain", |b| {
        b.iter(|| {
            let h = plain.get(black_box("/sum")).unwrap();
            black_box(h.handle(&plain_req).unwrap());
        })
    });
    group.bench_function("plain_with_middleware", |b| {
        b.iter(|| {
            let h = plain_mw.get(black_box("/sum")).unwrap();
            black_box(h.handle(&plain_req).unwrap());
        })
    });
    group.bench_function("registry", |b| {
        b.iter(|| {
            let h = registry.get(black_box("/api/echo")).unwrap();
            black_box(h.handle(&registry_req).unwrap());
        })
    });
    group.bench_function("registry_with_middleware", |b| {
        b.iter(|| {
            let h = registry_mw.get(black_box("/api/echo")).unwrap();
            black_box(h.handle(&registry_req).unwrap());
        })
    });
    group.bench_function("struct", |b| {
        b.iter(|| {
            let h = struct_router.get(black_box("/svc/get_number")).unwrap();
            black_box(h.handle(&struct_req).unwrap());
        })
    });
    group.bench_function("struct_with_middleware", |b| {
        b.iter(|| {
            let h = struct_router_mw.get(black_box("/svc/get_number")).unwrap();
            black_box(h.handle(&struct_req).unwrap());
        })
    });
    group.finish();
}

criterion_group!(benches, bench_router_get, bench_full_dispatch);
criterion_main!(benches);
