//! End-to-end round-trip latency: REPE+BEVE vs gRPC+protobuf over real sockets.
//!
//! Both servers do the same trivial work (echo the body back) so the handler
//! cost is identical and the measurement is dominated by encode + framing +
//! transport + decode. Requests are issued sequentially on a single warm
//! connection -- this measures *latency* (and single-stream throughput), not
//! peak concurrent throughput. For the concurrency story (where gRPC's HTTP/2
//! multiplexing matters) drive each client from N tasks and sum the rates; the
//! single-stream number below is the honest latency floor.
//!
//! The REPE path here uses the ergonomic `call_typed_beve` API (serde-encoded
//! BEVE body). The bulk typed-slice fast path measured in `benches/codec.rs` is
//! faster still on the encode/decode side; this harness reports the
//! out-of-the-box high-level API, which is what most callers actually use.
//!
//!   cargo run --release --bin e2e

use grpc_shootout::{ELEM_COUNTS, Small, pb, sample_f64, sample_i64};
use std::time::{Duration, Instant};

use pb::shootout_client::ShootoutClient;
use pb::shootout_server::{Shootout, ShootoutServer};
use tonic::{Request, Response, Status, transport::Server};

const REPE_ADDR: &str = "127.0.0.1:8090";
const GRPC_ADDR: &str = "127.0.0.1:8091";
const WARMUP: usize = 200;
const ITERS: usize = 2_000;

// ---- gRPC server ----------------------------------------------------------

#[derive(Default)]
struct EchoService;

#[tonic::async_trait]
impl Shootout for EchoService {
    async fn echo_f64(&self, r: Request<pb::NumericF64>) -> Result<Response<pb::NumericF64>, Status> {
        Ok(Response::new(r.into_inner()))
    }
    async fn echo_i64(&self, r: Request<pb::NumericI64>) -> Result<Response<pb::NumericI64>, Status> {
        Ok(Response::new(r.into_inner()))
    }
    async fn echo_small(&self, r: Request<pb::Small>) -> Result<Response<pb::Small>, Status> {
        Ok(Response::new(r.into_inner()))
    }
}

// ---- REPE server ----------------------------------------------------------

async fn spawn_repe_server() -> std::io::Result<()> {
    use repe::{AsyncServer, Router};
    let router = Router::new()
        .with_typed("/echo_f64", |v: Vec<f64>| Ok(v))
        .with_typed("/echo_i64", |v: Vec<i64>| Ok(v))
        .with_typed("/echo_small", |v: Small| Ok(v))
        // Bulk numeric fast path (the optimization under test): decode + frame
        // the whole array in one copy each way, no per-element serde walk.
        .with_typed_slice::<f64, f64, _>("/slice_f64", |v| Ok(v))
        .with_typed_slice::<i64, i64, _>("/slice_i64", |v| Ok(v));
    let listener = AsyncServer::listen(REPE_ADDR).await?;
    tokio::spawn(async move { AsyncServer::new(router).serve(listener).await });
    Ok(())
}

// ---- timing helper --------------------------------------------------------

/// Run an `.await` expression WARMUP+ITERS times; yield the mean latency over
/// the timed window. A macro (rather than a closure taking a future) so the
/// awaited expression can borrow `&mut grpc` each iteration without the borrow
/// escaping a `FnMut` body.
macro_rules! measure {
    ($body:block) => {{
        for _ in 0..WARMUP {
            $body
        }
        let start = Instant::now();
        for _ in 0..ITERS {
            $body
        }
        start.elapsed() / ITERS as u32
    }};
}

fn report(label: &str, n: usize, repe: Duration, grpc: Duration) {
    let ratio = grpc.as_secs_f64() / repe.as_secs_f64();
    println!(
        "{label:<14} n={n:<9} repe={:>9.2?}  grpc={:>9.2?}  grpc/repe={ratio:.2}x",
        repe, grpc
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bring up both servers.
    spawn_repe_server().await?;
    let grpc_addr = GRPC_ADDR.parse()?;
    tokio::spawn(async move {
        // Raise gRPC's default 4 MiB message cap so the 1M-element (8 MiB)
        // payload is admitted -- REPE has no such default limit, so leaving it
        // would compare unequal work.
        let svc = ShootoutServer::new(EchoService)
            .max_decoding_message_size(64 << 20)
            .max_encoding_message_size(64 << 20);
        Server::builder().add_service(svc).serve(grpc_addr).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await; // let listeners bind

    let repe = repe::AsyncClient::connect(REPE_ADDR).await?;
    let mut grpc = ShootoutClient::connect(format!("http://{GRPC_ADDR}"))
        .await?
        .max_decoding_message_size(64 << 20)
        .max_encoding_message_size(64 << 20);

    println!("== f64 echo: repe serde (call_typed_beve) vs gRPC fixed64 ==");
    for &n in ELEM_COUNTS {
        let data = sample_f64(n);
        let r = measure!({
            let _: Vec<f64> = repe.call_typed_beve("/echo_f64", &data).await.unwrap();
        });
        let g = measure!({
            let req = pb::NumericF64 { values: data.clone() };
            let _ = grpc.echo_f64(Request::new(req)).await.unwrap();
        });
        report("f64-serde", n, r, g);
    }

    println!("== f64 echo: repe BULK (call_typed_slice) vs gRPC fixed64 ==");
    for &n in ELEM_COUNTS {
        let data = sample_f64(n);
        let r = measure!({
            let _: Vec<f64> = repe.call_typed_slice("/slice_f64", &data).await.unwrap();
        });
        let g = measure!({
            let req = pb::NumericF64 { values: data.clone() };
            let _ = grpc.echo_f64(Request::new(req)).await.unwrap();
        });
        report("f64-bulk", n, r, g);
    }

    println!("== i64 echo: repe serde vs gRPC zig-zag varint ==");
    for &n in ELEM_COUNTS {
        let data = sample_i64(n);
        let r = measure!({
            let _: Vec<i64> = repe.call_typed_beve("/echo_i64", &data).await.unwrap();
        });
        let g = measure!({
            let req = pb::NumericI64 { values: data.clone() };
            let _ = grpc.echo_i64(Request::new(req)).await.unwrap();
        });
        report("i64-serde", n, r, g);
    }

    println!("== i64 echo: repe BULK (call_typed_slice) vs gRPC zig-zag varint ==");
    for &n in ELEM_COUNTS {
        let data = sample_i64(n);
        let r = measure!({
            let _: Vec<i64> = repe.call_typed_slice("/slice_i64", &data).await.unwrap();
        });
        let g = measure!({
            let req = pb::NumericI64 { values: data.clone() };
            let _ = grpc.echo_i64(Request::new(req)).await.unwrap();
        });
        report("i64-bulk", n, r, g);
    }

    println!("== small struct echo (narrow-gap case) ==");
    {
        let small = Small::sample();
        let r = measure!({
            let _: Small = repe.call_typed_beve("/echo_small", &small).await.unwrap();
        });
        let g = measure!({
            let req = pb::Small {
                id: small.id,
                name: small.name.clone(),
                ok: small.ok,
                score: small.score,
            };
            let _ = grpc.echo_small(Request::new(req)).await.unwrap();
        });
        report("small", 1, r, g);
    }

    Ok(())
}
