# Streaming with Backpressure (`repe::stream`)

When you need to push multi-GB blobs (capture files, log streams, paginated query results) from a server to a peer over notify messages, the bare notify primitive does not give you flow control or reconnect-resume. The `repe::stream` module adds three pieces:

- **ACK-driven window credit.** The producer waits before sending if the receiver is more than `window_bytes` behind on ACKs; an ACK releases the window and unparks the producer. Defaults to 64 MiB.
- **Idle watchdog.** A transfer that has gone silent for longer than `idle_timeout` (default 60 s) is force-cancelled.
- **Replay ring + reconnect.** Each emitted body lands in a byte-bounded ring (default 64 MiB). On a brief disconnect the producer parks; an inbound resume handler validates a `last_received_offset` against the ring and swaps in the new peer, after which the producer replays the ring tail and continues.

The wire shape (`transfer_begin`, chunk body fields, ACK / cancel / resume bodies) is up to the embedder. This module deals only in offsets, ACKs, and opaque body bytes. See `TransferControl`, `TransferRegistry<K>`, and `spawn_watchdog` for the full surface.

## Sketch

```rust
use std::sync::Arc;
use std::time::{Duration, Instant};
use repe::{NotifyBody, PeerSendError, ReconnectOutcome,
    TransferControl, TransferRegistry, spawn_watchdog};

#[derive(Hash, Eq, PartialEq, Copy, Clone)]
struct TransferId(u64);

let registry: Arc<TransferRegistry<TransferId>> = Arc::new(TransferRegistry::new());
spawn_watchdog(Arc::clone(&registry), Duration::from_secs(60));

let peer = make_peer();           // your embedder-side PeerHandle factory
let control = TransferControl::new(64 * 1024 * 1024);
control.set_peer(peer);
registry.register(TransferId(1), Arc::clone(&control));

let chunk_offset: u64 = 0;
let chunk_len: u64 = 4 * 1024 * 1024;
let body: Vec<u8> = body_for_chunk(chunk_offset);

control.wait_for_credit(chunk_len, Instant::now() + Duration::from_secs(30))?;
control.push_replay(chunk_offset, false, body.clone());
let p = control.peer().expect("peer installed");
match p.send_notify("/file_chunk", NotifyBody::Beve(body)) {
    Ok(()) => control.record_sent(chunk_offset + chunk_len),
    Err(PeerSendError::Disconnected) => match control.wait_for_reconnect(Duration::from_secs(30)) {
        ReconnectOutcome::ResumeReady => {
            let resume = control.take_pending_resume().unwrap();
            // request_resume installed the new peer into the slot;
            // read it back via control.peer() and replay every
            // ring chunk at offset >= resume.resume_at_offset.
        }
        ReconnectOutcome::Cancelled(_) | ReconnectOutcome::Timeout => { /* abort */ }
    },
    Err(_) => { /* application policy */ }
}

// Inbound handlers (run in your existing dispatch path):
//   ack:    registry.get(id).map(|c| c.record_ack(file_index, offset));
//   cancel: registry.get(id).map(|c| c.cancel("client cancelled"));
//   resume: registry.get(id).and_then(|c|
//             c.request_resume(new_peer, file_index, offset).ok());
```

## Defaults

`DEFAULT_WINDOW_BYTES`, `DEFAULT_BACKPRESSURE_TIMEOUT`, `DEFAULT_IDLE_TIMEOUT`, `DEFAULT_REPLAY_RING_BYTES`, and `DEFAULT_RECONNECT_TIMEOUT` are tuned for LAN-class links pushing multi-GB files. Lower the window on slow links; raise the reconnect timeout for clients with long roaming windows.

## Watchdog Lifecycle

`spawn_watchdog` holds a `Weak<TransferRegistry<K>>`, not an `Arc`. Dropping the embedder's last strong reference terminates the watchdog thread on its next tick (clamped to `[1 s, 5 s]`). For a process-wide singleton this means the thread lives for the process; embedders that build short-lived registries (per-test, per-tenant) get clean teardown for free.
