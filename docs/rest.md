# REST Gateway

Enable the `rest` feature for `RestGateway`, an HTTP facade in front of a `Registry`.

This adds a front door; it does not replace one. Public clients get curl, OpenAPI, and edge caching. Clients that need the [aligned numeric fast path](numeric-bodies.md), notify, or sub-millisecond dispatch keep talking REPE to the same registry, and both legs see the same state.

```rust
use repe::rest::RestGateway;
use repe::{Registry, Router};
use std::sync::Arc;

let registry = Arc::new(Registry::new());
registry.register_function("/counter", |_: Option<repe::structs::RequestBody<'_>>| Ok(0i64))?;

// One registry, two carriers.
let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
let gateway = RestGateway::new("/api/v1", registry);
# Ok::<(), Box<dyn std::error::Error>>(())
```

```rust
# async fn run(gateway: repe::rest::RestGateway) -> std::io::Result<()> {
let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
gateway.serve(listener).await
# }
```

`serve` handles HTTP/1.1 and HTTP/2 on the one listener, detecting the protocol per connection.

## This gateway has no authentication

TLS and identity are both out of scope. Put this behind a terminator that already handles certificate rotation, ALPN, and authentication — an ingress, an API gateway, a sidecar. `RestConfig` reduces blast radius; it is not an access-control system, and the defaults assume something in front is doing that job.

One default exists specifically to keep an unauthenticated deployment from being worse than it looks: **`read_only`** (default `false`) answers every mutation with `405`. Reads are what this facade is for — the safe, cacheable, CDN-frontable half. A gateway that only publishes state should say so here rather than trust an upstream to filter methods.

A second, **`accept_beve_bodies`**, defaults to `true` but is worth knowing about: see below.

### Why `accept_beve_bodies` exists

It was off by default while the previous BEVE decoder had no recursion limit. Nesting is declared by the input, so a few kilobytes of nested array tags overflowed the thread stack, and a Rust stack overflow **aborts the process** rather than unwinding into the per-connection catch — one unauthenticated request took down the gateway and anything co-hosted with it. `max_body_bytes` was three orders of magnitude too loose to help.

structio bounds nesting depth (256 levels) and reports the refusal as an ordinary error, which the gateway answers with `400` like any other malformed body, so the default is now `true`. It remains a knob because content negotiation is policy: a gateway that publishes a JSON-only contract can turn it off and refuse `application/x-beve` with `415` rather than accept a representation it does not document.

BEVE *responses* are unaffected either way and always available: encoding is driven by the server's own data, not by an anonymous caller.

## Why the translation is mechanical

A REPE query is an RFC 6901 JSON Pointer. A REST resource is a path. They are the same addressing scheme, so the gateway is a mapping rather than a redesign.

The verbs fall out the same way. Every registered path is a function, and `Registry::call` has two ways of calling one: with a body, or without. Those are the verbs:

| HTTP | Registry operation | Safe | Idempotent | Cacheable |
| --- | --- | --- | --- | --- |
| `GET` / `HEAD` | call the function with no body | yes | yes | yes |
| `PUT` / `POST` | call the function with the body as arguments | no | no | no |

```
$ curl -s localhost:8080/api/v1/counter
0
$ curl -s -X PUT -d 42 localhost:8080/api/v1/counter
42
$ curl -s -X POST -d '{"a":2,"b":3}' localhost:8080/api/v1/add
{"result":5}
```

## `PUT` and `POST` are aliases

The registry stores no values, so there is no assignment for `PUT` to mean instead of a call. Both verbs call the function at the pointer, `Allow` reports `GET, HEAD, PUT, POST, OPTIONS`, and neither is idempotent, because a call is not. A caller that wants RFC 9110 §9.2.2 idempotence out of `PUT` is relying on a promise the registry never made; the honest reading is that this gateway exposes calls and nothing else.

`GET` is safe on the understanding that a bodiless call is a read. A handler that mutates when called with no body breaks that contract on its own, and a cache in front of the gateway will then serve its stale output — which is the reason to keep reads and mutations on separate paths.

`OPTIONS` answers `204` with the `Allow` set the target actually supports, so the value/function distinction is discoverable:

```
$ curl -si -X OPTIONS localhost:8080/api/v1/add | grep -i allow
allow: GET, HEAD, POST, OPTIONS
```

`OPTIONS *` (RFC 9110 §9.3.7) reports the server-wide method set rather than any one resource's.

A `PUT` or `POST` with an empty body is `400`, not a read. An empty body is how the registry spells READ, so passing one through would answer a write with `200` and the *old* value, having written nothing. A call that takes no arguments sends `null`.

## Caching

Caching is the one REST property REPE has no answer for, and it is why this facade can beat the binary protocol on read-heavy traffic: a validated cache hit at the edge is zero origin work, which no wire format competes with.

Successful reads carry a strong `ETag` over the exact bytes sent, so a conditional `GET` costs a `304` with no body:

```
$ curl -si localhost:8080/api/v1/counter | grep -i etag
etag: "af63ad4c86019caf"
$ curl -s -o /dev/null -w '%{http_code}\n' -H 'If-None-Match: "af63ad4c86019caf"' localhost:8080/api/v1/counter
304
```

The tag is FNV-1a/64 over the response body rather than a `DefaultHasher`, because two instances behind one load balancer must hash identically or a shared cache thrashes, and `DefaultHasher`'s output is explicitly not stable across Rust releases.

