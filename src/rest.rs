//! REST facade over a [`Registry`]: an HTTP gateway in front of a REPE core.
//!
//! REPE and REST disagree about almost everything except the one thing that
//! matters here: both address state by path. A REPE query is an RFC 6901 JSON
//! Pointer into a value tree, and a REST resource is a path into a value tree,
//! so the translation is mechanical rather than a redesign. This module is that
//! translation, and nothing else — it adds a facade, it does not replace a
//! protocol. Public clients get curl, OpenAPI, and edge caching; clients that
//! need the aligned numeric fast path, notify, or sub-millisecond dispatch keep
//! talking REPE to the same [`Registry`].
//!
//! # The verb mapping
//!
//! [`Registry::dispatch`] already decides between three operations, and it does
//! so from the body alone: an empty body READs, a non-empty body against a
//! function CALLs, a non-empty body against anything else WRITEs. Those three
//! are exactly the three HTTP verbs worth having, with the same safety and
//! idempotency properties:
//!
//! | HTTP | Registry operation | Safe | Idempotent | Cacheable |
//! | --- | --- | --- | --- | --- |
//! | `GET` / `HEAD` | READ the value at the path | yes | yes | yes |
//! | `PUT` | WRITE the value at the path | no | yes | no |
//! | `POST` | CALL the function at the path | no | no | no |
//!
//! The mapping is enforced in both directions, which is the whole point of
//! putting REST in front of this rather than tunnelling RPC through it. A `PUT`
//! at a function is `405`, not a silent call, because a caller is entitled to
//! assume its `PUT` was idempotent. A `POST` at a value is `405`, not a silent
//! write, because a value *is* idempotently writable and saying otherwise pushes
//! callers away from the retry-safe verb. An API where every route is `POST`
//! pays all of REST's costs and collects none of its guarantees; refusing the
//! mismatched verb is what keeps this from becoming that.
//!
//! `OPTIONS` answers `204` with the `Allow` set the target actually supports,
//! so the value/function distinction is discoverable rather than folklore.
//!
//! # Caching
//!
//! Caching is the one REST property REPE has no answer for, and it is the reason
//! this facade can be *faster* than the binary protocol for read-heavy traffic:
//! a validated cache hit at the edge is zero origin work, which no wire format
//! competes with.
//!
//! Successful reads carry a strong `ETag` over the exact bytes sent, so a
//! conditional `GET` costs a `304` with no body. They also carry `Vary: Accept`,
//! because this gateway content-negotiates JSON against BEVE and the two
//! representations of one resource hash differently — a shared cache that missed
//! that would serve BEVE to a JSON client. [`RestConfig::cache_control`] sets the
//! freshness directive, defaulting to `no-cache`: revalidate every time, which
//! keeps `ETag` working while never serving stale state from a registry that has
//! no way to announce a mutation. Raise it per deployment where the data allows.
//!
//! # Content negotiation
//!
//! Requests and responses are JSON by default and BEVE on request
//! (`application/x-beve`), on both legs independently. That keeps one URL space
//! for a human with curl and for a program that would rather not pay for JSON,
//! without either of them needing a second endpoint.
//!
//! # Errors
//!
//! Failures answer with RFC 9457 problem details
//! (`application/problem+json`), carrying the originating REPE
//! [`ErrorCode`] as a `repe_code` member and in an `X-Repe-Error-Code` header,
//! so a REST client sees a status it understands without the underlying code
//! being lost in translation.
//!
//! # Example
//!
//! ```no_run
//! use repe::{Registry, rest::RestGateway};
//! use std::sync::Arc;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let registry = Arc::new(Registry::new());
//! registry.register_value("/counter", serde_json::json!(0))?;
//!
//! let gateway = RestGateway::new("/api/v1", Arc::clone(&registry))?;
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//! gateway.serve(listener).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ```text
//! $ curl -s localhost:8080/api/v1/counter
//! 0
//! $ curl -s -X PUT -d 42 localhost:8080/api/v1/counter
//! {"status":"ok","path":"/counter"}
//! ```

use crate::constants::ErrorCode;
use crate::registry::{Registry, RegistryError};
use serde_json::{Value, json};
use std::sync::Arc;

/// `application/json`, the default representation on both legs.
pub const MEDIA_JSON: &str = "application/json";
/// `application/x-beve`, the BEVE representation.
pub const MEDIA_BEVE: &str = "application/x-beve";
/// `application/problem+json`, RFC 9457 problem details, used for every failure.
pub const MEDIA_PROBLEM: &str = "application/problem+json";

/// The `Allow` set for a path holding a value (or holding nothing yet, which a
/// `PUT` may create).
const ALLOW_VALUE: &str = "GET, HEAD, PUT, OPTIONS";
/// The `Allow` set for a path holding a registered function.
const ALLOW_FUNCTION: &str = "GET, HEAD, POST, OPTIONS";

/// A gateway could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum RestError {
    #[error("REST mount `{mount}` must start with `/`")]
    MountNotAbsolute { mount: String },
    #[error("REST mount `{mount}` must not end with `/`: it is a prefix, and the separator is not part of it")]
    MountTrailingSlash { mount: String },
}

/// Which of the two supported representations a body is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
    Json,
    Beve,
}

impl Repr {
    fn media_type(self) -> &'static str {
        match self {
            Repr::Json => MEDIA_JSON,
            Repr::Beve => MEDIA_BEVE,
        }
    }
}

