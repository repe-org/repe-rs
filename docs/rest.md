# REST Gateway

Enable the `rest` feature for `RestGateway`, an HTTP facade in front of a `Registry`.

This adds a front door; it does not replace one. Public clients get curl, OpenAPI, and edge caching. Clients that need the [aligned numeric fast path](numeric-bodies.md), notify, or sub-millisecond dispatch keep talking REPE to the same registry, and both legs see the same state.

```rust
use repe::rest::RestGateway;
use repe::{Registry, Router};
use std::sync::Arc;

let registry = Arc::new(Registry::new());
registry.register_value("/counter", serde_json::json!(0))?;

// One registry, two carriers.
let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
let gateway = RestGateway::new("/api/v1", registry)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

```rust
# async fn run(gateway: repe::rest::RestGateway) -> std::io::Result<()> {
let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
gateway.serve(listener).await
# }
```

`serve` handles HTTP/1.1 and HTTP/2 on the one listener, detecting the protocol per connection. TLS is out of scope: put this behind a terminator that already does certificate rotation and ALPN.

## Why the translation is mechanical

A REPE query is an RFC 6901 JSON Pointer into a value tree. A REST resource is a path into a value tree. They are the same addressing scheme, so the gateway is a mapping rather than a redesign.

The verbs fall out the same way. `Registry::dispatch` already picks between three operations, from the body alone: an empty body READs, a non-empty body against a function CALLs, a non-empty body against anything else WRITEs. Those are exactly the three HTTP verbs worth having, with matching safety and idempotency:

| HTTP | Registry operation | Safe | Idempotent | Cacheable |
| --- | --- | --- | --- | --- |
| `GET` / `HEAD` | READ the value at the path | yes | yes | yes |
| `PUT` | WRITE the value at the path | no | yes | no |
| `POST` | CALL the function at the path | no | no | no |

```
$ curl -s localhost:8080/api/v1/counter
0
$ curl -s -X PUT -d 42 localhost:8080/api/v1/counter
{"path":"/counter","status":"ok"}
$ curl -s -X POST -d '{"a":2,"b":3}' localhost:8080/api/v1/add
{"result":5}
```

## The verb mismatch is an error, not a coercion

A `PUT` at a function answers `405`, not a silent call. A `POST` at a value answers `405`, not a silent write. `Registry::is_function` is what makes the distinction available before a body exists.

This is the part that keeps the facade from becoming RPC in a REST costume. An API where every route is `POST` pays all of REST's costs and collects none of its guarantees: nothing is cacheable, nothing is safely retryable, and no intermediary can act on the method. Refusing the mismatched verb is what preserves the promise a caller relies on when it retries a `PUT`.

`OPTIONS` answers `204` with the `Allow` set the target actually supports, so the value/function distinction is discoverable:

```
$ curl -si -X OPTIONS localhost:8080/api/v1/add | grep -i allow
allow: GET, HEAD, POST, OPTIONS
```

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

Mutations carry neither a validator nor a freshness directive.

## Content negotiation

JSON by default, BEVE on request, independently on each leg:

```
$ curl -s -H 'Accept: application/x-beve' localhost:8080/api/v1/config | xxd | head -1
00000000: 0308 106e 616d 6502 1064 656d 6f1c 7665  ...name..demo.ve
```

`Accept` handling covers media types and `q` values; anything unrecognized falls back to JSON rather than answering `406`. A missing `Content-Type` on a request body is treated as JSON, which is what `curl -d` sends. An unsupported one is `415`.

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

`RestConfig` carries the policy: `max_body_bytes` (default 1 MiB, answering `413`), `cache_control`, `etag`, and `application_error_status`. The body limit bounds the facade, not the protocol: a REST body is buffered whole before it can be decoded, so an unbounded limit is an unbounded allocation driven by an anonymous caller. Bulk payloads belong on the REPE leg, which streams.

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
