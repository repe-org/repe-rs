# Fleet API

`repe` includes three fleet APIs for multi-node control:

- `Fleet` (sync TCP request/response)
- `AsyncFleet` (tokio TCP request/response)
- `UniUdpFleet` (sync UDP fire-and-forget)

## TCP Fleet Types

- `NodeConfig` - per-node config (`name`, `host`, `port`, `tags`, `timeout`)
- `RetryPolicy` - retry behavior (`max_attempts`, `delay`)
- `FleetOptions` - fleet-level defaults (`default_timeout`, `retry_policy`)
- `RemoteResult<T>` - per-node call result (`value`, `error`, `elapsed`)
- `HealthStatus` - health-check status (`healthy`, `latency`, `error`)

## Sync Fleet (`Fleet`)

```rust
use repe::{Fleet, FleetOptions, NodeConfig, RetryPolicy};
use serde_json::json;
use std::time::Duration;

let fleet = Fleet::with_options(
    vec![
        NodeConfig::new("127.0.0.1", 8081)?.with_name("node-1")?.with_tags(["compute"]),
        NodeConfig::new("127.0.0.1", 8082)?.with_name("node-2")?.with_tags(["compute", "primary"]),
    ],
    FleetOptions {
        default_timeout: Duration::from_secs(2),
        retry_policy: RetryPolicy { max_attempts: 3, delay: Duration::from_millis(100) },
    },
)?;

let _ = fleet.connect_all();
let single = fleet.call_json("node-1", "/compute", Some(&json!({"value": 10})))?;
let many = fleet.broadcast_json("/status", None, &[] as &[&str]);
let primary = fleet.broadcast_json("/status", None, &["primary"]);
let health = fleet.health_check("/status");
let total = fleet.map_reduce_json("/compute", Some(&json!({"value": 10})), &["compute"], |results| {
    results
        .into_iter()
        .filter_map(|r| r.value.and_then(|v| v["result"].as_i64()))
        .sum::<i64>()
});
let _ = fleet.disconnect_all();
```

### Sync Fleet API

- Construction:
  - `Fleet::new(configs)`
  - `Fleet::with_options(configs, options)`
  - `Fleet::default()`
- Node access:
  - `len`, `is_empty`, `keys`, `node`, `nodes`, `connected_nodes`, `filter_nodes`
- Dynamic node management:
  - `add_node`, `remove_node`
- Connection management:
  - `connect_all`, `disconnect_all`, `reconnect_disconnected`
  - `is_connected_all`, `is_connected(name)`
- Invocation:
  - `call_json(name, method, params)`
  - `call_message(name, method)`
  - `broadcast_json(method, params, tags)`
  - `map_reduce_json(method, params, tags, reduce_fn)`
- Health:
  - `health_check(endpoint)`

## Async Fleet (`AsyncFleet`)

`AsyncFleet` mirrors `Fleet` but all operations are `async`.

```rust
use repe::{AsyncFleet, FleetOptions, NodeConfig, RetryPolicy};
use serde_json::json;
use std::time::Duration;

let fleet = AsyncFleet::with_options(
    vec![NodeConfig::new("127.0.0.1", 8081)?.with_name("node-1")?],
    FleetOptions {
        default_timeout: Duration::from_secs(2),
        retry_policy: RetryPolicy { max_attempts: 3, delay: Duration::from_millis(100) },
    },
)?;

let _ = fleet.connect_all().await;
let result = fleet.call_json("node-1", "/status", Some(&json!({}))).await?;
assert!(result.succeeded());
```

## UDP Fleet (`UniUdpFleet`)

`UniUdpFleet` is for fire-and-forget fanout. It reports send success/failure, not delivery confirmation.

### UDP Types

- `UniUdpNodeConfig` - endpoint config (`name`, `host`, `port`, `tags`, `redundancy`, `chunk_size`, `fec_group_size`, `parity_shards`)
- `SendResult` - per-node send result (`message_id`, `error`, `elapsed`)

### UDP API

- Construction:
  - `UniUdpFleet::new(configs)`
  - `UniUdpFleet::default()`
- Node access:
  - `len`, `is_empty`, `keys`, `node`, `nodes`, `filter_nodes`
- Node management:
  - `add_node`, `remove_node`, `close`
- Sends:
  - `send_notify(method, params, tags)`
  - `send_request(method, params, tags)`
  - `notify_all(method, params)`
  - `send_notify_to(node_name, method, params)`

```rust
use repe::{UniUdpFleet, UniUdpNodeConfig};
use serde_json::json;

let fleet = UniUdpFleet::new(vec![
    UniUdpNodeConfig::new("127.0.0.1", 5001)?.with_name("edge-a")?.with_tags(["edge"]),
    UniUdpNodeConfig::new("127.0.0.1", 5002)?.with_name("edge-b")?.with_tags(["edge"]),
])?;

let results = fleet.send_notify("/heartbeat", Some(&json!({"source": "controller"})), &["edge"]);
for (name, result) in results {
    println!("{name}: sent={} msg_id={}", result.succeeded(), result.message_id);
}
```

## Notes

- Tag filters match nodes that contain all specified tags.
- Fleet methods snapshot target nodes before fanout, so in-flight broadcasts are isolated from concurrent add/remove operations.
- Retry behavior is applied to TCP fleet calls (`call_json`, `call_message`, `broadcast_json`) for transport/I/O failures only (not well-formed server error responses).
- `UniUdpClient` is backed by the `uniudp` crate. UDP fanout now uses UniUDP framing/reliability features (chunking, redundancy, and optional RS FEC).
- Default RS profile is `data_shards=4`, `parity_shards=2` (with `redundancy=1`).