/// Gateway policy: body limit, cache freshness, validators, and the status an
/// application-level handler error maps to.
#[derive(Debug, Clone)]
pub struct RestConfig {
    /// Reject a request body larger than this with `413`. Defaults to 1 MiB.
    ///
    /// This bounds the *facade*, not the protocol. A REST body is buffered whole
    /// before it can be decoded into a `serde_json::Value`, so an unbounded
    /// limit here is an unbounded allocation driven by an anonymous caller.
    /// Bulk payloads belong on the REPE side, which streams.
    pub max_body_bytes: usize,
    /// `Cache-Control` for successful reads. Defaults to `no-cache`:
    /// revalidate every time, which keeps `ETag` and `304` working while never
    /// serving stale state. Set a `max-age` where the data tolerates it — that
    /// is where the edge-caching win actually comes from.
    pub cache_control: Option<String>,
    /// Emit `ETag` on successful reads and honor `If-None-Match`. Defaults to
    /// on. Turning it off costs every conditional request a full body.
    pub etag: bool,
    /// The status for an [`ErrorCode::ApplicationErrorBase`] handler failure.
    /// Defaults to `500`.
    ///
    /// The gateway cannot know whether a given handler's error means "you sent
    /// the wrong thing" (`400`/`422`) or "something here is broken" (`500`), and
    /// guessing 4xx would tell clients not to retry failures that a retry would
    /// fix. `500` is the honest default; a deployment that knows what its
    /// handlers mean should say so here.
    pub application_error_status: u16,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            cache_control: Some("no-cache".to_string()),
            etag: true,
            application_error_status: 500,
        }
    }
}

/// One inbound HTTP request, reduced to the parts the mapping reads.
///
/// Deliberately borrowed and transport-free: [`RestGateway::respond`] takes this
/// and returns a [`RestResponse`], so the entire mapping is exercisable without
/// a socket, a runtime, or hyper. The HTTP layer in [`RestGateway::serve`] does
/// nothing but fill this in and write the result back out.
#[derive(Debug, Clone, Copy)]
pub struct RestRequest<'a> {
    /// The HTTP method, case-sensitive as RFC 9110 requires.
    pub method: &'a str,
    /// The request target, query string included; it is split off here.
    pub target: &'a str,
    /// `Content-Type`, naming the request body's representation.
    pub content_type: Option<&'a str>,
    /// `Accept`, naming the preferred response representation.
    pub accept: Option<&'a str>,
    /// `If-None-Match`, for conditional reads.
    pub if_none_match: Option<&'a str>,
    /// The raw request body.
    pub body: &'a [u8],
}

impl<'a> RestRequest<'a> {
    /// A request with no headers and no body.
    pub fn new(method: &'a str, target: &'a str) -> Self {
        Self {
            method,
            target,
            content_type: None,
            accept: None,
            if_none_match: None,
            body: &[],
        }
    }

    /// Attach a body and the `Content-Type` describing it.
    pub fn with_body(mut self, content_type: &'a str, body: &'a [u8]) -> Self {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }

    /// Attach an `Accept` header.
    pub fn with_accept(mut self, accept: &'a str) -> Self {
        self.accept = Some(accept);
        self
    }

    /// Attach an `If-None-Match` header.
    pub fn with_if_none_match(mut self, tag: &'a str) -> Self {
        self.if_none_match = Some(tag);
        self
    }
}

/// One outbound HTTP response, reduced to the headers this gateway sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestResponse {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub etag: Option<String>,
    pub cache_control: Option<String>,
    /// `Vary`, set on reads because the response representation depends on
    /// `Accept` and a shared cache must not mix the two.
    pub vary: Option<&'static str>,
    pub allow: Option<&'static str>,
    /// The originating REPE error code, echoed as `X-Repe-Error-Code`.
    pub repe_error_code: Option<u32>,
    /// Set for `HEAD`: `body` holds what `GET` would have returned, and the
    /// carrier must report its length but send none of it.
    pub omit_body: bool,
    pub body: Vec<u8>,
}

impl RestResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            etag: None,
            cache_control: None,
            vary: None,
            allow: None,
            repe_error_code: None,
            omit_body: false,
            body: Vec::new(),
        }
    }

    /// An RFC 9457 problem-details failure.
    fn problem(status: u16, detail: impl Into<String>, repe_code: Option<u32>) -> Self {
        let mut problem = json!({
            "type": "about:blank",
            "title": reason_phrase(status),
            "status": status,
            "detail": detail.into(),
        });
        if let Some(code) = repe_code {
            problem["repe_code"] = json!(code);
        }
        Self {
            content_type: Some(MEDIA_PROBLEM),
            repe_error_code: repe_code,
            // `to_vec` on a Value built from object literals cannot fail: every
            // node is a plain JSON type with no custom Serialize impl to error.
            body: serde_json::to_vec(&problem).unwrap_or_default(),
            ..Self::empty(status)
        }
    }
}

/// An HTTP gateway mapping REST onto a [`Registry`].
///
/// See the [module docs](self) for the verb mapping, the caching rules, and the
/// error model.
#[derive(Clone)]
pub struct RestGateway {
    registry: Arc<Registry>,
    /// Normalized: empty for a root mount, otherwise `/`-prefixed with no
    /// trailing `/`.
    mount: String,
    config: RestConfig,
}