Reads also carry `Vary: Accept`, because the gateway content-negotiates JSON against BEVE and the two representations of one resource hash differently. Without it, a shared cache would serve BEVE bytes to a JSON client.

`RestConfig::cache_control` sets the freshness directive, defaulting to `no-cache`: revalidate every time, which keeps `ETag` and `304` working while never serving stale state from a registry that has no way to announce a mutation. Raise it to a `max-age` per deployment where the data allows, which is where the real edge-caching win comes from.

Mutations carry no validator and no freshness directive. Error responses carry `Cache-Control: no-store`, because RFC 9111 §4.2.2 makes 404 and 405 heuristically cacheable — a cached 405 would keep its stale `Allow` after the path became a function, leaving the resource unreachable through its only correct verb.

## No conditional writes

`If-Match` and `If-None-Match` are not evaluated on `PUT` or `POST`. They compared a tag against a stored value, and there is no stored value to compare against: a call's effect is the handler's business, and a gateway that evaluated a validator against the *previous call's output* would be answering `412` on a question nobody asked. The headers are ignored on a mutation, not refused.

A handler that needs compare-and-swap takes the expected state as an argument, where it is inside the handler's own lock rather than racing outside it. That is also the stronger guarantee: a validator compared outside the lock is check-then-act, and two clients holding the same tag would both write.

## Content negotiation

JSON by default, BEVE on request, independently on each leg:

```
$ curl -s -H 'Accept: application/x-beve' localhost:8080/api/v1/config | xxd | head -1
00000000: 0308 106e 616d 6502 1064 656d 6f1c 7665  ...name..demo.ve
```

`Accept` handling covers media types and `q` values; anything unrecognized falls back to JSON rather than answering `406`. A missing `Content-Type` on a request body is treated as JSON, which is what `curl -d` sends. An unsupported one is `415`.

## Mounting

The mount is normalized the same way `Router::with_registry` normalizes its prefix, through the same function: made absolute, stripped of a trailing separator, empty for the root. `"/api/v1"`, `"/api/v1/"`, and `"api/v1"` all name the same mount, so construction is infallible.

## Paths

The mount prefix is stripped, then each path segment is percent-decoded **and then** JSON-Pointer-escaped, per segment. The order matters: decoding the whole path first would let `%2F` decode into a `/` that reads as a segment separator, so `/items/a%2Fb` would address `b` inside `a` instead of the single key `a/b`.

The mount must match on a segment boundary, so `/api/v1x` does not match a `/api/v1` mount. A trailing slash names the same resource. The mount itself maps to the empty pointer, so `GET /api/v1` reads the whole tree.

## Errors

Failures answer with RFC 9457 problem details (`application/problem+json`), carrying the originating REPE `ErrorCode` as a `repe_code` member and in an `X-Repe-Error-Code` header, so the underlying code is not lost in translation:

```json
{"type":"about:blank","title":"Not Found","status":404,"detail":"path not found `/absent`","repe_code":6}
```

| `ErrorCode` | HTTP |
| --- | --- |
| `MethodNotFound` | 404 |
| `InvalidQuery`, `InvalidBody`, `ParseError`, `InvalidHeader`, `VersionMismatch` | 400 |
| `Timeout` | 504 |
| `ResourceExhausted` | 503 |
| `InternalError` | 500 |
| `ApplicationErrorBase` | `RestConfig::application_error_status`, default 500 |

An application error defaults to 500 because the gateway cannot know whether a given handler's failure means "you sent the wrong thing" or "something here is broken", and guessing 4xx would tell clients not to retry failures a retry would fix. A deployment that knows what its handlers mean should set the status.

## Configuration

`RestConfig` carries the policy: `max_body_bytes` (default 1 MiB, answering `413`), `cache_control`, `application_error_status`, the three safety defaults above, `max_connections` (default 1024), and `request_timeout` (default 30 s). Note that the first two multiply: at the defaults, 1024 connections each buffering up to 1 MiB is 1 GiB of request bodies in flight. The body limit bounds the facade, not the protocol: a REST body is buffered whole before it can be decoded, so an unbounded limit is an unbounded allocation driven by an anonymous caller. Bulk payloads belong on the REPE leg, which streams.

`max_connections` bounds concurrency in `serve`: past the cap it stops accepting, so a burst waits in the listen backlog rather than in the descriptor table. Two timeouts keep a slot from being held indefinitely, and both are needed. The 30-second header-read timeout covers a client that connects and sends nothing. `request_timeout` covers the other half: a client that sends a complete, well-formed head promising a `Content-Length` and then sends no body is past the header timeout and otherwise unbounded, so a few hundred sockets would hold every slot at almost no cost to the sender. Expiry answers `408` and closes the connection, since the promised body is still unread and the stream is no longer at a message boundary.

An `accept` failure that is not about a single connection — descriptor exhaustion is the one that matters, and Rust reports it as `Uncategorized` rather than a matchable kind — is retried with a backoff capped at one second. `serve` does not give up on the listener: staying down after the flood has drained would defeat the point of bounding it.

## Testing without a socket

`RestGateway::respond` takes a `RestRequest` and returns a `RestResponse` with no transport involved, the same shape `Router::call` gives the REPE side. `serve` is a hyper shim over it, and any other HTTP stack can be one too.

```rust
use repe::rest::{RestGateway, RestRequest};
# fn f(gateway: RestGateway) {
let response = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
assert_eq!(response.status, 200);
# }
```

## Example

`cargo run --features rest --example rest_gateway` starts both legs over one registry: REST on `:8080`, REPE on `:8081`.
