//! Stream a large in-memory value from a server to a client without either side
//! holding a full serialized or compressed copy — the SVS download path.
//!
//! Run with: `cargo run --example value_stream --features value-stream`

use repe::value_stream::{RouterValueStreamExt, StreamOpts};
use repe::{Client, Router, Server, pull_value};
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::thread;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Dataset {
    name: String,
    samples: Vec<f64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The value the server will stream. In a real service this might be loaded
    // lazily inside `resolve` keyed on the requested resource.
    let dataset = Dataset {
        name: "demo".to_string(),
        samples: (0..1_000_000).map(|i| i as f64).collect(),
    };

    // Register the SVS routes. `resolve` maps a resource key to the value to
    // stream; the value is BEVE-serialized and zstd-compressed as it is pulled.
    let server_copy = dataset.clone();
    let router = Router::new().with_value_stream(
        move |resource: &str| (resource == "demo").then(|| server_copy.clone()),
        StreamOpts::default(),
    );

    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        let _ = server.serve(listener);
    });

    // Pull the value straight into a `Dataset`, streaming-decoded — never holding
    // the full compressed or decompressed buffer in memory.
    let client = Client::connect(addr)?;
    let pulled: Dataset = pull_value(&client, "demo")?;

    println!(
        "pulled '{}' with {} samples; matches source: {}",
        pulled.name,
        pulled.samples.len(),
        pulled == dataset
    );
    Ok(())
}