impl std::fmt::Debug for RestGateway {
    /// Hand-written because `Registry` holds `dyn RegistryCallable` handlers and
    /// is not `Debug`. The mount and the policy are what a reader wants anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestGateway")
            .field("mount", &self.mount)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RestGateway {
    /// Mount `registry` under the URL prefix `mount` with default policy.
    ///
    /// `mount` follows the same rule as [`Router::with_registry`]: absolute, no
    /// trailing slash. `""` or `"/"` mounts at the root.
    ///
    /// [`Router::with_registry`]: crate::server::Router::with_registry
    pub fn new(mount: &str, registry: Arc<Registry>) -> Result<Self, RestError> {
        Self::with_config(mount, registry, RestConfig::default())
    }

    /// [`new`](Self::new) with explicit policy.
    pub fn with_config(
        mount: &str,
        registry: Arc<Registry>,
        config: RestConfig,
    ) -> Result<Self, RestError> {
        let normalized = if mount.is_empty() || mount == "/" {
            String::new()
        } else {
            if !mount.starts_with('/') {
                return Err(RestError::MountNotAbsolute {
                    mount: mount.to_string(),
                });
            }
            if mount.ends_with('/') {
                return Err(RestError::MountTrailingSlash {
                    mount: mount.to_string(),
                });
            }
            mount.to_string()
        };
        Ok(Self {
            registry,
            mount: normalized,
            config,
        })
    }

    /// The registry this gateway fronts.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Map one HTTP request onto a registry operation and back, with no
    /// transport involved.
    ///
    /// This is the whole facade. [`serve`](Self::serve) is a hyper shim over it,
    /// and any other HTTP stack can be one too.
    pub fn respond(&self, request: RestRequest<'_>) -> RestResponse {
        let path = request.target.split(['?', '#']).next().unwrap_or("");

        let Some(remainder) = self.strip_mount(path) else {
            return RestResponse::problem(
                404,
                format!("`{path}` is not under this gateway's mount"),
                None,
            );
        };

        let pointer = match path_to_pointer(remainder) {
            Ok(pointer) => pointer,
            Err(detail) => return RestResponse::problem(400, detail, None),
        };

        // Resolved before the verb is dispatched, because the verb is only legal
        // against one kind of target and the caller deserves to be told which.
        let allow = if self.registry.is_function(&pointer) {
            ALLOW_FUNCTION
        } else {
            ALLOW_VALUE
        };

        match request.method {
            "GET" | "HEAD" => self.read(&pointer, request),
            "PUT" => {
                if allow == ALLOW_FUNCTION {
                    return method_not_allowed(
                        allow,
                        "this path is a function; a PUT would call it, which is not idempotent — use POST",
                    );
                }
                self.write_or_call(&pointer, request)
            }
            "POST" => {
                if allow == ALLOW_VALUE {
                    return method_not_allowed(
                        allow,
                        "this path is a value; a POST would write it, which is idempotent — use PUT",
                    );
                }
                self.write_or_call(&pointer, request)
            }
            "OPTIONS" => RestResponse {
                allow: Some(allow),
                ..RestResponse::empty(204)
            },
            _ => method_not_allowed(allow, "unsupported method"),
        }
    }

    /// `GET` / `HEAD`: a registry READ, plus validators and conditional handling.
    fn read(&self, pointer: &str, request: RestRequest<'_>) -> RestResponse {
        let value = match self.registry.dispatch(pointer, None) {
            Ok(value) => value,
            Err(err) => return self.problem_for(err),
        };

        let repr = negotiate(request.accept);
        let (body, media) = match encode(&value, repr) {
            Ok(encoded) => encoded,
            Err(detail) => return RestResponse::problem(500, detail, None),
        };

        let etag = self.config.etag.then(|| etag_for(&body));
        let cache_control = self.config.cache_control.clone();

        // A matching validator ends the request here — the point of the ETag.
        if let (Some(tag), Some(header)) = (etag.as_deref(), request.if_none_match)
            && if_none_match_matches(header, tag)
        {
            return RestResponse {
                etag: etag.clone(),
                cache_control,
                vary: Some("Accept"),
                ..RestResponse::empty(304)
            };
        }

        RestResponse {
            status: 200,
            content_type: Some(media),
            etag,
            cache_control,
            vary: Some("Accept"),
            allow: None,
            repe_error_code: None,
            omit_body: request.method == "HEAD",
            body,
        }
    }

    /// `PUT` / `POST`: a registry WRITE or CALL. The verb was already checked
    /// against the target kind by [`respond`](Self::respond).
    fn write_or_call(&self, pointer: &str, request: RestRequest<'_>) -> RestResponse {
        // An empty body is how the registry spells READ. Letting one through
        // here would turn a write into a silent read that answers `200` with the
        // old value, so it is refused rather than reinterpreted.
        if request.body.is_empty() {
            return RestResponse::problem(
                400,
                "a body is required; send `null` for a call that takes no arguments",
                Some(ErrorCode::InvalidBody as u32),
            );
        }

        let repr = match body_repr(request.content_type) {
            Ok(repr) => repr,
            Err(detail) => {
                return RestResponse::problem(415, detail, Some(ErrorCode::InvalidBody as u32));
            }
        };

        let payload = match decode(request.body, repr) {
            Ok(payload) => payload,
            Err(detail) => {
                return RestResponse::problem(400, detail, Some(ErrorCode::InvalidBody as u32));
            }
        };

        let value = match self.registry.dispatch(pointer, Some(payload)) {
            Ok(value) => value,
            Err(err) => return self.problem_for(err),
        };

        let (body, media) = match encode(&value, negotiate(request.accept)) {
            Ok(encoded) => encoded,
            Err(detail) => return RestResponse::problem(500, detail, None),
        };

        RestResponse {
            status: 200,
            content_type: Some(media),
            // No validator and no freshness on a mutation: `ETag` is a read
            // concern, and a cached write response is never useful.
            body,
            ..RestResponse::empty(200)
        }
    }

    /// Strip the mount prefix, or `None` if the path is not under it.
    fn strip_mount<'p>(&self, path: &'p str) -> Option<&'p str> {
        if self.mount.is_empty() {
            return Some(path);
        }
        let rest = path.strip_prefix(self.mount.as_str())?;
        // `/api/v1x` must not match the mount `/api/v1`: the prefix has to end
        // on a segment boundary or at the end of the path.
        if rest.is_empty() || rest.starts_with('/') {
            Some(rest)
        } else {
            None
        }
    }

    fn problem_for(&self, err: RegistryError) -> RestResponse {
        let code = err.code();
        RestResponse::problem(
            self.status_for(code),
            err.to_string(),
            Some(code as u32),
        )
    }

    fn status_for(&self, code: ErrorCode) -> u16 {
        match code {
            ErrorCode::Ok => 200,
            ErrorCode::MethodNotFound => 404,
            ErrorCode::Timeout => 504,
            ErrorCode::ResourceExhausted => 503,
            ErrorCode::InternalError => 500,
            ErrorCode::ApplicationErrorBase => self.config.application_error_status,
            // Everything else the registry can raise is the caller's fault: a
            // malformed body, an unusable pointer, a body format this gateway
            // does not accept.
            _ => 400,
        }
    }
}

fn method_not_allowed(allow: &'static str, detail: &str) -> RestResponse {
    RestResponse {
        allow: Some(allow),
        ..RestResponse::problem(405, detail, None)
    }
}

/// Map a URL path (already stripped of the mount) onto a JSON Pointer.
///
/// Each segment is percent-decoded *and then* JSON-Pointer-escaped, in that
/// order and per segment. The order is the security-relevant part: decoding the
/// whole path first would let `%2F` decode into a `/` that then reads as a
/// segment separator, so `/items/a%2Fb` would address `b` inside `a` instead of
/// the single key `a/b`. Escaping after decoding turns it into the `~1` the
/// pointer spec asks for, and the injected separator cannot occur.
fn path_to_pointer(path: &str) -> Result<String, &'static str> {
    if path.is_empty() || path == "/" {
        return Ok(String::new());
    }
    // `/counter/` and `/counter` are the same resource. Without this, the
    // trailing slash would address the empty-string key beneath `counter`,
    // which is a legal pointer and never what the caller meant.
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return Ok(String::new());
    }
    if !path.starts_with('/') {
        return Err("request path must be absolute");
    }

    let mut pointer = String::with_capacity(path.len());
    for segment in path.split('/').skip(1) {
        pointer.push('/');
        pointer.push_str(&escape_pointer_token(&percent_decode(segment)?));
    }
    Ok(pointer)
}

fn percent_decode(segment: &str) -> Result<String, &'static str> {
    if !segment.contains('%') {
        return Ok(segment.to_string());
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("truncated percent-escape in request path");
            }
            let (hi, lo) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]));
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err("invalid percent-escape in request path");
            };
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "percent-escape decoded to invalid UTF-8")
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// RFC 6901 escaping. `~` first: doing `/` first would turn a literal `/` into
/// `~1` and the following pass would then escape that `~` into `~01`.
fn escape_pointer_token(token: &str) -> String {
    if !token.contains('~') && !token.contains('/') {
        return token.to_string();
    }
    token.replace('~', "~0").replace('/', "~1")
}

/// Pick a response representation from `Accept`.
///
/// A pragmatic subset of RFC 9110 content negotiation: media types and `q`
/// values, no other parameters, no wildcard specificity ranking. That is enough
/// to separate two representations, which is all this gateway offers; anything
/// unrecognized falls back to JSON rather than answering `406`, because a client
/// that got JSON when it asked for something exotic is better served than one
/// that got nothing.
fn negotiate(accept: Option<&str>) -> Repr {
    let Some(accept) = accept else {
        return Repr::Json;
    };
    let mut best: Option<(u32, Repr)> = None;
    for entry in accept.split(',') {
        let mut parts = entry.split(';');
        let media = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let mut quality = 1000;
        for param in parts {
            let param = param.trim();
            if let Some(value) = param
                .strip_prefix("q=")
                .or_else(|| param.strip_prefix("Q="))
            {
                quality = parse_quality(value);
            }
        }
        let repr = match media.as_str() {
            MEDIA_BEVE | "application/beve" => Repr::Beve,
            MEDIA_JSON | "application/*" | "*/*" => Repr::Json,
            _ => continue,
        };
        // `q=0` is an explicit refusal, not a weak preference.
        if quality == 0 {
            continue;
        }
        // Strictly greater, so equal weights keep the earlier entry — the
        // conventional tie-break, and the one that makes header order meaningful.
        if best.is_none_or(|(best_quality, _)| quality > best_quality) {
            best = Some((quality, repr));
        }
    }
    best.map_or(Repr::Json, |(_, repr)| repr)
}

/// Parse a `q` value into thousandths, clamped to `0..=1000`. An unparseable
/// weight is treated as the `1` default rather than as a refusal.
fn parse_quality(value: &str) -> u32 {
    value
        .trim()
        .parse::<f32>()
        .map_or(1000, |q| (q.clamp(0.0, 1.0) * 1000.0).round() as u32)
}

/// Pick a request-body representation from `Content-Type`. A missing header is
/// JSON, matching what a `curl -d` sends.
fn body_repr(content_type: Option<&str>) -> Result<Repr, &'static str> {
    let Some(content_type) = content_type else {
        return Ok(Repr::Json);
    };
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match media.as_str() {
        "" | MEDIA_JSON | "text/json" => Ok(Repr::Json),
        MEDIA_BEVE | "application/beve" => Ok(Repr::Beve),
        // `curl -d` defaults to this and means JSON often enough that rejecting
        // it would be pedantry at the cost of the primary use case.
        "application/x-www-form-urlencoded" => Ok(Repr::Json),
        _ => Err("unsupported Content-Type; send application/json or application/x-beve"),
    }
}

fn encode(value: &Value, repr: Repr) -> Result<(Vec<u8>, &'static str), &'static str> {
    let bytes = match repr {
        Repr::Json => serde_json::to_vec(value).map_err(|_| "response is not encodable as JSON")?,
        Repr::Beve => beve::to_vec(value).map_err(|_| "response is not encodable as BEVE")?,
    };
    Ok((bytes, repr.media_type()))
}

fn decode(body: &[u8], repr: Repr) -> Result<Value, &'static str> {
    match repr {
        Repr::Json => serde_json::from_slice(body).map_err(|_| "request body is not valid JSON"),
        Repr::Beve => beve::from_slice(body).map_err(|_| "request body is not valid BEVE"),
    }
}

/// A strong `ETag` over the exact bytes sent, as FNV-1a/64.
///
/// FNV rather than [`std::hash::DefaultHasher`] because the tag has to agree
/// across processes: two instances of this gateway behind one load balancer must
/// hash a body identically or a shared cache will thrash. `DefaultHasher`'s
/// output is explicitly not stable across Rust releases, so two instances built
/// with different toolchains would disagree. FNV-1a is fully specified and
/// stable forever. It is not, and does not need to be, cryptographic: an `ETag`
/// is a change detector, not an integrity check.
fn etag_for(body: &[u8]) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in body {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("\"{hash:016x}\"")
}

fn if_none_match_matches(header: &str, etag: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        // `*` matches any current representation, so on a successful read it
        // always matches.
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------
//
// Everything above is transport-free. This part does nothing but fill in a
// `RestRequest` from a hyper request and write a `RestResponse` back out, which
// is why the mapping's tests need no socket and this shim needs almost none.

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use std::convert::Infallible;

impl RestGateway {
    /// Serve this gateway on `listener` until the accept loop fails
    /// unrecoverably.
    ///
    /// Both HTTP/1.1 and HTTP/2 are served on the one listener via hyper's
    /// protocol-detecting builder: HTTP/2 for a client that arrives with prior
    /// knowledge (or via ALPN behind a TLS terminator), HTTP/1.1 for curl and
    /// for the origin leg behind a CDN. Nothing in the mapping depends on which
    /// one a request arrived over.
    ///
    /// TLS is deliberately out of scope. A gateway of this shape belongs behind
    /// a terminator (a CDN, an ingress, a sidecar) that is already doing
    /// certificate rotation and ALPN, and building a second, worse copy of that
    /// into the crate would be the wrong place for it.
    ///
    /// Each connection is served on its own task, so a slow client blocks only
    /// itself.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let gateway = Arc::new(self);
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _peer)) => stream,
                // A failed accept is usually about *that* connection (the peer
                // vanished mid-handshake) or transient saturation, neither of
                // which is a reason to take the listener down. Returning here
                // would let one EMFILE or one aborted connection end the
                // gateway for everyone.
                Err(err) if is_transient_accept_error(&err) => continue,
                Err(err) => return Err(err),
            };

            let gateway = Arc::clone(&gateway);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let gateway = Arc::clone(&gateway);
                    async move { Ok::<_, Infallible>(gateway.handle(request).await) }
                });
                // The connection's own errors are its own: a client that hangs
                // up mid-response is routine, and there is no caller left to
                // report it to.
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }

    /// One hyper request in, one hyper response out.
    async fn handle(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let (parts, body) = request.into_parts();

        let method = parts.method.as_str().to_owned();
        let target = parts
            .uri
            .path_and_query()
            .map_or("/", |path_and_query| path_and_query.as_str())
            .to_owned();
        let header = |name: hyper::header::HeaderName| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let content_type = header(hyper::header::CONTENT_TYPE);
        let accept = header(hyper::header::ACCEPT);
        let if_none_match = header(hyper::header::IF_NONE_MATCH);

        // `Limited` caps the body before it is buffered, so an oversized request
        // is refused rather than allocated. Checking `Content-Length` instead
        // would miss a chunked body that declares nothing.
        let body = match Limited::new(body, self.config.max_body_bytes)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                let response = if err.downcast_ref::<LengthLimitError>().is_some() {
                    RestResponse::problem(
                        413,
                        format!(
                            "request body exceeds the {}-byte limit",
                            self.config.max_body_bytes
                        ),
                        None,
                    )
                } else {
                    RestResponse::problem(400, "request body could not be read", None)
                };
                return to_hyper(response);
            }
        };

        to_hyper(self.respond(RestRequest {
            method: &method,
            target: &target,
            content_type: content_type.as_deref(),
            accept: accept.as_deref(),
            if_none_match: if_none_match.as_deref(),
            body: &body,
        }))
    }
}

fn is_transient_accept_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::OutOfMemory
    )
}

fn to_hyper(response: RestResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(response.status);
    if let Some(content_type) = response.content_type {
        builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
    }
    if let Some(etag) = &response.etag {
        builder = builder.header(hyper::header::ETAG, etag);
    }
    if let Some(cache_control) = &response.cache_control {
        builder = builder.header(hyper::header::CACHE_CONTROL, cache_control);
    }
    if let Some(vary) = response.vary {
        builder = builder.header(hyper::header::VARY, vary);
    }
    if let Some(allow) = response.allow {
        builder = builder.header(hyper::header::ALLOW, allow);
    }
    if let Some(code) = response.repe_error_code {
        builder = builder.header("x-repe-error-code", code);
    }

    // A `HEAD` reports the length `GET` would have sent while sending none of
    // it, which RFC 9110 asks for and which a client sizing a download relies on.
    let built = if response.omit_body {
        builder
            .header(hyper::header::CONTENT_LENGTH, response.body.len())
            .body(Full::new(Bytes::new()))
    } else {
        builder.body(Full::new(Bytes::from(response.body)))
    };

    built.unwrap_or_else(|_| {
        // Only reachable if a header value built above is not a legal header
        // value. Every one of them is either a static string or hex/decimal
        // digits, so this is unreachable in practice — but a panic in a
        // connection task is not the way to find out otherwise.
        Response::builder()
            .status(500)
            .body(Full::new(Bytes::new()))
            .expect("a status-only response is always buildable")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway() -> RestGateway {
        gateway_with(RestConfig::default())
    }

    fn gateway_with(config: RestConfig) -> RestGateway {
        let registry = Arc::new(Registry::new());
        registry.register_value("/counter", json!(7)).unwrap();
        registry
            .register_value("/config", json!({ "verbose": false }))
            .unwrap();
        // A key with a literal `/` in it, reachable only through `~1`. This is
        // what the percent-decode-then-escape order exists to keep addressable.
        registry.register_value("/items/a~1b", json!("slashed")).unwrap();
        registry.register_value("/tilde/a~0b", json!("tilded")).unwrap();
        registry
            .register_function("/add", |params| {
                let Some(Value::Object(map)) = params else {
                    return Err((ErrorCode::InvalidBody, "expected an object".into()));
                };
                let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
                let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
                Ok(json!({ "result": a + b }))
            })
            .unwrap();
        registry
            .register_function("/boom", |_| {
                Err((ErrorCode::ApplicationErrorBase, "handler said no".into()))
            })
            .unwrap();
        RestGateway::with_config("/api/v1", registry, config).unwrap()
    }

    fn json_body(response: &RestResponse) -> Value {
        serde_json::from_slice(&response.body).expect("response body is JSON")
    }

    // -- the verb mapping ---------------------------------------------------

    #[test]
    fn get_reads_a_value() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/counter"));
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, Some(MEDIA_JSON));
        assert_eq!(json_body(&response), json!(7));
    }

    #[test]
    fn get_at_the_mount_itself_reads_the_whole_tree() {
        // The mount maps to the empty pointer, i.e. the registry root, so the
        // tree is browsable from one URL rather than only leaf by leaf.
        for target in ["/api/v1", "/api/v1/"] {
            let response = gateway().respond(RestRequest::new("GET", target));
            assert_eq!(response.status, 200, "{target}");
            assert_eq!(json_body(&response)["counter"], json!(7), "{target}");
        }
    }

    #[test]
    fn put_writes_a_value() {
        let gateway = gateway();
        let response = gateway.respond(
            RestRequest::new("PUT", "/api/v1/counter").with_body(MEDIA_JSON, b"42"),
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            gateway.registry().read_value("/counter").unwrap(),
            json!(42)
        );
    }

    #[test]
    fn post_calls_a_function() {
        let response = gateway().respond(
            RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_JSON, br#"{"a":2,"b":3}"#),
        );
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!({ "result": 5 }));
    }

    #[test]
    fn put_at_a_function_is_refused_rather_than_silently_calling_it() {
        // The guarantee being protected: a caller retried a PUT because PUT is
        // idempotent. Routing it into a function call would run the side effect
        // twice, so the verb mismatch has to be an error, not a coercion.
        let response = gateway()
            .respond(RestRequest::new("PUT", "/api/v1/add").with_body(MEDIA_JSON, b"{}"));
        assert_eq!(response.status, 405);
        assert_eq!(response.allow, Some(ALLOW_FUNCTION));
        assert_eq!(response.content_type, Some(MEDIA_PROBLEM));
    }

    #[test]
    fn post_at_a_value_is_refused_rather_than_silently_writing_it() {
        // The mirror image: a value is idempotently writable, and answering a
        // POST would tell callers otherwise — which is how an API ends up with
        // POST on every route and REST's guarantees on none.
        let gateway = gateway();
        let response = gateway
            .respond(RestRequest::new("POST", "/api/v1/counter").with_body(MEDIA_JSON, b"1"));
        assert_eq!(response.status, 405);
        assert_eq!(response.allow, Some(ALLOW_VALUE));
        assert_eq!(
            gateway.registry().read_value("/counter").unwrap(),
            json!(7),
            "the refused POST must not have written"
        );
    }

    #[test]
    fn a_write_with_no_body_is_refused_rather_than_read() {
        // An empty body is how the registry spells READ, so letting one through
        // would answer a PUT with `200` and the *old* value, having written
        // nothing. That is the single most confusing outcome available here.
        // Each against the target kind its verb is legal for, so the empty-body
        // check is what answers rather than the verb guard.
        for (method, target) in [("PUT", "/api/v1/counter"), ("POST", "/api/v1/add")] {
            let response = gateway().respond(RestRequest::new(method, target));
            assert_eq!(response.status, 400, "{method} {target}");
            assert_eq!(
                response.repe_error_code,
                Some(ErrorCode::InvalidBody as u32),
                "{method} {target}"
            );
        }
    }

    #[test]
    fn a_no_argument_call_is_spelled_with_a_null_body() {
        let registry = Arc::new(Registry::new());
        registry
            .register_function("/ping", |_| Ok(json!("pong")))
            .unwrap();
        let gateway = RestGateway::new("/api/v1", registry).unwrap();
        let response = gateway
            .respond(RestRequest::new("POST", "/api/v1/ping").with_body(MEDIA_JSON, b"null"));
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!("pong"));
    }

    #[test]
    fn options_advertises_the_verbs_the_target_actually_supports() {
        let gateway = gateway();
        let value = gateway.respond(RestRequest::new("OPTIONS", "/api/v1/counter"));
        assert_eq!(value.status, 204);
        assert_eq!(value.allow, Some(ALLOW_VALUE));

        let function = gateway.respond(RestRequest::new("OPTIONS", "/api/v1/add"));
        assert_eq!(function.status, 204);
        assert_eq!(function.allow, Some(ALLOW_FUNCTION));
    }

    #[test]
    fn an_unsupported_method_is_405_with_allow() {
        let response = gateway().respond(RestRequest::new("DELETE", "/api/v1/counter"));
        assert_eq!(response.status, 405);
        assert_eq!(response.allow, Some(ALLOW_VALUE));
    }

    #[test]
    fn head_reports_the_length_without_the_body() {
        let gateway = gateway();
        let get = gateway.respond(RestRequest::new("GET", "/api/v1/config"));
        let head = gateway.respond(RestRequest::new("HEAD", "/api/v1/config"));
        assert!(head.omit_body);
        assert!(!get.omit_body);
        assert_eq!(head.body, get.body, "HEAD carries what GET would have sent");
        assert_eq!(head.etag, get.etag);
    }

    // -- caching ------------------------------------------------------------

    #[test]
    fn a_read_carries_a_validator_and_a_freshness_directive() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/counter"));
        assert!(response.etag.is_some());
        assert_eq!(response.cache_control.as_deref(), Some("no-cache"));
        assert_eq!(response.vary, Some("Accept"));
    }

    #[test]
    fn a_write_carries_neither() {
        let response = gateway()
            .respond(RestRequest::new("PUT", "/api/v1/counter").with_body(MEDIA_JSON, b"1"));
        assert_eq!(response.etag, None);
        assert_eq!(response.cache_control, None);
    }

    #[test]
    fn a_matching_validator_answers_304_with_no_body() {
        let gateway = gateway();
        let first = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        let tag = first.etag.clone().unwrap();

        let second =
            gateway.respond(RestRequest::new("GET", "/api/v1/counter").with_if_none_match(&tag));
        assert_eq!(second.status, 304);
        assert!(second.body.is_empty());
        assert_eq!(second.etag, first.etag, "304 still carries the validator");
    }

    #[test]
    fn a_stale_validator_answers_the_full_body() {
        let gateway = gateway();
        let response = gateway.respond(
            RestRequest::new("GET", "/api/v1/counter").with_if_none_match("\"0000000000000000\""),
        );
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!(7));
    }

    #[test]
    fn the_validator_changes_when_the_value_does() {
        let gateway = gateway();
        let before = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        gateway.respond(RestRequest::new("PUT", "/api/v1/counter").with_body(MEDIA_JSON, b"8"));
        let after = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        assert_ne!(before.etag, after.etag);

        // And the stale validator no longer short-circuits.
        let conditional = gateway.respond(
            RestRequest::new("GET", "/api/v1/counter")
                .with_if_none_match(&before.etag.unwrap()),
        );
        assert_eq!(conditional.status, 200);
    }

    #[test]
    fn a_weak_validator_and_a_star_both_match() {
        let gateway = gateway();
        let tag = gateway
            .respond(RestRequest::new("GET", "/api/v1/counter"))
            .etag
            .unwrap();

        for header in [format!("W/{tag}"), "*".to_string(), format!("\"x\", {tag}")] {
            let response = gateway
                .respond(RestRequest::new("GET", "/api/v1/counter").with_if_none_match(&header));
            assert_eq!(response.status, 304, "{header}");
        }
    }

    #[test]
    fn the_two_representations_get_different_validators() {
        // Why `Vary: Accept` is not optional: a shared cache keyed on URL alone
        // would hand the BEVE bytes to a JSON client under a tag that matched.
        let gateway = gateway();
        let as_json = gateway.respond(RestRequest::new("GET", "/api/v1/config"));
        let as_beve = gateway
            .respond(RestRequest::new("GET", "/api/v1/config").with_accept(MEDIA_BEVE));
        assert_ne!(as_json.etag, as_beve.etag);
        assert_eq!(as_json.vary, Some("Accept"));
        assert_eq!(as_beve.vary, Some("Accept"));
    }

    #[test]
    fn validators_can_be_turned_off() {
        let gateway = gateway_with(RestConfig {
            etag: false,
            ..RestConfig::default()
        });
        let response = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        assert_eq!(response.etag, None);
        // And with no tag to compare against, a conditional request is answered
        // in full rather than short-circuited on a tag we did not issue.
        let conditional = gateway
            .respond(RestRequest::new("GET", "/api/v1/counter").with_if_none_match("*"));
        assert_eq!(conditional.status, 200);
    }

    // -- content negotiation ------------------------------------------------

    #[test]
    fn beve_is_served_on_request_and_decodes_to_the_same_value() {
        let response =
            gateway().respond(RestRequest::new("GET", "/api/v1/config").with_accept(MEDIA_BEVE));
        assert_eq!(response.content_type, Some(MEDIA_BEVE));
        let decoded: Value = beve::from_slice(&response.body).unwrap();
        assert_eq!(decoded, json!({ "verbose": false }));
    }

    #[test]
    fn a_beve_request_body_is_accepted() {
        let gateway = gateway();
        let body = beve::to_vec(&json!({ "a": 20, "b": 22 })).unwrap();
        let response = gateway
            .respond(RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_BEVE, &body));
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!({ "result": 42 }));
    }

    #[test]
    fn quality_values_are_honored() {
        let gateway = gateway();
        let cases = [
            ("application/json", MEDIA_JSON),
            ("application/x-beve", MEDIA_BEVE),
            ("*/*", MEDIA_JSON),
            ("application/json;q=0.1, application/x-beve;q=0.9", MEDIA_BEVE),
            ("application/json;q=0.9, application/x-beve;q=0.1", MEDIA_JSON),
            // An explicit refusal of BEVE, even listed first.
            ("application/x-beve;q=0, application/json", MEDIA_JSON),
            // A tie keeps the earlier entry, so header order stays meaningful.
            ("application/x-beve, application/json", MEDIA_BEVE),
            // Nothing recognized: JSON rather than a 406, since a client that
            // gets JSON is better served than one that gets nothing.
            ("text/plain", MEDIA_JSON),
        ];
        for (accept, expected) in cases {
            let response =
                gateway.respond(RestRequest::new("GET", "/api/v1/counter").with_accept(accept));
            assert_eq!(response.content_type, Some(expected), "Accept: {accept}");
        }
    }

    #[test]
    fn an_unsupported_content_type_is_415() {
        let response = gateway().respond(
            RestRequest::new("PUT", "/api/v1/counter").with_body("application/xml", b"<x/>"),
        );
        assert_eq!(response.status, 415);
    }

    #[test]
    fn a_missing_content_type_is_treated_as_json() {
        // What `curl -d` sends, and the single most common way this gateway will
        // be driven by hand.
        let mut request = RestRequest::new("PUT", "/api/v1/counter");
        request.body = b"99";
        let gateway = gateway();
        assert_eq!(gateway.respond(request).status, 200);
        assert_eq!(
            gateway.registry().read_value("/counter").unwrap(),
            json!(99)
        );
    }

    #[test]
    fn a_malformed_body_is_400_not_500() {
        let response = gateway()
            .respond(RestRequest::new("PUT", "/api/v1/counter").with_body(MEDIA_JSON, b"{oops"));
        assert_eq!(response.status, 400);
    }

    // -- paths --------------------------------------------------------------

    #[test]
    fn a_path_outside_the_mount_is_404() {
        for target in ["/counter", "/other/counter", "/"] {
            assert_eq!(
                gateway().respond(RestRequest::new("GET", target)).status,
                404,
                "{target}"
            );
        }
    }

    #[test]
    fn the_mount_must_match_on_a_segment_boundary() {
        // `/api/v1x` shares a textual prefix with `/api/v1` but is a different
        // path; a plain `strip_prefix` would have served it.
        assert_eq!(
            gateway()
                .respond(RestRequest::new("GET", "/api/v1x/counter"))
                .status,
            404
        );
    }

    #[test]
    fn a_percent_encoded_separator_stays_inside_one_segment() {
        // The injection this guards: decoding the whole path first would turn
        // `%2F` into a separator, addressing `b` inside `a` instead of the one
        // key `a/b`.
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/items/a%2Fb"));
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!("slashed"));
    }

    #[test]
    fn a_literal_tilde_in_a_segment_is_escaped_not_interpreted() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/tilde/a~b"));
        assert_eq!(response.status, 200);
        assert_eq!(json_body(&response), json!("tilded"));
    }

    #[test]
    fn a_trailing_slash_is_the_same_resource() {
        let gateway = gateway();
        assert_eq!(
            gateway.respond(RestRequest::new("GET", "/api/v1/counter/")).body,
            gateway.respond(RestRequest::new("GET", "/api/v1/counter")).body
        );
    }

    #[test]
    fn the_query_string_and_fragment_are_not_part_of_the_resource() {
        let gateway = gateway();
        let plain = gateway.respond(RestRequest::new("GET", "/api/v1/counter"));
        for target in ["/api/v1/counter?pretty=1", "/api/v1/counter#x"] {
            assert_eq!(
                gateway.respond(RestRequest::new("GET", target)).body,
                plain.body,
                "{target}"
            );
        }
    }

    #[test]
    fn a_malformed_percent_escape_is_400() {
        for target in ["/api/v1/a%zz", "/api/v1/a%2"] {
            assert_eq!(
                gateway().respond(RestRequest::new("GET", target)).status,
                400,
                "{target}"
            );
        }
    }

    #[test]
    fn a_root_mount_serves_every_path() {
        let registry = Arc::new(Registry::new());
        registry.register_value("/counter", json!(1)).unwrap();
        for mount in ["", "/"] {
            let gateway = RestGateway::new(mount, Arc::clone(&registry)).unwrap();
            assert_eq!(
                gateway.respond(RestRequest::new("GET", "/counter")).status,
                200,
                "mount {mount:?}"
            );
        }
    }

    #[test]
    fn a_mount_with_a_trailing_slash_is_rejected_at_construction() {
        let registry = Arc::new(Registry::new());
        assert!(matches!(
            RestGateway::new("/api/v1/", Arc::clone(&registry)),
            Err(RestError::MountTrailingSlash { .. })
        ));
        assert!(matches!(
            RestGateway::new("api/v1", registry),
            Err(RestError::MountNotAbsolute { .. })
        ));
    }

    // -- errors -------------------------------------------------------------

    #[test]
    fn a_missing_path_is_404_as_problem_details_carrying_the_repe_code() {
        let response = gateway().respond(RestRequest::new("GET", "/api/v1/absent"));
        assert_eq!(response.status, 404);
        assert_eq!(response.content_type, Some(MEDIA_PROBLEM));
        assert_eq!(
            response.repe_error_code,
            Some(ErrorCode::MethodNotFound as u32)
        );
        let problem = json_body(&response);
        assert_eq!(problem["status"], json!(404));
        assert_eq!(problem["title"], json!("Not Found"));
        assert_eq!(problem["repe_code"], json!(ErrorCode::MethodNotFound as u32));
    }

    #[test]
    fn a_handler_error_defaults_to_500_and_is_configurable() {
        let default = gateway()
            .respond(RestRequest::new("POST", "/api/v1/boom").with_body(MEDIA_JSON, b"null"));
        assert_eq!(default.status, 500);
        assert_eq!(
            default.repe_error_code,
            Some(ErrorCode::ApplicationErrorBase as u32)
        );
        assert_eq!(json_body(&default)["detail"], json!("handler said no"));

        let remapped = gateway_with(RestConfig {
            application_error_status: 422,
            ..RestConfig::default()
        })
        .respond(RestRequest::new("POST", "/api/v1/boom").with_body(MEDIA_JSON, b"null"));
        assert_eq!(remapped.status, 422);
    }

    #[test]
    fn a_handler_rejecting_its_arguments_is_400() {
        // `InvalidBody` from inside a handler is still the caller's fault, so it
        // must not surface as a 500 that tells the client to retry.
        let response = gateway()
            .respond(RestRequest::new("POST", "/api/v1/add").with_body(MEDIA_JSON, b"[1,2]"));
        assert_eq!(response.status, 400);
    }

    // -- units --------------------------------------------------------------

    #[test]
    fn path_to_pointer_cases() {
        let cases = [
            ("", ""),
            ("/", ""),
            ("/counter", "/counter"),
            ("/counter/", "/counter"),
            ("/a/b/c", "/a/b/c"),
            ("/a%2Fb", "/a~1b"),
            ("/a~b", "/a~0b"),
            ("/a%7Eb", "/a~0b"),
            ("/sp%20ace", "/sp ace"),
            // An empty middle segment is a legal pointer to an empty key, and
            // is left alone rather than collapsed.
            ("/a//b", "/a//b"),
        ];
        for (path, expected) in cases {
            assert_eq!(path_to_pointer(path).unwrap(), expected, "{path}");
        }
    }

    #[test]
    fn etag_is_stable_and_content_addressed() {
        assert_eq!(etag_for(b"abc"), etag_for(b"abc"));
        assert_ne!(etag_for(b"abc"), etag_for(b"abd"));
        // Pinned so a refactor cannot silently change the function: two gateway
        // instances that disagree would make a shared cache thrash rather than
        // fail, which is the kind of bug nobody notices.
        assert_eq!(etag_for(b""), "\"cbf29ce484222325\"");
        assert_eq!(etag_for(b"repe"), "\"4cac4e1fbfac553f\"");
    }
}
